use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::{bail, ensure, Result, WrapErr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, instrument, warn};

// Ideally when a client wants to connect with the serivce,
// through the hub, it connects to a port that the hub listens
// to, and establishes a TCP(?) connection, the hub then proceeds
// to establish a TCP connection to the spoke.
//
// But you can't, and that's the whole point of this program!
// The spoke is behind a NAT, and you can't actively connect
// from the outside(*). As the hub, how can you inform that the
// spoke we want to connect???
//
// Of course the device behind NAT can connect to the hub, we could
// spare another port to open up a side-channel from the spoke to the
// hub for coordination purposes, but wouldn't it waste another port?
//
// Another option is that the spoke always tries to connect to the hub,
// whenever possible, and when the connection ends, probably because the
// service sides sends an EOF, terminating the chain of connections, then
// the hub and spoke can actively establish the chain together. But it
// won't work, since effectively we're stuck with one connection at a time.
//
// So we're back with a side-channel really, but no we don't need extra
// port to do that, we can multiplex on that hub coordination port. We'll
// need a few handshakes to know whether it's a side-channel or a proxy
// tunnel.
//
// Too many failure points! How can I possibly consider all of them?! Even
// it's just this dead simple program?!

fn install_eyre() -> Result<()> {
    color_eyre::install()?;
    Ok(())
}

fn install_tracing() -> Result<()> {
    use tracing_subscriber::{prelude::*, EnvFilter, Registry};
    use tracing_tree::HierarchicalLayer;

    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::default().add_directive("hub=trace".parse()?),
    };

    Registry::default()
        .with(env_filter)
        .with(
            HierarchicalLayer::new(2)
                .with_bracketed_fields(true)
                .with_targets(true),
        )
        .init();

    Ok(())
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Message {
    /// Used by client to indicate it's a coordination connection
    Coord = 1,

    /// Used by client to indicate it's a tunnel connection
    Tunnel = 2,

    /// Used by server to acknowledge a message sent by the client
    Ack = 3,

    /// Used by server to tell the spoke to establish a tunnel connection
    Accept = 4,
}

impl From<u8> for Message {
    fn from(message: u8) -> Self {
        use Message::*;
        match message {
            1 => Coord,
            2 => Tunnel,
            3 => Ack,
            4 => Accept,
            _ => unreachable!(),
        }
    }
}

impl From<Message> for u8 {
    fn from(message: Message) -> Self {
        use Message::*;
        match message {
            Coord => 1,
            Tunnel => 2,
            Ack => 3,
            Accept => 4,
        }
    }
}

async fn run_server() -> Result<()> {
    info!("✨ running the hub at 0.0.0.0:1337!");

    let mut coord = {
        let service_listener = TcpListener::bind(("0.0.0.0", 4242)).await?;

        info!("waiting for spoke come up at 0.0.0.0:4242");
        let (stream, addr) = service_listener.accept().await?;
        info!(?addr, "spoke connected!");

        // note: listener is dropped here to prevent mutiple spokes connected in
        stream
    };
    // todo: what happens the coordination conn breaks afterwards? reconnect.

    // really should use `tokio_util::codec`, but hey :), can't we be naive?
    // expect spoke to declare `coord` is a coordnation connection
    let message: Message = coord.read_u8().await?.into();
    ensure!(
        message == Message::Coord,
        "wanted to establish a coordination connection with spoke, but spoke refused"
    );
    coord.write_u8(Message::Ack.into()).await?;

    // coordinator will be shared between threads, to notify the spoke to open up a tunnel
    // todo: it's sad to use tokio's mutex
    let coord = Arc::new(tokio::sync::Mutex::new(coord));

    let listener = TcpListener::bind(("0.0.0.0", 1337)).await?;

    info!("anyone who want to access the service can go to 0.0.0.0:1337");
    loop {
        let (socket, peer_addr) = listener.accept().await?;
        info!(?peer_addr, "connected");

        let coord = coord.clone();

        tokio::spawn(async move {
            // todo: one connection failure shouldn't bring down the whole system
            server_handle_connection(coord, socket, peer_addr)
                .await
                .unwrap();
        });
    }
}

#[instrument(skip(coord, client))]
async fn server_handle_connection(
    coord: Arc<tokio::sync::Mutex<TcpStream>>,
    mut client: TcpStream,
    addr: SocketAddr,
) -> Result<()> {
    info!("asking the spoke to open up a tunnel");

    let (mut tunnel, tunnel_addr) = {
        // todo: we're using the coordinator lock to prevent mutiple threads
        // from listening on the same port, what are we doing?
        let mut coord = coord.lock().await;
        let listener = TcpListener::bind(("0.0.0.0", 4242)).await?;
        coord.write_u8(Message::Accept.into()).await?;
        listener.accept().await?
    };
    let message: Message = tunnel.read_u8().await?.into();
    ensure!(
        message == Message::Tunnel,
        "expected spoke to connect to establish a tunnel"
    );
    tunnel.write_u8(Message::Ack.into()).await?;
    info!(?tunnel_addr, "tunnel established");

    tokio::io::copy_bidirectional(&mut client, &mut tunnel).await?;

    info!("closed");
    Ok(())
}

#[instrument]
async fn client_accept() -> Result<()> {
    info!("since the hub asked, we are opening a tunnel to hub");

    // open up connection from spoke to the hub
    info!("connecting to the hub 127.0.0.1:4242");
    let mut tunnel = loop {
        match TcpStream::connect(("127.0.0.1", 4242)).await {
            Ok(conn) => break conn,
            Err(err) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                warn!(?err, "failed to connect to the hub 127.0.0.1:4242");
            }
        }
    };
    info!("connected to the hub");
    info!("trying to establish a tunnel connection");

    tunnel
        .write_u8(Message::Tunnel.into())
        .await
        .wrap_err("failed to establish coordination connection with the hub")?;

    info!("hub please open up a tunnel");

    let message: Message = tunnel.read_u8().await?.into();
    ensure!(
        message == Message::Ack,
        "the hub should have acked our request to establish a coordination connection, but it won't, who's to blame?"
    );

    info!("tunnel established");

    info!("connecting to local serivce");

    // use some retry backoffs?
    let mut service = loop {
        match TcpStream::connect(("127.0.0.1", 4444)).await {
            Ok(conn) => break conn,
            Err(err) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                warn!(?err, "failed to connect to service 127.0.0.1:4444");
            }
        }
    };

    info!("connected to local serivce, proxying");

    // todo: what does copy do, exactly? copy tcp data sections?
    // what if serivce shutdown early? reconnect?
    tokio::io::copy_bidirectional(&mut service, &mut tunnel).await?;

    info!("closed");

    Ok(())
}

async fn run_client() -> Result<()> {
    info!("✨ running the spoke!");

    // open up connection from spoke to the hub
    info!("connecting to the hub 127.0.0.1:4242");
    let mut coord = loop {
        match TcpStream::connect(("127.0.0.1", 4242)).await {
            Ok(conn) => break conn,
            Err(err) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                warn!(?err, "failed to connect to the hub 127.0.0.1:4242");
            }
        }
    };
    info!("connected to the hub");
    info!("trying to establish a coordination connection");

    coord
        .write_u8(Message::Coord.into())
        .await
        .wrap_err("failed to establish coordination connection with the hub")?;

    let message: Message = coord.read_u8().await?.into();
    ensure!(
        message == Message::Ack,
        "the hub should have acked our request to establish a coordination connection, but it won't, who's to blame?"
    );

    info!("coordination connection established");
    // todo: what if hub disconnects afterwards?

    info!("waiting for server to ask us to open a tunnel, because the server can't do that itself");
    loop {
        let message: Message = coord.read_u8().await?.into();
        match message {
            Message::Coord => unreachable!(),
            Message::Tunnel => unreachable!(),
            Message::Ack => unreachable!(),
            Message::Accept => {
                tokio::spawn(async move {
                    // todo: single failure shouldn't bring down the whole system
                    client_accept().await.unwrap();
                });
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing()?;
    install_eyre()?;

    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 2, "which peer to run: `server` or `client`?");

    let peer = &args[1];

    if peer == "server" {
        run_server().await?;
    } else if peer == "client" {
        run_client().await?;
    } else {
        bail!("which peer to run: `server` or `client`?");
    }

    Ok(())
}
