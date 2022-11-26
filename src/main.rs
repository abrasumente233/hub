use std::time::Duration;

use color_eyre::eyre::{bail, ensure, Result, WrapErr};
use tokio::{
    fs::File,
    net::{TcpListener, TcpStream},
};
use tracing::{info, warn};

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

async fn run_server() -> Result<()> {
    info!("✨ running the hub at 0.0.0.0:1337!");

    let mut service_stream = {
        let service_listener = TcpListener::bind(("0.0.0.0", 4242)).await?;

        info!("waiting for spoke (the service behind NAT) come up at 0.0.0.0:4242");
        let (stream, addr) = service_listener.accept().await?;
        info!(?addr, "spoke connected!");

        // note: listener is dropped here to prevent mutiple spokes connected in
        stream
    };

    // todo: what happens the serivce conn breaks afterwards? reconnect.

    let listener = TcpListener::bind(("0.0.0.0", 1337)).await?;

    info!("anyone who want to access the service can go to 0.0.0.0:1337");
    loop {
        let (mut socket, peer_addr) = listener.accept().await?;
        info!(?peer_addr, "connected");

        tokio::io::copy_bidirectional(&mut socket, &mut service_stream).await?;
    }
}

async fn run_client() -> Result<()> {
    info!("✨ running the spoke!");

    // suppose we have a dummy file hosting service
    // we will replace it with a proxy from specific
    // local port in the end
    // it just have to be `AsyncRead + AsyncWrite`
    let mut service = File::open("./Cargo.toml").await?;

    // open up connection from spoke to the hub
    info!("connecting to the hub 127.0.0.1:4242");
    let mut conn = loop {
        match TcpStream::connect(("127.0.0.1", 4242)).await {
            Ok(conn) => break conn,
            Err(err) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
                warn!(?err, "connecting to the hub 127.0.0.1:4242");
            }
        }
    };
    info!("connected to the hub");

    // info!("connecting to the serivce to be proxyed: 127.0.0.1:4444");

    // since our `File` is `AsyncRead`-only, we `copy` instead
    // of `copy_bidirectional`, for now
    // todo: what does copy do, exactly? copy tcp data sections?
    tokio::io::copy(&mut service, &mut conn).await?;

    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    //Ok(())
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
