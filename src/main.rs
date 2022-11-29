use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{ensure, Result, WrapErr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, instrument, trace, warn};

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
    ctrl: Option<TcpStream>,
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

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Message {
    /// Used by spoke to open a control channel
    Control = 1,

    /// Used by spoke to open a data channel
    Data = 2,

    /// Used by hub to acknowledge a message sent by the spoke
    Ack = 3,

    /// Used by hub to tell the spoke to open a data channel
    Accept = 4,
}

impl From<u8> for Message {
    fn from(message: u8) -> Self {
        use Message::*;
        match message {
            1 => Control,
            2 => Data,
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
            Control => 1,
            Data => 2,
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
            ctrl: None,
        }
    }

    // it's going banana cakes, the amount of failure points is getting
    // crazy at this point. for example, any coordination error will boil
    // down to reconnecting, but how do you know, when calling a function,
    // if coordination breaks?
    async fn run(&mut self) -> Result<()> {
        info!(
            "✨ running the hub at {{exposed_addr={}, tunnel_addr={}}}!",
            self.exposed_addr, self.tunnel_addr
        );

        'outer: loop {
            // if we can't establish coordination, there's nothing we can do, so bail out.
            info!("opening control channel");
            let (coord, _) = loop {
                match self.server_establish_with_timeout().await {
                    Ok(conn) => break conn,
                    Err(_) => {
                        warn!("can't open control channel, is the spoke down?");
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            };
            self.ctrl.replace(coord);
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
    #[instrument(skip(self, client))]
    async fn handle_server_connection(
        &mut self,
        mut client: TcpStream,
        client_addr: SocketAddr,
    ) -> Result<()> {
        trace!(?client_addr, "connected");

        trace!("asking the spoke to open up a data channel");

        let (mut tunnel, tunnel_addr) = self
            .server_establish_with_timeout()
            .await
            .wrap_err("can't open data channel")?;

        trace!(?tunnel_addr, "opened data channel");

        info!("tunneling");

        tokio::spawn(async move {
            // todo: one connection failure shouldn't bring down the whole system
            // fix this unwrap
            tokio::io::copy_bidirectional(&mut client, &mut tunnel)
                .await
                .unwrap();

            trace!("data channel closed");
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
        let is_tunnel = self.ctrl.is_some();
        let (mut stream, addr) = {
            let listener = TcpListener::bind(self.tunnel_addr)
                .await
                .wrap_err("bind error")?;

            // todo: we want to be able to do
            //       `self.coord.send(Message::Accept)`
            //
            if is_tunnel {
                self.ctrl
                    .as_mut()
                    .unwrap()
                    .write_u8(Message::Accept.into())
                    .await?;
            }

            trace!("waiting for spoke at {}", self.tunnel_addr);
            let (stream, addr) = listener.accept().await?;
            trace!(?addr, "spoke connected!");

            // note: listener is dropped here to prevent mutiple spokes connected in
            (stream, addr)
        };

        // really should use `tokio_util::codec`, but hey :), can't we be naive?
        // expect spoke to declare `coord` is a coordnation connection
        let message: Message = stream.read_u8().await?.into();
        ensure!(
            message
                == if is_tunnel {
                    Message::Data
                } else {
                    Message::Control
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
            trace!("connecting to the hub {}", self.tunnel_addr);
            let mut coord = loop {
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

            coord
                .write_u8(Message::Control.into())
                .await
                .wrap_err("failed to establish coordination connection with the hub")?;

            let message: Message = coord.read_u8().await?.into();
            ensure!(
                message == Message::Ack,
                "the hub should have acked our request to establish a coordination connection, but it won't, who's to blame?"
            );

            info!("✨ opened control channel!");
            info!("all good! listening for connections");

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

    #[instrument(skip(self))]
    async fn accept(&self) -> Result<()> {
        trace!("since the hub asked, opening a data channel to the hub");

        // open up connection from spoke to the hub
        trace!("connecting to the hub {}", self.tunnel_addr);
        let mut tunnel = loop {
            match TcpStream::connect(self.tunnel_addr).await {
                Ok(conn) => break conn,
                Err(err) => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    warn!(?err, "failed to connect to the hub {}", self.tunnel_addr);
                }
            }
        };
        trace!("connected to the hub");
        trace!("trying to establish a tunnel connection");

        // actually tell the hub we want a data channel
        // error: the hub side of the tunnel could be closed, for unexpected reasons
        //        resulting in an error when `write_u8`, not good not terrible, we could
        //        reconnect. but for now we just give up
        tunnel
            .write_u8(Message::Data.into())
            .await
            .wrap_err("failed to open data channel")?;

        // same error, could retry but whatever
        let message: Message = tunnel.read_u8().await?.into();
        ensure!(
            message == Message::Ack,
            "can't establish hub -> spoke coordination connection"
        );

        trace!("opened data channel");

        trace!("connecting to local serivce");

        // use some retry backoffs?
        // todo: maybe we shouldn't retry since it's local. when we can't connect,
        // it's more likely that the service is down, rather than there's a
        // network issue...
        let mut service = loop {
            match TcpStream::connect(self.local_addr).await {
                Ok(conn) => break conn,
                Err(err) => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    warn!(
                        ?err,
                        "failed to connect to local service {}, is the service down?",
                        self.local_addr
                    );
                }
            }
        };

        info!("connected to local serivce, proxying to tunnel");

        // todo: what does copy do, exactly? copy tcp data sections?
        // what if serivce shutdown early? reconnect?
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
