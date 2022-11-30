mod handshake;

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{ensure, Result, WrapErr};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, instrument, trace, warn};

use crate::handshake::*;

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
        port: u16,

        /// Tunnel port
        #[arg(short, long, default_value_t = 4242)]
        tunnel_port: u16,
    },
    Spoke {
        /// Local service port
        #[arg(short, long)]
        port: u16,

        /// Tunnel port for connecting to the hub
        #[arg(short, long, default_value_t = 4242)]
        tunnel_port: u16,

        /// Tunnel port for connecting to the hub
        #[arg(short = 'i', long)]
        tunnel_ip: std::net::IpAddr,
    },
}

struct Hub {
    exposed_addr: SocketAddr,
    tunnel_addr: SocketAddr,
    ctrl: Option<Framed<TcpStream>>,
}

#[derive(Clone)]
struct Spoke {
    local_addr: SocketAddr,
    tunnel_addr: SocketAddr,
}

fn install_eyre() -> Result<()> {
    color_eyre::install()?;
    Ok(())
}

fn install_tracing() -> Result<()> {
    use tracing_subscriber::{prelude::*, EnvFilter, Registry};
    use tracing_tree::HierarchicalLayer;

    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::default().add_directive("hub=info".parse()?),
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

impl Hub {
    fn new<A1, A2>(exposed_addr: A1, tunnel_addr: A2) -> Self
    where
        A1: ToSocketAddrs,
        A2: ToSocketAddrs,
    {
        Self {
            exposed_addr: exposed_addr.to_socket_addrs().unwrap().next().unwrap(),
            tunnel_addr: tunnel_addr.to_socket_addrs().unwrap().next().unwrap(),
            ctrl: None,
        }
    }

    // it's going banana cakes, the amount of failure points is getting
    // crazy at this point. for example, any control chan error will boil
    // down to reconnecting, but how do you know, when calling a function,
    // if control channel breaks?
    async fn run(&mut self) -> Result<()> {
        info!(
            "✨ running the hub at {{exposed_addr={}, tunnel_addr={}}}!",
            self.exposed_addr, self.tunnel_addr
        );

        'outer: loop {
            // if we can't open control channel, there's nothing we can do, so bail out.
            info!("opening control channel");
            let (ctrl, _) = loop {
                match self.server_establish_with_timeout().await {
                    Ok(conn) => break conn,
                    Err(_) => {
                        warn!("can't open control channel, is the spoke up?");
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            };
            self.ctrl.replace(ctrl);
            info!("✨ opened control channel!");
            info!("all good! listening for connections");

            let listener = TcpListener::bind(self.exposed_addr).await?;

            info!("service exposed at {}", self.exposed_addr);
            loop {
                let (client, client_addr) = listener.accept().await?;

                match self.handle_server_connection(client, client_addr).await {
                    Ok(_) => (),
                    Err(err) => {
                        warn!(?err);
                        // can't connect to the spoke, reconnect
                        self.ctrl.take();
                        continue 'outer;
                    }
                };
            }
        }
    }

    /// Returns error when we can't connect to the spoke
    #[instrument(level = "trace", skip(self, client))]
    async fn handle_server_connection(
        &mut self,
        mut client: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        trace!(?client_addr, "connected");

        trace!("asking the spoke to open up a data channel");

        let (tunnel, tunnel_addr) = self
            .server_establish_with_timeout()
            .await
            .wrap_err("can't open data channel")?;

        trace!(?tunnel_addr, "opened data channel");

        tokio::spawn(async move {
            // todo: one connection failure shouldn't bring down the whole system
            // fix these two unwrap

            let (mut stream, mut buffer) = tunnel.into_parts();
            client.write_buf(&mut buffer).await.unwrap(); // clean up read buffer
            if tokio::io::copy_bidirectional(&mut client, &mut stream)
                .await
                .is_err()
            {
                warn!("connection reset by peer");
            }

            trace!("data channel closed");
        });

        Ok(())
    }

    /// Used by server to establish a connection with the spoke.
    async fn server_establish(&mut self) -> Result<(Framed<TcpStream>, SocketAddr)> {
        // todo: retry if at all question marks, only return error when timeout or
        // exceeded maximum retries.
        let is_tunnel = self.ctrl.is_some();
        let (stream, addr) = {
            let listener = TcpListener::bind(self.tunnel_addr)
                .await
                .wrap_err("bind error")?;

            if is_tunnel {
                self.ctrl
                    .as_mut()
                    .unwrap()
                    .write_frame(Message::Accept)
                    .await?;
            }

            trace!("waiting for spoke at {}", self.tunnel_addr);
            let (stream, addr) = listener.accept().await?;
            trace!(?addr, "spoke connected!");

            // note: listener is dropped here to prevent mutiple spokes connected in
            (stream, addr)
        };

        let mut stream = Framed::new(stream);

        // expect spoke to declare `ctrl` is a control channel
        let message: Message = stream.read_frame().await?;
        ensure!(
            message
                == if is_tunnel {
                    Message::Data
                } else {
                    Message::Control
                },
            "wrong type of connection established between the hub and spoke"
        );
        stream.write_frame(Message::Ack).await?;

        Ok((stream, addr))
    }

    async fn server_establish_with_timeout(&mut self) -> Result<(Framed<TcpStream>, SocketAddr)> {
        tokio::time::timeout(Duration::from_secs(3), self.server_establish())
            .await
            .wrap_err("timeout establishing connection to the spoke, is the spoke up?")?
    }
}

impl Spoke {
    fn new<A1, A2>(local_addr: A1, tunnel_addr: A2) -> Self
    where
        A1: ToSocketAddrs,
        A2: ToSocketAddrs,
    {
        Self {
            local_addr: local_addr.to_socket_addrs().unwrap().next().unwrap(),
            tunnel_addr: tunnel_addr.to_socket_addrs().unwrap().next().unwrap(),
        }
    }

    async fn run(&self) -> Result<()> {
        info!("✨ running the spoke!");

        'outer: loop {
            // open up connection from spoke to the hub
            trace!("connecting to the hub {}", self.tunnel_addr);
            let ctrl = loop {
                match TcpStream::connect(self.tunnel_addr).await {
                    Ok(conn) => break conn,
                    Err(err) => {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        warn!(?err, "failed to connect to the hub {}", self.tunnel_addr);
                    }
                }
            };
            trace!("connected to the hub");
            trace!("opening control channel");

            let mut ctrl = Framed::new(ctrl);

            ctrl.write_frame(Message::Control)
                .await
                .wrap_err("failed to open control channel with the hub")?;

            let message = ctrl.read_frame().await?;
            ensure!(
                message == Message::Ack,
                "the hub should have acked our request to open a control channel, but it won't"
            );

            info!("✨ opened control channel!");
            info!("all good! listening for connections");

            loop {
                let message = match ctrl
                    .read_frame()
                    .await
                    .wrap_err("control channel error, is the hub up?")
                {
                    Ok(message) => message,
                    Err(err) => {
                        warn!(?err);
                        continue 'outer;
                    }
                };
                match message {
                    Message::Control => unreachable!(),
                    Message::Data => unreachable!(),
                    Message::Ack => unreachable!(),
                    Message::Accept => {
                        // tunnel creation error, maybe because hub is down, or maybe because
                        // of accident.
                        //
                        //   1. if the hub connection is down, next loop `read_u8` will error out,
                        //      casuing reconnection to the hub.
                        //   2. if by accident, retry is left as an exercise for the user.
                        //
                        let spoke = self.clone();
                        tokio::spawn(async move {
                            // todo: single failure shouldn't bring down the whole system
                            if let Err(err) = spoke.accept().await {
                                warn!(
                                    ?err,
                                    "spoke enountered an error when accepting a connection"
                                );
                            }
                        });
                    }
                }
            }
        }
    }

    #[instrument(level = "trace", skip(self))]
    async fn accept(&self) -> Result<()> {
        trace!("since the hub asked, opening a data channel to the hub");

        // open up connection from spoke to the hub
        trace!("connecting to the hub {}", self.tunnel_addr);
        let tunnel = loop {
            match TcpStream::connect(self.tunnel_addr).await {
                Ok(conn) => break conn,
                Err(err) => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    warn!(?err, "failed to connect to the hub {}", self.tunnel_addr);
                }
            }
        };
        let mut tunnel = Framed::new(tunnel);
        trace!("connected to the hub");
        trace!("trying to establish a tunnel connection");

        // actually tell the hub we want a data channel
        // error: the hub side of the tunnel could be closed, for unexpected reasons
        //        resulting in an error when `write_u8`, not good not terrible, we could
        //        reconnect. but for now we just give up
        tunnel
            .write_frame(Message::Data)
            .await
            .wrap_err("failed to open data channel")?;

        // same error, could retry but whatever
        let message: Message = tunnel.read_frame().await?;
        ensure!(
            message == Message::Ack,
            "can't establish hub -> spoke control channel"
        );

        trace!("opened data channel");

        trace!("connecting to local serivce");

        let mut service = TcpStream::connect(self.local_addr).await.wrap_err(format!(
            "can't connect to local service {}, is the local service up?",
            self.local_addr
        ))?;

        let (mut tunnel, mut buffer) = tunnel.into_parts();
        service
            .write_buf(&mut buffer)
            .await
            .wrap_err("broken proxy between service and tunnel")?; // clean up buffer
        tokio::io::copy_bidirectional(&mut service, &mut tunnel)
            .await
            .wrap_err("broken proxy between service and tunnel")?;

        trace!("tunnel closed");

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing()?;
    install_eyre()?;

    let args = Args::parse();

    match args.command {
        Commands::Hub { port, tunnel_port } => {
            Hub::new(("0.0.0.0", port), ("0.0.0.0", tunnel_port))
                .run()
                .await?;
        }
        Commands::Spoke {
            port,
            tunnel_port,
            tunnel_ip,
        } => {
            Spoke::new(("0.0.0.0", port), (tunnel_ip, tunnel_port))
                .run()
                .await?;
        }
    }

    Ok(())
}
