use bytes::BytesMut;
use hmac::Hmac;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio_util::codec::{Decoder, Encoder, LengthDelimitedCodec};

pub type HmacSha256 = Hmac<Sha256>;

#[repr(u8)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum Message {
    /// Used by spoke to open a control channel
    Control { service_port: u16 },

    /// Used by spoke to open a data channel
    Data,

    /// Used by hub to acknowledge a message sent by the spoke
    Ack,

    /// Used by hub to challenge whether if the spoke holds the key
    Challenge { payload: Vec<u8> },

    /// Used by hub to challenge whether if the spoke holds the key
    Answer(Vec<u8>),

    /// Used by hub to tell the spoke to open a data channel
    Accept,
}

pub type Framed = tokio_util::codec::Framed<tokio::net::TcpStream, FunCodec>;

#[derive(Debug, Clone)]
pub struct FunCodec {
    pub length_codec: LengthDelimitedCodec,
}

impl FunCodec {
    pub fn new() -> Self {
        Self {
            length_codec: LengthDelimitedCodec::builder()
                .length_field_type::<u16>()
                .new_codec(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FunError {
    #[error("IO error")]
    Io(#[from] std::io::Error),

    #[error("decode error")]
    Decode(#[from] serde_json::Error),
}

impl Decoder for FunCodec {
    type Item = Message;
    type Error = FunError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let src = match self.length_codec.decode(src) {
            Ok(Some(src)) => src,
            Ok(None) => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        Ok(Some(serde_json::from_slice(&src[..])?))
    }
}

impl Encoder<Message> for FunCodec {
    type Error = FunError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let buf = serde_json::to_vec(&item)?;
        let buf = bytes::Bytes::from(buf);

        self.length_codec.encode(buf, dst)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use futures::{SinkExt, StreamExt};
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn test_tokio_codec() {
        let codec = FunCodec::new();

        let (client, server) = tokio::io::duplex(64);
        let mut client = tokio_util::codec::Framed::new(client, codec.clone());
        let mut server = tokio_util::codec::Framed::new(server, codec);

        client.send(Message::Ack).await.unwrap();

        assert_eq!(server.next().await.unwrap().unwrap(), Message::Ack);
    }

    #[tokio::test]
    async fn test_tokio_codec_peek() {
        let codec = FunCodec::new();

        let (client, mut server) = tokio::io::duplex(64);
        let mut client = tokio_util::codec::Framed::new(client, codec.clone());

        client.send(Message::Ack).await.unwrap();

        let mut buf = BytesMut::new();
        server.read_buf(&mut buf).await.unwrap();

        dbg!(buf);
    }
}
