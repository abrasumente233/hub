mod handshake;
mod hub;
mod spoke;
mod trace;

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;
use hub::Hub;
use spoke::Spoke;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Hub {
        /// Exposed port to the internet.
        #[arg(short, long)]
        port: Option<u16>,

        /// Tunnel port
        #[arg(short, long, default_value_t = 4242)]
        tunnel_port: u16,

        /// Key to protect this hub from being absued, do not leak
        #[arg(short, long)]
        key: Option<String>,
    },
    Spoke {
        /// Local service port
        #[arg(short, long)]
        port: u16,

        /// Tunnel port for connecting to the hub
        #[arg(short, long, default_value_t = 4242)]
        tunnel_port: u16,

        /// Tunnel address for connecting to the hub
        #[arg(short = 'i', long)]
        tunnel_ip: std::net::IpAddr,

        /// Key to the hub
        #[arg(short, long)]
        key: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    trace::init()?;

    let args = Args::parse();
    match args.command {
        Commands::Hub {
            port,
            tunnel_port,
            key,
        } => {
            Hub::new(
                "0.0.0.0".parse().unwrap(),
                port,
                ("0.0.0.0", tunnel_port),
                key,
            )
            .run()
            .await?;
        }
        Commands::Spoke {
            port,
            tunnel_port,
            tunnel_ip,
            key,
        } => {
            Spoke::new(("0.0.0.0", port), (tunnel_ip, tunnel_port), key)
                .run()
                .await?;
        }
    }

    Ok(())
}
