use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::{bail, ensure, Result, WrapErr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, instrument, trace_span, warn};

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

/// Used by server to establish a connection with the spoke.
///
/// If `coord` is `Some`, this function will try to establish a
/// tunnel connection, otherwise a coordination connection.
async fn server_establish(coord: Option<&mut TcpStream>) -> Result<(TcpStream, SocketAddr)> {
    // todo: retry if at all question marks, only return error when timeout or
    // exceeded maximum retries.
    let is_tunnel = coord.is_some();
    let (mut stream, addr) = {
        let service_listener = TcpListener::bind(("0.0.0.0", 4242)).await?;
        if is_tunnel {
            coord.unwrap().write_u8(Message::Accept.into()).await?;
        }

        info!("waiting for spoke come up at 0.0.0.0:4242");
        let (stream, addr) = service_listener.accept().await?;
        info!(?addr, "spoke connected!");

        // note: listener is dropped here to prevent mutiple spokes connected in
        (stream, addr)
    };
    // todo: what happens the coordination conn breaks afterwards? reconnect.

    // really should use `tokio_util::codec`, but hey :), can't we be naive?
    // expect spoke to declare `coord` is a coordnation connection
    let message: Message = stream.read_u8().await?.into();
    ensure!(
        message
            == if is_tunnel {
                Message::Tunnel
            } else {
                Message::Coord
            },
        "wrong type of connection established between the hub and spoke"
    );
    stream.write_u8(Message::Ack.into()).await?;

    Ok((stream, addr))
}

async fn server_establish_with_timeout(
    coord: Option<&mut TcpStream>,
) -> Result<(TcpStream, SocketAddr)> {
    tokio::time::timeout(Duration::from_secs(3), server_establish(coord))
        .await
        .wrap_err("timeout establishing connection to the spoke, is the spoke down?")?
}

// it's going banana cakes, the amount of failure points is getting
// crazy at this point. for example, any coordination error will boil
// down to reconnecting, but how do you know, when calling a function,
// if coordination breaks?
async fn run_server() -> Result<()> {
    info!("✨ running the hub at 0.0.0.0:1337!");

    'outer: loop {
        // if we can't establish coordination, there's nothing we can do, so bail out.
        info!("trying to establish coordination connection");
        let (mut coord, _) = server_establish_with_timeout(None).await?;
        info!("established coordination connection");

        let listener = TcpListener::bind(("0.0.0.0", 1337)).await?;

        info!("anyone who want to access the service can go to 0.0.0.0:1337");
        loop {
            let (client, client_addr) = listener.accept().await?;

            // todo: again, one failure shouldn't bring down the whole system
            match handle_server_connection(&mut coord, client, client_addr).await {
                Ok(_) => (),
                Err(err) => {
                    warn!(?err);
                    continue 'outer;
                }
            };
        }
    }
}

/// Returns error when we can't connect to the spoke
#[instrument(skip(coord, client))]
async fn handle_server_connection(
    coord: &mut TcpStream,
    mut client: TcpStream,
    client_addr: SocketAddr,
) -> Result<()> {
    info!(?client_addr, "connected");

    info!("asking the spoke to open up a tunnel");

    let (mut tunnel, tunnel_addr) = server_establish_with_timeout(Some(coord)).await?;

    info!(?tunnel_addr, "tunnel established");

    tokio::spawn(async move {
        // todo: one connection failure shouldn't bring down the whole system
        tokio::io::copy_bidirectional(&mut client, &mut tunnel)
            .await
            .unwrap();

        info!("closed");
    });

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

    // the hub side of the tunnel could be closed, for unexpected reasons
    // resulting in an error when `write_u8`, not good not terrible, we could
    // reconnect. but for now we just give up
    tunnel
        .write_u8(Message::Tunnel.into())
        .await
        .wrap_err("failed to establish coordination connection with the hub")?;

    // same error, could retry but whatever
    let message: Message = tunnel.read_u8().await?.into();
    ensure!(
        message == Message::Ack,
        "the hub should have acked our request to establish a coordination connection, but it won't, who's to blame?"
    );

    info!("tunnel established");

    info!("connecting to local serivce");

    // use some retry backoffs?
    // todo: maybe we shouldn't retry since it's local. when we can't connect,
    // it's more likely that the service is down, rather than there's a
    // network issue...
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
    tokio::io::copy_bidirectional(&mut service, &mut tunnel)
        .await
        .wrap_err("broken proxy between service and tunnel")?;

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
                    if let Err(err) = client_accept().await {
                        warn!(?err, "client enountered an error when proxying");
                    }
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
