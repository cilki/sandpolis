use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopStreamColorMode {
    /// Each pixel encoded in three bytes
    Rgb888,
    /// Each pixel encoded in two bytes
    Rgb565,
    /// Each pixel encoded in one byte
    Rgb332,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopStreamCompressionMode {
    None,
    Zstd,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopStreamPointerButton {
    Primary,
    Middle,
    Secondary,
    Back,
    Forward,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DesktopStreamInputEvent {
    /// Indicates a key was pressed
    pub key_pressed: Option<char>,

    /// Indicates a key was released
    pub key_released: Option<char>,

    /// Indicates a key was typed
    pub key_typed: Option<char>,

    /// Indicates a pointing device was pressed
    pub pointer_pressed: Option<DesktopStreamPointerButton>,

    /// Indicates a pointing device was released
    pub pointer_released: Option<DesktopStreamPointerButton>,

    /// The X coordinate of the pointer
    pub pointer_x: Option<i32>,

    /// The Y coordinate of the pointer
    pub pointer_y: Option<i32>,

    /// Clipboard data
    pub clipboard: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DesktopStreamOutputEvent {
    /// The width of the destination block in pixels
    pub width: Option<i32>,

    /// The height of the destination block in pixels
    pub height: Option<i32>,

    /// The X coordinate of the destination block's top left corner
    pub dest_x: Option<i32>,

    /// The Y coordinate of the destination block's top left corner
    pub dest_y: Option<i32>,

    /// The X coordinate of the source block's top left corner
    pub source_x: Option<i32>,

    /// The Y coordinate of the source block's top left corner
    pub source_y: Option<i32>,

    /// The color mode used to encode `pixel_data`
    pub color_mode: DesktopStreamColorMode,

    /// The compression mode applied to `pixel_data`
    pub compression_mode: DesktopStreamCompressionMode,

    /// The pixel data encoded according to the session's parameters
    pub pixel_data: Option<Vec<u8>>,

    /// Clipboard data
    pub clipboard: Option<Vec<u8>>,
}

/// Request message for desktop streams.
#[derive(Serialize, Deserialize)]
pub enum DesktopStreamRequest {
    /// Requester wants to start streaming a desktop
    Start {
        /// The desktop to capture (matches `DesktopData::name`)
        desktop_uuid: String,

        /// How pixels should be encoded
        color_mode: DesktopStreamColorMode,

        /// How encoded pixels should be compressed
        compression_mode: DesktopStreamCompressionMode,
    },
    /// Requester is forwarding an input event to the desktop
    Input(DesktopStreamInputEvent),
    /// Requester wants to stop the stream
    Stop,
}

/// Response message for desktop streams.
#[derive(Serialize, Deserialize)]
pub enum DesktopStreamResponse {
    /// Stream has started; reports the captured desktop dimensions
    Started { width: i32, height: i32 },
    /// A captured frame
    Frame(DesktopStreamOutputEvent),
    /// Stream has stopped
    Stopped,
}

/// Whether `response` is a frame, which every desktop stream responder sends
/// unreliably: a frame is worth nothing once the next exists, so it goes as
/// datagrams that congestion drops rather than queueing behind the frames
/// before it. Every frame is the whole screen, so a lost one is made good by
/// the next. `Started` and `Stopped` stay reliable.
#[cfg(any(feature = "agent", all(feature = "server", feature = "probe")))]
pub(crate) fn is_frame(response: &DesktopStreamResponse) -> bool {
    matches!(response, DesktopStreamResponse::Frame(_))
}

#[cfg(feature = "agent")]
mod agent {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::network::StreamResponder;
    use sandpolis_macros::Stream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::RwLock;
    use tokio::sync::mpsc::Sender;
    use tracing::{debug, trace, warn};

    /// Stream that captures a desktop and applies remote input.
    #[derive(Stream, Default)]
    pub struct DesktopStreamResponder {
        stop: Arc<AtomicBool>,
        input_tx: RwLock<Option<std::sync::mpsc::Sender<DesktopStreamInputEvent>>>,
    }

    impl StreamResponder for DesktopStreamResponder {
        type In = DesktopStreamRequest;
        type Out = DesktopStreamResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            match request {
                DesktopStreamRequest::Start {
                    desktop_uuid,
                    color_mode,
                    compression_mode,
                } => {
                    // Ignore a duplicate Start: a session is already running.
                    if self.input_tx.read().await.is_some() {
                        warn!("Desktop stream already running; ignoring Start");
                        return Ok(());
                    }
                    self.stop.store(false, Ordering::SeqCst);

                    // Spawn the input thread (owns the non-Send `Enigo` instance).
                    let (input_tx, input_rx) = std::sync::mpsc::channel();
                    *self.input_tx.write().await = Some(input_tx);
                    let input_stop = self.stop.clone();
                    std::thread::Builder::new()
                        .name("desktop-input".into())
                        .spawn(move || input_loop(input_rx, input_stop))?;

                    // Spawn the capture thread (owns the non-Send `Capturer`).
                    let capture_stop = self.stop.clone();
                    std::thread::Builder::new()
                        .name("desktop-capture".into())
                        .spawn(move || {
                            if let Err(e) = capture_loop(
                                desktop_uuid,
                                color_mode,
                                compression_mode,
                                capture_stop,
                                sender,
                            ) {
                                warn!(error = %e, "Desktop capture loop failed");
                            }
                        })?;
                }
                DesktopStreamRequest::Input(event) => {
                    if let Some(tx) = self.input_tx.read().await.as_ref() {
                        let _ = tx.send(event);
                    }
                }
                DesktopStreamRequest::Stop => {
                    self.stop.store(true, Ordering::SeqCst);
                    // Drop the input sender so the input thread exits and a later
                    // Start can begin a fresh session.
                    *self.input_tx.write().await = None;
                }
            }
            Ok(())
        }

        fn unreliable(response: &Self::Out) -> bool {
            is_frame(response)
        }
    }

    impl Drop for DesktopStreamResponder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Owns a `Capturer` and forwards encoded frames until stopped.
    fn capture_loop(
        desktop_uuid: String,
        color_mode: DesktopStreamColorMode,
        compression_mode: DesktopStreamCompressionMode,
        stop: Arc<AtomicBool>,
        sender: Sender<DesktopStreamResponse>,
    ) -> Result<()> {
        use crate::capture::{Frame, TraitCapturer, TraitPixelBuffer};

        let display = crate::agent::find_display(&desktop_uuid)?;
        let mut capturer = crate::capture::Capturer::new(display)?;
        let width = capturer.width() as i32;
        let height = capturer.height() as i32;

        if sender
            .blocking_send(DesktopStreamResponse::Started { width, height })
            .is_err()
        {
            return Ok(());
        }

        while !stop.load(Ordering::SeqCst) {
            match capturer.frame(Duration::from_millis(0)) {
                Ok(Frame::PixelBuffer(buffer)) => {
                    let pixel_data = encode_frame(
                        buffer.data(),
                        buffer.width(),
                        buffer.height(),
                        &buffer.stride(),
                        buffer.pixfmt(),
                        color_mode,
                        compression_mode,
                    );

                    let event = DesktopStreamOutputEvent {
                        width: Some(buffer.width() as i32),
                        height: Some(buffer.height() as i32),
                        dest_x: Some(0),
                        dest_y: Some(0),
                        source_x: Some(0),
                        source_y: Some(0),
                        color_mode,
                        compression_mode,
                        pixel_data: Some(pixel_data),
                        clipboard: None,
                    };

                    if sender
                        .blocking_send(DesktopStreamResponse::Frame(event))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Frame::Texture(_)) => {
                    trace!("Skipping texture frame (unsupported)");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No new frame available yet.
                    std::thread::sleep(Duration::from_millis(16));
                }
                Err(e) => {
                    warn!(error = %e, "Frame capture error");
                    break;
                }
            }
        }

        let _ = sender.blocking_send(DesktopStreamResponse::Stopped);
        debug!("Desktop capture loop ended");
        Ok(())
    }

    /// Convert a captured BGRA/RGBA frame into the requested color mode and
    /// optionally compress it.
    fn encode_frame(
        data: &[u8],
        width: usize,
        height: usize,
        stride: &[usize],
        pixfmt: crate::capture::Pixfmt,
        color_mode: DesktopStreamColorMode,
        compression_mode: DesktopStreamCompressionMode,
    ) -> Vec<u8> {
        let mut out = match color_mode {
            DesktopStreamColorMode::Rgb888 => Vec::with_capacity(width * height * 3),
            DesktopStreamColorMode::Rgb565 => Vec::with_capacity(width * height * 2),
            DesktopStreamColorMode::Rgb332 => Vec::with_capacity(width * height),
        };

        crate::agent::for_each_rgb(data, width, height, stride, pixfmt, |r, g, b| {
            match color_mode {
                DesktopStreamColorMode::Rgb888 => {
                    out.extend_from_slice(&[r, g, b]);
                }
                DesktopStreamColorMode::Rgb565 => {
                    let v: u16 =
                        (((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                DesktopStreamColorMode::Rgb332 => {
                    out.push((r & 0xE0) | ((g & 0xE0) >> 3) | (b >> 6));
                }
            }
        });

        match compression_mode {
            DesktopStreamCompressionMode::None => out,
            DesktopStreamCompressionMode::Zstd => {
                zstd::encode_all(out.as_slice(), 1).unwrap_or(out)
            }
        }
    }

    /// Owns an `Enigo` instance and applies queued input events.
    fn input_loop(
        rx: std::sync::mpsc::Receiver<DesktopStreamInputEvent>,
        stop: Arc<AtomicBool>,
    ) {
        use crate::input::{Enigo, Key, KeyboardControllable, MouseButton, MouseControllable};

        fn map_button(button: DesktopStreamPointerButton) -> MouseButton {
            match button {
                DesktopStreamPointerButton::Primary => MouseButton::Left,
                DesktopStreamPointerButton::Middle => MouseButton::Middle,
                DesktopStreamPointerButton::Secondary => MouseButton::Right,
                DesktopStreamPointerButton::Back => MouseButton::Back,
                DesktopStreamPointerButton::Forward => MouseButton::Forward,
            }
        }

        let mut enigo = Enigo::new();
        while !stop.load(Ordering::SeqCst) {
            let event = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

            if let (Some(x), Some(y)) = (event.pointer_x, event.pointer_y) {
                enigo.mouse_move_to(x, y);
            }
            if let Some(button) = event.pointer_pressed {
                let _ = enigo.mouse_down(map_button(button));
            }
            if let Some(button) = event.pointer_released {
                enigo.mouse_up(map_button(button));
            }
            if let Some(c) = event.key_pressed {
                let _ = enigo.key_down(Key::Layout(c));
            }
            if let Some(c) = event.key_released {
                enigo.key_up(Key::Layout(c));
            }
            if let Some(c) = event.key_typed {
                enigo.key_click(Key::Layout(c));
            }
        }
        debug!("Desktop input loop ended");
    }
}

#[cfg(feature = "agent")]
pub use agent::DesktopStreamResponder;

#[cfg(feature = "client")]
mod client {
    use super::*;
    use anyhow::{Result, bail};
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_macros::Stream;
    use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// A decoded RGBA8 frame ready to upload to a GUI texture.
    #[derive(Clone)]
    pub struct DesktopFrame {
        pub width: u32,
        pub height: u32,
        /// Tightly packed RGBA8 (`width * height * 4` bytes).
        pub rgba: Vec<u8>,
    }

    /// Events surfaced to the GUI as a desktop stream progresses.
    pub enum DesktopStreamEvent {
        Started { width: u32, height: u32 },
        Frame(DesktopFrame),
        Stopped,
    }

    /// Client side of a desktop stream: decodes incoming frames and forwards
    /// them to the GUI through an unbounded channel.
    #[derive(Stream)]
    pub struct DesktopStreamRequester {
        events: UnboundedSender<DesktopStreamEvent>,
    }

    impl DesktopStreamRequester {
        /// Construct a requester paired with the receiver the GUI drains.
        pub fn channel() -> (Self, UnboundedReceiver<DesktopStreamEvent>) {
            let (events, rx) = unbounded_channel();
            (Self { events }, rx)
        }
    }

    impl StreamRequester for DesktopStreamRequester {
        type In = DesktopStreamResponse;
        type Out = DesktopStreamRequest;

        async fn new(initial: Self::Out, tx: Sender<Self::Out>) -> Result<Self> {
            tx.send(initial).await?;
            // The GUI-facing constructor is `channel()`; this trait path has no
            // receiver attached, so decoded events are discarded.
            let (events, _rx) = unbounded_channel();
            Ok(Self { events })
        }

        async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
            let event = match response {
                DesktopStreamResponse::Started { width, height } => DesktopStreamEvent::Started {
                    width: width.max(0) as u32,
                    height: height.max(0) as u32,
                },
                DesktopStreamResponse::Frame(frame) => {
                    DesktopStreamEvent::Frame(decode_frame(&frame)?)
                }
                DesktopStreamResponse::Stopped => DesktopStreamEvent::Stopped,
            };
            // GUI receiver may be gone (controller closed); dropping is fine.
            let _ = self.events.send(event);
            Ok(())
        }
    }

    /// Decode a captured frame (reverse of the agent's `encode_frame`) into RGBA8.
    pub fn decode_frame(event: &DesktopStreamOutputEvent) -> Result<DesktopFrame> {
        let width = event.width.unwrap_or(0).max(0) as usize;
        let height = event.height.unwrap_or(0).max(0) as usize;
        let Some(pixel_data) = event.pixel_data.as_ref() else {
            bail!("Frame has no pixel data");
        };

        // Reverse compression.
        let raw = match event.compression_mode {
            DesktopStreamCompressionMode::None => std::borrow::Cow::Borrowed(pixel_data.as_slice()),
            DesktopStreamCompressionMode::Zstd => {
                std::borrow::Cow::Owned(zstd::decode_all(pixel_data.as_slice())?)
            }
        };

        // Reverse color encoding into RGBA8.
        let mut rgba = vec![0u8; width * height * 4];
        match event.color_mode {
            DesktopStreamColorMode::Rgb888 => {
                if raw.len() < width * height * 3 {
                    bail!("Truncated Rgb888 frame");
                }
                for (i, px) in raw.chunks_exact(3).take(width * height).enumerate() {
                    let o = i * 4;
                    rgba[o] = px[0];
                    rgba[o + 1] = px[1];
                    rgba[o + 2] = px[2];
                    rgba[o + 3] = 255;
                }
            }
            DesktopStreamColorMode::Rgb565 => {
                if raw.len() < width * height * 2 {
                    bail!("Truncated Rgb565 frame");
                }
                for (i, px) in raw.chunks_exact(2).take(width * height).enumerate() {
                    let v = u16::from_le_bytes([px[0], px[1]]);
                    let r = ((v >> 11) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x3f) as u8;
                    let b = (v & 0x1f) as u8;
                    let o = i * 4;
                    // Expand 5/6/5-bit channels to 8 bits by bit replication.
                    rgba[o] = (r << 3) | (r >> 2);
                    rgba[o + 1] = (g << 2) | (g >> 4);
                    rgba[o + 2] = (b << 3) | (b >> 2);
                    rgba[o + 3] = 255;
                }
            }
            DesktopStreamColorMode::Rgb332 => {
                if raw.len() < width * height {
                    bail!("Truncated Rgb332 frame");
                }
                for (i, &px) in raw.iter().take(width * height).enumerate() {
                    let r = (px >> 5) & 0x7;
                    let g = (px >> 2) & 0x7;
                    let b = px & 0x3;
                    let o = i * 4;
                    rgba[o] = (r << 5) | (r << 2) | (r >> 1);
                    rgba[o + 1] = (g << 5) | (g << 2) | (g >> 1);
                    rgba[o + 2] = (b << 6) | (b << 4) | (b << 2) | b;
                    rgba[o + 3] = 255;
                }
            }
        }

        Ok(DesktopFrame {
            width: width as u32,
            height: height as u32,
            rgba,
        })
    }
}

#[cfg(feature = "client")]
pub use client::{DesktopFrame, DesktopStreamEvent, DesktopStreamRequester, decode_frame};

#[cfg(test)]
mod test_desktop_session {
    use super::*;

    #[test]
    fn test_request_roundtrip() {
        let req = DesktopStreamRequest::Start {
            desktop_uuid: "display-0".into(),
            color_mode: DesktopStreamColorMode::Rgb888,
            compression_mode: DesktopStreamCompressionMode::None,
        };
        let bytes = serde_cbor::to_vec(&req).unwrap();
        let decoded: DesktopStreamRequest = serde_cbor::from_slice(&bytes).unwrap();
        assert!(matches!(decoded, DesktopStreamRequest::Start { .. }));
    }

    #[test]
    fn test_input_event_roundtrip() {
        let event = DesktopStreamInputEvent {
            key_pressed: None,
            key_released: None,
            key_typed: Some('a'),
            pointer_pressed: Some(DesktopStreamPointerButton::Primary),
            pointer_released: None,
            pointer_x: Some(100),
            pointer_y: Some(200),
            clipboard: None,
        };
        let req = DesktopStreamRequest::Input(event);
        let bytes = serde_cbor::to_vec(&req).unwrap();
        let decoded: DesktopStreamRequest = serde_cbor::from_slice(&bytes).unwrap();
        match decoded {
            DesktopStreamRequest::Input(e) => {
                assert_eq!(e.key_typed, Some('a'));
                assert_eq!(e.pointer_x, Some(100));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[cfg(feature = "client")]
    #[test]
    fn test_decode_rgb888_frame() {
        // Two pixels: red then green, uncompressed Rgb888.
        let event = DesktopStreamOutputEvent {
            width: Some(2),
            height: Some(1),
            dest_x: Some(0),
            dest_y: Some(0),
            source_x: Some(0),
            source_y: Some(0),
            color_mode: DesktopStreamColorMode::Rgb888,
            compression_mode: DesktopStreamCompressionMode::None,
            pixel_data: Some(vec![255, 0, 0, 0, 255, 0]),
            clipboard: None,
        };
        let frame = decode_frame(&event).unwrap();
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[cfg(feature = "client")]
    #[test]
    fn test_decode_zstd_frame() {
        let payload = zstd::encode_all([10u8, 20, 30].as_slice(), 1).unwrap();
        let event = DesktopStreamOutputEvent {
            width: Some(1),
            height: Some(1),
            dest_x: Some(0),
            dest_y: Some(0),
            source_x: Some(0),
            source_y: Some(0),
            color_mode: DesktopStreamColorMode::Rgb888,
            compression_mode: DesktopStreamCompressionMode::Zstd,
            pixel_data: Some(payload),
            clipboard: None,
        };
        let frame = decode_frame(&event).unwrap();
        assert_eq!(frame.rgba, vec![10, 20, 30, 255]);
    }
}
