use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct DesktopScreenshotRequest {
    /// The desktop to capture (matches `DesktopData::name`)
    pub desktop_uuid: String,
    // TODO geometry
}

#[derive(Serialize, Deserialize)]
pub enum DesktopScreenshotResponse {
    /// PNG-encoded screenshot
    Ok(Vec<u8>),
    /// The requested desktop could not be captured
    Failed,
}

#[cfg(feature = "agent")]
mod agent {
    use super::*;
    use anyhow::{Result, bail};
    use sandpolis_instance::network::StreamResponder;
    use sandpolis_macros::Stream;
    use std::time::Duration;
    use tokio::sync::mpsc::Sender;
    use tracing::warn;

    /// Stream that captures a single screenshot and then terminates.
    #[derive(Stream, Default)]
    pub struct DesktopScreenshotResponder;

    impl StreamResponder for DesktopScreenshotResponder {
        type In = DesktopScreenshotRequest;
        type Out = DesktopScreenshotResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            // Capture + encode on a blocking thread since the capture module types are not Send.
            let response = tokio::task::spawn_blocking(move || capture_png(&request.desktop_uuid))
                .await?
                .map(DesktopScreenshotResponse::Ok)
                .unwrap_or_else(|e| {
                    warn!(error = %e, "Screenshot capture failed");
                    DesktopScreenshotResponse::Failed
                });

            sender.send(response).await?;
            Ok(())
        }
    }

    /// Capture one frame from the named display and encode it as PNG.
    fn capture_png(desktop_uuid: &str) -> Result<Vec<u8>> {
        use crate::capture::{Frame, TraitCapturer, TraitPixelBuffer};

        let display = crate::agent::find_display(desktop_uuid)?;
        let mut capturer = crate::capture::Capturer::new(display)?;

        // Retry a few times to skip the initial empty frames.
        for _ in 0..30 {
            match capturer.frame(Duration::from_millis(0)) {
                Ok(Frame::PixelBuffer(buffer)) => {
                    return encode_png(
                        buffer.data(),
                        buffer.width(),
                        buffer.height(),
                        &buffer.stride(),
                        buffer.pixfmt(),
                    );
                }
                Ok(Frame::Texture(_)) => bail!("Texture frames are unsupported"),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(16));
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!("Timed out waiting for a frame");
    }

    fn encode_png(
        data: &[u8],
        width: usize,
        height: usize,
        stride: &[usize],
        pixfmt: crate::capture::Pixfmt,
    ) -> Result<Vec<u8>> {
        let mut rgba = Vec::with_capacity(width * height * 4);
        crate::agent::for_each_rgb(data, width, height, stride, pixfmt, |r, g, b| {
            rgba.extend_from_slice(&[r, g, b, 255]);
        });

        let buffer = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
            .ok_or_else(|| anyhow::anyhow!("Failed to build image buffer"))?;

        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(buffer).write_to(&mut png, image::ImageFormat::Png)?;
        Ok(png.into_inner())
    }
}

#[cfg(feature = "agent")]
pub use agent::DesktopScreenshotResponder;

#[cfg(feature = "client")]
mod client {
    use super::*;
    use crate::session::DesktopFrame;
    use anyhow::Result;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_macros::Stream;
    use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// Outcome of a screenshot request surfaced to the client. The `Ok` variant
    /// carries the agent's PNG bytes verbatim; the CLI writes them directly and
    /// the GUI decodes them via [`decode_png`] only when it needs a texture.
    pub enum DesktopScreenshotResult {
        Ok(Vec<u8>),
        Failed,
    }

    /// Client side of a one-shot screenshot: forwards the returned PNG bytes.
    #[derive(Stream)]
    pub struct DesktopScreenshotRequester {
        result: UnboundedSender<DesktopScreenshotResult>,
    }

    impl DesktopScreenshotRequester {
        /// Construct a requester paired with the receiver the GUI drains.
        pub fn channel() -> (Self, UnboundedReceiver<DesktopScreenshotResult>) {
            let (result, rx) = unbounded_channel();
            (Self { result }, rx)
        }
    }

    impl StreamRequester for DesktopScreenshotRequester {
        type In = DesktopScreenshotResponse;
        type Out = DesktopScreenshotRequest;

        async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
            let outcome = match response {
                DesktopScreenshotResponse::Ok(png) => DesktopScreenshotResult::Ok(png),
                DesktopScreenshotResponse::Failed => DesktopScreenshotResult::Failed,
            };
            let _ = self.result.send(outcome);
            Ok(())
        }
    }

    /// Decode PNG bytes into an RGBA8 `DesktopFrame`.
    pub fn decode_png(png: &[u8]) -> Result<DesktopFrame> {
        let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgba8();
        let (width, height) = img.dimensions();
        Ok(DesktopFrame {
            width,
            height,
            rgba: img.into_raw(),
        })
    }
}

#[cfg(feature = "client")]
pub use client::{DesktopScreenshotRequester, DesktopScreenshotResult, decode_png};
