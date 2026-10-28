//! Desktop streams backed by a VNC probe device.
//!
//! A probe can't run an agent, so there's no capture backend to relay to.
//! Instead the device's owning *server* speaks RFB to it and re-encodes what it
//! sees into the desktop subsystem's own frame format. Reusing
//! [`DesktopStreamResponse`] as the response type means the client's decoder,
//! texture upload, and input mapping are shared with agent streams.
//!
//! Credentials never leave the server. The client sends only a device id, which
//! the server resolves against the probe subsystem's device registry.

use crate::session::{DesktopStreamInputEvent, DesktopStreamResponse};
use serde::{Deserialize, Serialize};

/// Request message for VNC probe streams.
#[derive(Serialize, Deserialize)]
pub enum VncStreamRequest {
    /// Start streaming the probe device with this id. The server looks the
    /// device's credentials up itself.
    Start { device_id: u64 },
    /// Requester is forwarding an input event to the device
    Input(DesktopStreamInputEvent),
    /// Requester wants to stop the stream
    Stop,
}

#[cfg(all(feature = "server", feature = "probe"))]
mod server {
    use super::*;
    use crate::session::{
        DesktopStreamColorMode, DesktopStreamCompressionMode, DesktopStreamOutputEvent,
        DesktopStreamPointerButton,
    };
    use anyhow::{Result, bail};
    use sandpolis_instance::network::StreamResponder;
    use sandpolis_macros::Stream;
    use sandpolis_probe::config::VncProbeConfig;
    use std::net::{IpAddr, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::RwLock;
    use tokio::sync::mpsc::Sender;
    use tracing::{debug, warn};

    /// How long to idle when the device sent nothing, roughly one frame at 60Hz.
    const IDLE_SLEEP: Duration = Duration::from_millis(16);

    /// Stream responder that mirrors a VNC device into the desktop subsystem.
    ///
    /// The `vnc` crate's client is blocking — `poll_event` is a `try_recv` over
    /// an internal reader thread — so the session lives on a dedicated OS thread
    /// rather than a tokio task, the same arrangement the agent capture backend
    /// uses for its non-`Send` `Capturer`.
    #[derive(Stream, Default)]
    pub struct VncStreamResponder {
        stop: Arc<AtomicBool>,
        input_tx: RwLock<Option<std::sync::mpsc::Sender<DesktopStreamInputEvent>>>,
    }

    impl StreamResponder for VncStreamResponder {
        type In = VncStreamRequest;
        type Out = DesktopStreamResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            match request {
                VncStreamRequest::Start { device_id } => {
                    // Ignore a duplicate Start: a session is already running.
                    if self.input_tx.read().await.is_some() {
                        warn!("VNC stream already running; ignoring Start");
                        return Ok(());
                    }

                    let Some(device) = sandpolis_probe::REGISTERED_DEVICES
                        .read()
                        .ok()
                        .and_then(|devices| devices.iter().find(|d| d.id.body() == device_id).cloned())
                    else {
                        warn!("VNC stream requested for unregistered device {device_id}");
                        let _ = sender.send(DesktopStreamResponse::Stopped).await;
                        return Ok(());
                    };
                    let Some(config) = device.device.vnc.clone() else {
                        warn!("Device {device_id} has no VNC configuration");
                        let _ = sender.send(DesktopStreamResponse::Stopped).await;
                        return Ok(());
                    };

                    self.stop.store(false, Ordering::SeqCst);
                    let (input_tx, input_rx) = std::sync::mpsc::channel();
                    *self.input_tx.write().await = Some(input_tx);

                    let ip = device.device.ip;
                    let stop = self.stop.clone();
                    std::thread::Builder::new()
                        .name("probe-vnc".into())
                        .spawn(move || {
                            if let Err(e) =
                                vnc_loop(config, ip, input_rx, stop, sender.clone())
                            {
                                warn!(error = %e, "VNC probe stream failed");
                            }
                            let _ = sender.blocking_send(DesktopStreamResponse::Stopped);
                        })?;
                }
                VncStreamRequest::Input(event) => {
                    if let Some(tx) = self.input_tx.read().await.as_ref() {
                        let _ = tx.send(event);
                    }
                }
                VncStreamRequest::Stop => {
                    self.stop.store(true, Ordering::SeqCst);
                    // Drop the input sender so the thread exits and a later
                    // Start can begin a fresh session.
                    *self.input_tx.write().await = None;
                }
            }
            Ok(())
        }

        fn unreliable(response: &Self::Out) -> bool {
            crate::session::is_frame(response)
        }
    }

    impl Drop for VncStreamResponder {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    /// A BGRX framebuffer the device's partial updates are composited into.
    ///
    /// RFB sends dirty rectangles, but [`decode_frame`](crate::session::decode_frame)
    /// wants a whole image and the client replaces its texture wholesale — so the
    /// full picture has to be reassembled here.
    struct Framebuffer {
        width: usize,
        height: usize,
        /// `width * height * 4` bytes of BGRX.
        pixels: Vec<u8>,
    }

    impl Framebuffer {
        fn new(width: u16, height: u16) -> Self {
            let (width, height) = (width as usize, height as usize);
            Self {
                width,
                height,
                pixels: vec![0; width * height * 4],
            }
        }

        /// Copy an incoming rectangle of BGRX pixels into place, ignoring any
        /// part of it that falls outside the current framebuffer.
        fn put(&mut self, rect: vnc::Rect, data: &[u8]) {
            let rect_w = rect.width as usize;
            for row in 0..rect.height as usize {
                let y = rect.top as usize + row;
                if y >= self.height {
                    break;
                }
                let visible = rect_w.min(self.width.saturating_sub(rect.left as usize));
                if visible == 0 {
                    break;
                }
                let src = row * rect_w * 4;
                let dst = (y * self.width + rect.left as usize) * 4;
                let Some(src_row) = data.get(src..src + visible * 4) else {
                    break;
                };
                self.pixels[dst..dst + visible * 4].copy_from_slice(src_row);
            }
        }

        /// Move a rectangle within the framebuffer (the `CopyRect` encoding).
        fn copy(&mut self, src: vnc::Rect, dst: vnc::Rect) {
            let width = (src.width as usize)
                .min(self.width.saturating_sub(src.left as usize))
                .min(self.width.saturating_sub(dst.left as usize));
            let height = (src.height as usize)
                .min(self.height.saturating_sub(src.top as usize))
                .min(self.height.saturating_sub(dst.top as usize));
            if width == 0 || height == 0 {
                return;
            }
            // Copy row-wise into a scratch buffer first: source and destination
            // may overlap, and downward moves would otherwise smear.
            let mut scratch = vec![0u8; width * height * 4];
            for row in 0..height {
                let from = ((src.top as usize + row) * self.width + src.left as usize) * 4;
                scratch[row * width * 4..(row + 1) * width * 4]
                    .copy_from_slice(&self.pixels[from..from + width * 4]);
            }
            for row in 0..height {
                let to = ((dst.top as usize + row) * self.width + dst.left as usize) * 4;
                self.pixels[to..to + width * 4]
                    .copy_from_slice(&scratch[row * width * 4..(row + 1) * width * 4]);
            }
        }

        /// Pack the framebuffer as zstd-compressed RGB888, which is what the
        /// agent path already sends and the client already decodes.
        fn encode(&self) -> Result<Vec<u8>> {
            let mut rgb = Vec::with_capacity(self.width * self.height * 3);
            for pixel in self.pixels.chunks_exact(4) {
                rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
            Ok(zstd::encode_all(rgb.as_slice(), 1)?)
        }
    }

    /// Connect to the device and mirror it until stopped.
    fn vnc_loop(
        config: VncProbeConfig,
        ip: IpAddr,
        input_rx: std::sync::mpsc::Receiver<DesktopStreamInputEvent>,
        stop: Arc<AtomicBool>,
        sender: Sender<DesktopStreamResponse>,
    ) -> Result<()> {
        // The config's host wins, but a device registered with only an IP is
        // perfectly usable.
        let host = if config.host.is_empty() {
            ip.to_string()
        } else {
            config.host.clone()
        };
        let port = config.port.unwrap_or(5900);

        let stream = TcpStream::connect((host.as_str(), port))?;
        let password = config.password.clone();
        let mut client = vnc::Client::from_tcp_stream(stream, true, move |methods| {
            // VNC passwords are truncated to 8 bytes; the crate applies the DES
            // challenge itself.
            let offers_password = methods
                .iter()
                .any(|method| matches!(method, vnc::client::AuthMethod::Password));
            if offers_password
                && let Some(password) = password.as_deref()
            {
                let mut key = [0u8; 8];
                for (slot, byte) in key.iter_mut().zip(password.bytes()) {
                    *slot = byte;
                }
                return Some(vnc::client::AuthChoice::Password(key));
            }
            Some(vnc::client::AuthChoice::None)
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        // 32bpp true colour with these shifts lands as BGRX in memory, which is
        // one swizzle away from the RGB888 the wire format wants.
        client
            .set_format(vnc::PixelFormat {
                bits_per_pixel: 32,
                depth: 24,
                big_endian: false,
                true_colour: true,
                red_max: 255,
                green_max: 255,
                blue_max: 255,
                red_shift: 16,
                green_shift: 8,
                blue_shift: 0,
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        client
            .set_encodings(&[
                vnc::Encoding::Raw,
                vnc::Encoding::CopyRect,
                vnc::Encoding::DesktopSize,
            ])
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let (width, height) = client.size();
        if width == 0 || height == 0 {
            bail!("device reported a {width}x{height} framebuffer");
        }
        debug!(host = %host, width, height, "VNC probe stream opened");
        if sender
            .blocking_send(DesktopStreamResponse::Started {
                width: width as i32,
                height: height as i32,
            })
            .is_err()
        {
            return Ok(());
        }

        let mut framebuffer = Framebuffer::new(width, height);
        let mut buttons = 0u8;
        let mut clipboard: Option<Vec<u8>> = None;
        // The first update must be non-incremental to get a full picture.
        request_update(&mut client, &framebuffer, false)?;

        while !stop.load(Ordering::SeqCst) {
            apply_input(&mut client, &input_rx, &mut buttons, &framebuffer)?;

            let mut idle = true;
            let mut frame_ready = false;
            while let Some(event) = client.poll_event() {
                idle = false;
                match event {
                    vnc::client::Event::PutPixels(rect, data) => framebuffer.put(rect, &data),
                    vnc::client::Event::CopyPixels { src, dst } => framebuffer.copy(src, dst),
                    vnc::client::Event::Resize(width, height) => {
                        framebuffer = Framebuffer::new(width, height);
                    }
                    vnc::client::Event::Clipboard(text) => {
                        clipboard = Some(text.into_bytes());
                    }
                    // One frame per update batch, not one per rectangle.
                    vnc::client::Event::EndOfFrame => frame_ready = true,
                    vnc::client::Event::Disconnected(error) => {
                        if let Some(error) = error {
                            warn!(error = %error, "VNC device disconnected");
                        }
                        return Ok(());
                    }
                    // Cursor shape, colour maps and the bell have nowhere to go
                    // in the desktop subsystem's frame format.
                    vnc::client::Event::SetCursor { .. }
                    | vnc::client::Event::SetColourMap { .. }
                    | vnc::client::Event::Bell => {}
                }
            }

            if frame_ready {
                let event = DesktopStreamOutputEvent {
                    width: Some(framebuffer.width as i32),
                    height: Some(framebuffer.height as i32),
                    dest_x: Some(0),
                    dest_y: Some(0),
                    source_x: Some(0),
                    source_y: Some(0),
                    color_mode: DesktopStreamColorMode::Rgb888,
                    compression_mode: DesktopStreamCompressionMode::Zstd,
                    pixel_data: Some(framebuffer.encode()?),
                    clipboard: clipboard.take(),
                };
                if sender
                    .blocking_send(DesktopStreamResponse::Frame(event))
                    .is_err()
                {
                    break;
                }
                request_update(&mut client, &framebuffer, true)?;
            } else if idle {
                std::thread::sleep(IDLE_SLEEP);
            }
        }

        debug!(host = %host, "VNC probe stream closed");
        Ok(())
    }

    /// Ask the device for an update covering the whole framebuffer.
    fn request_update(
        client: &mut vnc::Client,
        framebuffer: &Framebuffer,
        incremental: bool,
    ) -> Result<()> {
        client
            .request_update(
                vnc::Rect {
                    left: 0,
                    top: 0,
                    width: framebuffer.width as u16,
                    height: framebuffer.height as u16,
                },
                incremental,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Drain queued input events and replay them onto the device.
    fn apply_input(
        client: &mut vnc::Client,
        input_rx: &std::sync::mpsc::Receiver<DesktopStreamInputEvent>,
        buttons: &mut u8,
        framebuffer: &Framebuffer,
    ) -> Result<()> {
        for event in input_rx.try_iter() {
            if let (Some(x), Some(y)) = (event.pointer_x, event.pointer_y) {
                // RFB carries the button mask with every pointer event, so
                // presses and releases have to be tracked across events.
                if let Some(button) = event.pointer_pressed {
                    *buttons |= button_mask(button);
                }
                if let Some(button) = event.pointer_released {
                    *buttons &= !button_mask(button);
                }
                let x = x.clamp(0, framebuffer.width.saturating_sub(1) as i32) as u16;
                let y = y.clamp(0, framebuffer.height.saturating_sub(1) as i32) as u16;
                client
                    .send_pointer_event(*buttons, x, y)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }

            // `key_typed` is ignored: press and release already cover it.
            for (character, down) in [(event.key_pressed, true), (event.key_released, false)] {
                let Some(character) = character else {
                    continue;
                };
                client
                    .send_key_event(down, keysym(character))
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
        }
        Ok(())
    }

    /// The RFB button-mask bit for a pointer button.
    fn button_mask(button: DesktopStreamPointerButton) -> u8 {
        match button {
            DesktopStreamPointerButton::Primary => 1 << 0,
            DesktopStreamPointerButton::Middle => 1 << 1,
            DesktopStreamPointerButton::Secondary => 1 << 2,
            DesktopStreamPointerButton::Back => 1 << 3,
            DesktopStreamPointerButton::Forward => 1 << 4,
        }
    }

    /// Map a character to an X11 keysym, which is what RFB key events carry.
    ///
    /// Latin-1 is its own keysym range; everything else uses the Unicode
    /// escape. Non-printing keys (Enter, Backspace, arrows) never get here —
    /// [`DesktopStreamInputEvent`] only carries characters, the same limit the
    /// agent path has.
    fn keysym(character: char) -> u32 {
        let code = character as u32;
        if code < 0x100 {
            code
        } else {
            0x0100_0000 + code
        }
    }
}

#[cfg(all(feature = "server", feature = "probe"))]
pub use server::VncStreamResponder;

#[cfg(all(feature = "client", feature = "probe"))]
mod client {
    use super::*;
    use crate::session::{DesktopStreamEvent, decode_frame};
    use anyhow::Result;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_macros::Stream;
    use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// Client side of a VNC probe stream. Identical in shape to
    /// [`DesktopStreamRequester`](crate::session::DesktopStreamRequester) so both
    /// feed the same viewer.
    #[derive(Stream)]
    pub struct VncStreamRequester {
        events: UnboundedSender<DesktopStreamEvent>,
    }

    impl VncStreamRequester {
        /// Construct a requester paired with the receiver the GUI drains.
        pub fn channel() -> (Self, UnboundedReceiver<DesktopStreamEvent>) {
            let (events, rx) = unbounded_channel();
            (Self { events }, rx)
        }
    }

    impl StreamRequester for VncStreamRequester {
        type In = DesktopStreamResponse;
        type Out = VncStreamRequest;

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
}

#[cfg(all(feature = "client", feature = "probe"))]
pub use client::VncStreamRequester;
