//! The accept side of a connection: a websocket `axum` handed us after an
//! upgrade. Driving it is [`super::socket`]'s job; this is only the message
//! type it speaks.

use super::socket::{Frame, SocketMessage};
use axum::extract::ws::Message;

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
            Message::Text(_) | Message::Close(_) => Frame::Other,
        }
    }
}
