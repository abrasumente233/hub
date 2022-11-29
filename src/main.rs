use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{ensure, Result, WrapErr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, instrument, warn};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

struct Hub {
    exposed_addr: SocketAddr,
    tunnel_addr: SocketAddr,
    coord: Option<TcpStream>,
}

#[derive(Clone)]
struct Spoke {
    local_addr: SocketAddr,
    tunnel_addr: SocketAddr,
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

impl Hub {
    fn new<A1, A2>(exposed_addr: A1, tunnel_addr: A2) -> Self
    where
        A1: ToSocketAddrs,
        A2: ToSocketAddrs,
    {
        Self {
            exposed_addr: exposed_addr.to_socket_addrs().unwrap().next().unwrap(),
            tunnel_addr: tunnel_addr.to_socket_addrs().unwrap().next().unwrap(),
            coord: None,
        }
    }

    // it's going banana cakes, the amount of failure points is getting
    // crazy at this point. for example, any coordination error will boil
    // down to reconnecting, but how do you know, when calling a function,
    // if coordination breaks?
    async fn run(&mut self) -> Result<()> {
        info!(
            "✨ running the hub at < exposed_addr={}, tunnel_addr={} >!",
            self.exposed_addr, self.tunnel_addr
        );

        'outer: loop {
            // if we can't establish coordination, there's nothing we can do, so bail out.
            info!("trying to establish coordination connection");
            let (coord, _) = loop {
                match self.server_establish_with_timeout().await {
                    Ok(conn) => break conn,
                    Err(_) => {
                        warn!("can't connect to spoke to establish coordination connection, is the spoke down?");
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            };
            self.coord.replace(coord);
            info!("established coordination connection");

            let listener = TcpListener::bind(self.exposed_addr).await?;

            info!(
                "anyone who want to access the service can go to {}",
                self.exposed_addr
            );
            loop {
                let (client, client_addr) = listener.accept().await?;

                // todo: again, one failure shouldn't bring down the whole system
                match self.handle_server_connection(client, client_addr).await {
                    Ok(_) => (),
                    Err(err) => {
                        warn!(?err);
                        self.coord.take();
                        continue 'outer;
                    }
                };
            }
        }
    }

    /// Returns error when we can't connect to the spoke
    #[instrument(skip(self, client))]
    async fn handle_server_connection(
        &mut self,
        mut client: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        info!(?client_addr, "connected");

        info!("asking the spoke to open up a tunnel");

        let (mut tunnel, tunnel_addr) = self
            .server_establish_with_timeout()
            .await
            .wrap_err("cannot open tunnel")?;

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

    /// Used by server to establish a connection with the spoke.
    ///
    /// If `coord` is `Some`, this function will try to establish a
    /// tunnel connection, otherwise a coordination connection.
    async fn server_establish(&mut self) -> Result<(TcpStream, SocketAddr)> {
        // todo: retry if at all question marks, only return error when timeout or
        // exceeded maximum retries.
        let is_tunnel = self.coord.is_some();
        let (mut stream, addr) = {
            let service_listener = TcpListener::bind(self.tunnel_addr)
                .await
                .wrap_err("bind error")?;
            if is_tunnel {
                self.coord
                    .as_mut()
                    .unwrap()
                    .write_u8(Message::Accept.into())
                    .await?;
            }

            info!("waiting for spoke come up at {}", self.tunnel_addr);
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

    async fn server_establish_with_timeout(&mut self) -> Result<(TcpStream, SocketAddr)> {
        tokio::time::timeout(Duration::from_secs(3), self.server_establish())
            .await
            .wrap_err("timeout establishing connection to the spoke, is the spoke down?")?
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
            info!("connecting to the hub {}", self.tunnel_addr);
            let mut coord = loop {
                match TcpStream::connect(self.tunnel_addr).await {
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

            info!("waiting for server to ask us to open a tunnel, because the server can't do that itself");
            loop {
                let message: Message =
                    match coord.read_u8().await.wrap_err(
                        "coordination channel encounters unexpected EOF, is the hub down?",
                    ) {
                        Ok(message) => message,
                        Err(err) => {
                            warn!(?err);
                            continue 'outer;
                        }
                    }
                    .into();
                match message {
                    Message::Coord => unreachable!(),
                    Message::Tunnel => unreachable!(),
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

    #[instrument(skip(self))]
    async fn accept(&self) -> Result<()> {
        info!("since the hub asked, we are opening a tunnel to hub");

        // open up connection from spoke to the hub
        info!("connecting to the hub {}", self.tunnel_addr);
        let mut tunnel = loop {
            match TcpStream::connect(self.tunnel_addr).await {
                Ok(conn) => break conn,
                Err(err) => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    warn!(?err, "failed to connect to the hub {}", self.tunnel_addr);
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
            "can't establish hub -> spoke coordination connection"
        );

        info!("tunnel established");

        info!("connecting to local serivce");

        // use some retry backoffs?
        // todo: maybe we shouldn't retry since it's local. when we can't connect,
        // it's more likely that the service is down, rather than there's a
        // network issue...
        let mut service = loop {
            match TcpStream::connect(self.local_addr).await {
                Ok(conn) => break conn,
                Err(err) => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    warn!(?err, "failed to connect to service {}", self.local_addr);
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
