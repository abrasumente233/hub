use color_eyre::eyre::{bail, ensure, Result, WrapErr};
use tokio::{
    fs::File,
    net::{TcpListener, TcpStream},
};
use tracing::info;

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

    let listener = TcpListener::bind(("0.0.0.0", 1337)).await?;

    loop {
        let (mut socket, peer_addr) = listener.accept().await?;
        info!(?peer_addr, "connected");
        let mut file = File::open("Cargo.toml")
            .await
            .wrap_err("cannot open Cargo.toml")?;
        tokio::io::copy(&mut file, &mut socket).await?;
    }
}

async fn run_client() -> Result<()> {
    info!("✨ running the spoke!");
    Ok(())
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
