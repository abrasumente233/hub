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
    // Do we need `bytes`? no! we just need to
    // read on. since this can be an async fn, we
    // can afford to do that!
    //
    // I don't know why tokio codec doesn't do that,
    // maybe their `Decoder` is a trait and we don't
    // get async fn in traits (when they invented tokio_codec),
    // but this doesn't prevent them from inventing an
    // `AsyncDecoder` that has a
    // `fn poll_decode(reader: R) -> Poll<Result<Frame, PraseError>>` method.
    //
    // Oh! It could be the tokio authors don't want
    // us to `read` an random `AsyncRead` over and over, which, in the case
    // of `TcpStream`, could invovles syscalls, which are typically expensive
    // and buffering is desired instead, thus `Bytes`.
    //
    // Uhh, if you want buffering, you can still do
    // `fn poll_decode(buf: B) -> Poll<Result<Frame, ParseError>>` to avoid
    // parsing the same data over and over again if there's not enough data?
    //
    // And, if you take the tokio approach, you'll often find yourself *allocating*
    // memory for a succuessful parse, but it's possible that there's not
    // enough data, you throw away all that allocated memory and report error,
    // which is a huge waste. To workaround that, typically you write a separate
    // parser that only checks if there's a whole frame to be parsed, hopefully not
    // allocating any memory on the way. And you'll be sad to find that you have
    // basically two piece of code that looks almost identical to each other, which
    // is basiclly what
    // `mini-redis` [does](https://github.com/tokio-rs/mini-redis/blob/tutorial/src/frame.rs#L66-L168)
    //
    // But back to buffering, it occurs to me that you don't need futures since
    // you already have a buffer instead of a `AsyncRead`. All you need to is to
    // somehow save the state of the parser:
    //
    // ```
    // struct Parser { /* ... */ }
    //
    // impl Parser {
    //     /// Returns `None` if not enough data,
    //     /// Returns `Some(Err(_))` if encountered invalid data
    //     fn poll_decode(&mut self, buf: B) -> Option<Result<Frame, ParseError>>
    // }
    // ```
    //
    // uhh, no, how do you even resume the state. do we want a generator, like async/await uses?
    // but that unfortunely could pessimise some yet IMO common cases where large majority of
    // `decode` will result in a succuessful parse.
    //
    // so, in all, is tokio's approach a pretty good trade-off? From my understanding,
    // probably yes.
    //
    // another interesting problem is that, what if in some point you want to unframe?
    // specifically if we currently have a stream of `FrameA`:
    //
    //   1. go from stream of `FrameA` to `FrameB`
    //   2. I don't want stream of `FrameA` anymore, I just want to underlaying `AsyncRead`
    //
    // The former is trivial IMO, just move the buffer to codec B and you're done.
    // The latter is a little bit trickly though, how do you put buffered bytes back to
    // the `AsyncRead`???
    //
    // This is not a problem with framing, it is a problem with buffering. In our proxy
    // case, this is trivial yet hacky to solve: just take the underlaying read buffer,
    // and write those bytes to the other side. In the end assert the write buffer is
    // empty.
    //
    // errors:
    //
    //   1. not enough data, Option
    //   2. invalid data, Result<Frame, DecodeError>
    //
    // in all, a nest of `Option` and `Result`. But which one is inside?
    // I think both is okay.
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

// todo: use thiserror
#[derive(Debug, Display, Error)]
pub enum FramedError {
    Io(std::io::Error),
    Decode(DecodeError),
}

impl From<std::io::Error> for FramedError {
    fn from(err: std::io::Error) -> Self {
        FramedError::Io(err)
    }
}

impl From<DecodeError> for FramedError {
    fn from(err: DecodeError) -> Self {
        FramedError::Decode(err)
    }
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
                        todo!();
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
