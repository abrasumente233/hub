#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
