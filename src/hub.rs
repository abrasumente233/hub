use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use color_eyre::eyre::{ensure, Result, WrapErr};
use futures::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, instrument, trace, warn};

use crate::handshake::*;

pub struct Hub {
    exposed_addr: SocketAddr,
    tunnel_addr: SocketAddr,
    ctrl: Option<Framed>,
}

impl Hub {
    pub fn new<A1, A2>(exposed_addr: A1, tunnel_addr: A2) -> Self
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
    pub async fn run(&mut self) -> Result<()> {
        info!(
            "✨ running the hub at (exposed_addr={}, tunnel_addr={})!",
            self.exposed_addr, self.tunnel_addr
        );

        'reconnect: loop {
            info!("opening control channel");
            let (ctrl, _) = self.open_chan_retry().await;
            self.ctrl.replace(ctrl);

            let listener = TcpListener::bind(self.exposed_addr).await?;
            info!("✨ all good! listening for connections");
            info!("service exposed at {}", self.exposed_addr);

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
        let message = stream.next().await.unwrap()?;
        ensure!(
            message
                == if is_tunnel {
                    Message::Data
                } else {
                    Message::Control
                },
            "wrong type of channel opened between the hub and spoke"
        );
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
