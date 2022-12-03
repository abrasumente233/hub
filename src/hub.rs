use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use color_eyre::eyre::{bail, ensure, ContextCompat, Result, WrapErr};
use futures::{SinkExt, StreamExt};
use hmac::digest::Mac;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, instrument, trace, warn};

use crate::handshake::*;

const DEFAULT_EXPOSED_PORT: u16 = 1337;

pub struct Hub {
    exposed_addr: IpAddr,
    exposed_port: Option<u16>,
    tunnel_addr: SocketAddr,
    ctrl: Option<Framed>,
    key: Option<String>,
}

impl Hub {
    pub fn new<A>(
        exposed_addr: IpAddr,
        exposed_port: Option<u16>,
        tunnel_addr: A,
        key: Option<String>,
    ) -> Self
    where
        A: ToSocketAddrs,
    {
        Self {
            exposed_addr,
            exposed_port,
            tunnel_addr: tunnel_addr.to_socket_addrs().unwrap().next().unwrap(),
            ctrl: None,
            key,
        }
    }

    // it's going banana cakes, the amount of failure points is getting
    // crazy at this point. for example, any control chan error will boil
    // down to reconnecting, but how do you know, when calling a function,
    // if control channel breaks?
    pub async fn run(&mut self) -> Result<()> {
        info!(
            "✨ running the hub at (exposed_addr={}, tunnel_addr={})!",
            self.exposed_addr, self.tunnel_addr
        );

        'reconnect: loop {
            info!("opening control channel");
            let (ctrl, _) = self.open_chan_retry().await;
            self.ctrl.replace(ctrl);

            let listener = TcpListener::bind((
                self.exposed_addr,
                self.exposed_port.unwrap_or(DEFAULT_EXPOSED_PORT),
            ))
            .await?;
            info!("✨ all good! listening for connections");
            info!(
                "service exposed at {}:{}",
                self.exposed_addr,
                self.exposed_port.unwrap_or(DEFAULT_EXPOSED_PORT)
            );

            loop {
                let (client, client_addr) = listener.accept().await?;

                match self.handle_server_connection(client, client_addr).await {
                    Ok(_) => (),
                    Err(err) => {
                        // can't connect to the spoke, reconnect
                        warn!(?err);
                        self.ctrl.take();
                        continue 'reconnect;
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
        trace!("asking the spoke to open up a data channel");

        let (tunnel, tunnel_addr) = self
            .open_chan_timeout()
            .await
            .wrap_err("can't open data channel")?;

        trace!(?tunnel_addr, "opened data channel");

        tokio::spawn(async move {
            // todo: one connection failure shouldn't bring down the whole system
            // fix these two unwrap

            let mut parts = tunnel.into_parts();
            assert!(parts.write_buf.is_empty());
            client.write_buf(&mut parts.read_buf).await.unwrap(); // clean up read buffer
            if tokio::io::copy_bidirectional(&mut client, &mut parts.io)
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
    async fn open_chan(&mut self) -> Result<(Framed, SocketAddr)> {
        // todo: retry if at all question marks, only return error when timeout or
        // exceeded maximum retries.
        let is_tunnel = self.ctrl.is_some();
        let (stream, addr) = {
            let listener = TcpListener::bind(self.tunnel_addr)
                .await
                .wrap_err("bind error")?;

            if is_tunnel {
                self.ctrl.as_mut().unwrap().send(Message::Accept).await?;
            }

            trace!("waiting for spoke at {}", self.tunnel_addr);
            let (stream, addr) = listener.accept().await?;
            trace!(?addr, "spoke connected!");

            // note: listener is dropped here to prevent mutiple spokes connected in
            (stream, addr)
        };

        let mut stream = Framed::new(stream, FunCodec::new());

        // expect spoke to declare `ctrl` is a control channel
        // todo: this is so ugly, why?
        let message = stream.next().await.context("connection reset by peer")??;
        if is_tunnel {
            ensure!(
                message == Message::Data,
                "wrong type of channel opened between the hub and spoke"
            );
        } else {
            match message {
                Message::Control { service_port } => {
                    if self.exposed_port.is_none() {
                        self.exposed_port.replace(service_port);
                    }
                }
                _ => bail!("wrong type of channel opened between the hub and spoke"),
            }
        }

        if self.key.is_some() {
            // challenge the spoke to see whether it has the key
            let challenge = uuid::Uuid::new_v4().into_bytes().to_vec();

            stream
                .send(Message::Challenge {
                    payload: challenge.clone(), // todo: use array instead of
                                                // vec!
                })
                .await?;

            let mut code = HmacSha256::new_from_slice(b"top secret").unwrap();
            code.update(&challenge);

            let message = stream.next().await.context("connection reset by peer")??;

            match message {
                Message::Answer(answer) => {
                    if code.verify_slice(&answer).is_err() {
                        warn!("auth failed");
                        bail!("auth failed");
                    }
                }
                _ => bail!("auth failed"),
            }

            // auth passed, ack back
            if !is_tunnel {
                info!("auth succeeded!");
            }
        }

        stream.send(Message::Ack).await?;

        Ok((stream, addr))
    }

    async fn open_chan_timeout(&mut self) -> Result<(Framed, SocketAddr)> {
        tokio::time::timeout(Duration::from_secs(3), self.open_chan())
            .await
            .wrap_err("timeout establishing connection to the spoke, is the spoke up?")?
    }

    async fn open_chan_retry(&mut self) -> (Framed, SocketAddr) {
        loop {
            match self.open_chan_timeout().await {
                Ok(conn) => break conn,
                Err(_) => {
                    warn!("can't open tunnel, is the spoke up?");
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}
