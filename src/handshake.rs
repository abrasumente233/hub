use bytes::{Buf, BytesMut};
use derive_more::{Display, Error};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Message {
    /// Used by spoke to open a control channel
    Control = 1,

    /// Used by spoke to open a data channel
    Data = 2,

    /// Used by hub to acknowledge a message sent by the spoke
    Ack = 3,

    /// Used by hub to tell the spoke to open a data channel
    Accept = 4,
}

#[derive(Debug, PartialEq, Eq, Display, Error)]
pub struct DecodeError;

impl TryFrom<u8> for Message {
    type Error = DecodeError;

    fn try_from(message: u8) -> Result<Self, Self::Error> {
        use Message::*;
        Ok(match message {
            1 => Control,
            2 => Data,
            3 => Ack,
            4 => Accept,
            _ => return Err(DecodeError),
        })
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

impl Message {
    fn check<B: Buf>(buf: &B) -> bool {
        buf.remaining() >= 1
    }
}

impl Message {
    fn decode<B: Buf>(buf: &mut B) -> Result<Option<Self>, DecodeError> {
        if !Message::check(buf) {
            return Ok(None);
        }

        Message::try_from(buf.get_u8())
            .map(Some)
            .map_err(|_| DecodeError)
    }

    // fn encode(self, buf: BytesMut) {
    //     buf.put_u8(self.into());
    // }
}

pub struct Framed<I> {
    io: I,
    buffer: BytesMut,
}

#[derive(Debug, thiserror::Error)]
pub enum FramedError {
    #[error("read IO error")]
    Io(#[from] std::io::Error),

    #[error("invalid message")]
    Decode(#[from] DecodeError),

    #[error("input incomplete")]
    Incomplete,
}

impl<I> Framed<I> {
    pub fn new(io: I) -> Self {
        Self {
            io,
            buffer: BytesMut::new(),
        }
    }

    pub async fn read_frame(&mut self) -> Result<Message, FramedError>
    where
        I: AsyncRead + Unpin,
    {
        loop {
            match Message::decode(&mut self.buffer)? {
                Some(message) => break Ok(message),
                None => {
                    if 0 == self.io.read_buf(&mut self.buffer).await? {
                        return Err(FramedError::Incomplete);
                    }
                }
            }
        }
    }

    pub async fn write_frame(&mut self, message: Message) -> std::io::Result<()>
    where
        I: AsyncWrite + Unpin,
    {
        self.io.write_u8(message.into()).await?;
        Ok(())
    }

    pub fn into_parts(self) -> (I, BytesMut) {
        (self.io, self.buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_codec() {
        let (client, mut server) = tokio::io::duplex(64);

        // write invalid data
        server.write_u8(233).await.unwrap();

        // frame both stream
        let (mut client, mut server) = (Framed::new(client), Framed::new(server));

        assert!(client.read_frame().await.is_err());

        // normal data
        server.write_frame(Message::Ack).await.unwrap();
        assert_eq!(client.read_frame().await.unwrap(), Message::Ack);
    }
}
