use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

use color_eyre::eyre::{ensure, ContextCompat, Result, WrapErr};
use futures::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{info, instrument, trace, warn};

use crate::handshake::*;

#[derive(Clone)]
pub struct Spoke {
    local_addr: SocketAddr,
    tunnel_addr: SocketAddr,
}

impl Spoke {
    pub fn new<A1, A2>(local_addr: A1, tunnel_addr: A2) -> Self
    where
        A1: ToSocketAddrs,
        A2: ToSocketAddrs,
    {
        Self {
            local_addr: local_addr.to_socket_addrs().unwrap().next().unwrap(),
            tunnel_addr: tunnel_addr.to_socket_addrs().unwrap().next().unwrap(),
        }
    }

    pub async fn run(&self) -> Result<()> {
        info!("✨ running the spoke!");

        'reconnect: loop {
            // open control channel
            trace!("opening control channel to {}", self.tunnel_addr);
            let mut ctrl = Self::open_chan_retry(
                self.tunnel_addr,
                Message::Control {
                    service_port: self.local_addr.port(),
                },
            )
            .await?;

            info!("✨ all good! listening for connections");

            loop {
                let message = match ctrl
                    .next()
                    .await
                    .context("connection reset by peer")?
                    .wrap_err("control channel error, is the hub up?")
                {
                    Ok(message) => message,
                    Err(err) => {
                        warn!(?err);
                        continue 'reconnect;
                    }
                };
                match message {
                    Message::Control { .. } => unreachable!(),
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

        // open data channel
        trace!("opening data channel to {}", self.tunnel_addr);
        let tunnel = Self::open_chan_retry(self.tunnel_addr, Message::Data).await?;

        // connect to local service
        trace!("connecting to local serivce");
        let mut service = TcpStream::connect(self.local_addr).await.wrap_err(format!(
            "can't connect to local service {}, is the local service up?",
            self.local_addr
        ))?;

        // proxying
        let mut parts = tunnel.into_parts();
        assert!(parts.write_buf.is_empty());
        service
            .write_buf(&mut parts.read_buf)
            .await
            .wrap_err("broken proxy between service and tunnel")?; // clean up buffer
        tokio::io::copy_bidirectional(&mut service, &mut parts.io)
            .await
            .wrap_err("broken proxy between service and tunnel")?;

        trace!("tunnel closed");

        Ok(())
    }

    async fn open_chan_retry(addr: SocketAddr, chan_type: Message) -> Result<Framed> {
        matches!(chan_type, Message::Data | Message::Control { .. });
        let mut chan = loop {
            match TcpStream::connect(addr).await {
                Ok(conn) => break Framed::new(conn, FunCodec::new()),
                Err(err) => {
                    warn!(?err, "failed to connect to the hub {}", addr);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        };

        // handshakes
        chan.send(chan_type)
            .await
            .wrap_err("failed to open control channel with the hub")?;

        let message = chan.next().await.context("connection reset by peer")??;

        ensure!(
            message == Message::Ack,
            "the hub should have acked our request to open a control channel, but it won't"
        );

        Ok(chan)
    }
}
