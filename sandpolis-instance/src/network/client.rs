//! The dialing side of a connection: a websocket `reqwest` upgraded for us.
//! Driving it is [`super::socket`]'s job — the same loop the accept side runs,
//! so streams are symmetric across the connection — and this is only the
//! message type it speaks.

use super::socket::{Frame, SocketMessage};
use reqwest_websocket::Message;

impl SocketMessage for Message {
    fn binary(payload: Vec<u8>) -> Self {
        Message::Binary(payload.into())
    }

    fn ping(payload: Vec<u8>) -> Self {
        Message::Ping(payload.into())
    }

    fn pong(payload: Vec<u8>) -> Self {
        Message::Pong(payload.into())
    }

    fn frame(&self) -> Frame<'_> {
        match self {
            Message::Binary(payload) => Frame::Binary(payload),
            Message::Ping(payload) => Frame::Ping(payload),
            Message::Pong(payload) => Frame::Pong(payload),
            Message::Text(_) | Message::Close { .. } => Frame::Other,
        }
    }
}
