//! Desktop streams backed by an RDP probe device.
//!
//! A probe can't run an agent, so there's no capture backend to relay to.
//! Instead the device's owning *server* speaks RDP to it (via the IronRDP
//! crates) and re-encodes what it decodes into the desktop subsystem's own frame
//! format. Reusing [`DesktopStreamResponse`] as the response type means the
//! client's decoder, texture upload, and input mapping are shared with agent and
//! VNC streams alike.
//!
//! Credentials never leave the server. The client sends only a device id, which
//! the server resolves against the probe subsystem's device registry.
//!
//! The first cut negotiates TLS security rather than NLA/CredSSP: it keeps the
//! sspi CredSSP path out of the critical path and matches the servers the test
//! container brings up. Hosts that require Network Level Authentication (most
//! default Windows configurations) will need `enable_credssp` turned on.

use crate::session::{DesktopStreamInputEvent, DesktopStreamResponse};
use serde::{Deserialize, Serialize};

/// Request message for RDP probe streams.
#[derive(Serialize, Deserialize)]
pub enum RdpStreamRequest {
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
    use ironrdp_connector::{ClientConnector, Config, Credentials, DesktopSize};
    use ironrdp_graphics::image_processing::PixelFormat;
    use ironrdp_input::{Database, MouseButton, MousePosition, Operation};
    use ironrdp_pdu::gcc::{ConnectionType, KeyboardType};
    use ironrdp_pdu::rdp::capability_sets::{MajorPlatformType, RailSupportLevel};
    use ironrdp_pdu::rdp::client_info::{OptionalSystemTime, PerformanceFlags, TimezoneInfo};
    use ironrdp_session::image::DecodedImage;
    use ironrdp_session::{ActiveStageBuilder, ActiveStageOutput};
    use ironrdp_tokio::FramedWrite;
    use sandpolis_instance::network::StreamResponder;
    use sandpolis_macros::Stream;
    use sandpolis_probe::config::RdpProbeConfig;
    use std::net::IpAddr;
    use tokio::net::TcpStream;
    use tokio::sync::RwLock;
    use tokio::sync::mpsc::{Receiver, Sender, channel};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, warn};

    /// Stream responder that mirrors an RDP device into the desktop subsystem.
    ///
    /// IronRDP is async, so the session lives on a tokio task rather than the
    /// dedicated OS thread the (blocking) VNC crate needs. The connection is
    /// owned by that task, spawned before [`Start`](RdpStreamRequest::Start)
    /// returns: connecting inline would stall the connection's dispatch loop,
    /// since responder handlers run on the socket's receive path.
    #[derive(Stream, Default)]
    pub struct RdpStreamResponder {
        /// Cancels the background session task (on `Stop` or drop).
        cancel: CancellationToken,
        /// Sends input to that task. `None` until `Start`.
        input: RwLock<Option<Sender<DesktopStreamInputEvent>>>,
    }

    impl StreamResponder for RdpStreamResponder {
        type In = RdpStreamRequest;
        type Out = DesktopStreamResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            match request {
                RdpStreamRequest::Start { device_id } => {
                    // Ignore a duplicate Start: a session is already running.
                    if self.input.read().await.is_some() {
                        warn!("RDP stream already running; ignoring Start");
                        return Ok(());
                    }

                    let Some(device) = sandpolis_probe::REGISTERED_DEVICES
                        .read()
                        .ok()
                        .and_then(|devices| devices.iter().find(|d| d.id.body() == device_id).cloned())
                    else {
                        warn!("RDP stream requested for unregistered device {device_id}");
                        let _ = sender.send(DesktopStreamResponse::Stopped).await;
                        return Ok(());
                    };
                    let Some(config) = device.device.rdp.clone() else {
                        warn!("Device {device_id} has no RDP configuration");
                        let _ = sender.send(DesktopStreamResponse::Stopped).await;
                        return Ok(());
                    };

                    let (input_tx, input_rx) = channel(64);
                    *self.input.write().await = Some(input_tx);

                    let ip = device.device.ip;
                    let cancel = self.cancel.clone();
                    tokio::spawn(async move {
                        if let Err(e) = run_session(config, ip, input_rx, sender.clone(), cancel).await
                        {
                            warn!(error = %e, "RDP probe stream failed");
                        }
                        let _ = sender.send(DesktopStreamResponse::Stopped).await;
                    });
                }
                RdpStreamRequest::Input(event) => {
                    if let Some(tx) = self.input.read().await.as_ref() {
                        let _ = tx.send(event).await;
                    }
                }
                RdpStreamRequest::Stop => {
                    self.cancel.cancel();
                    // Drop the input sender so the task exits and a later Start
                    // can begin a fresh session.
                    *self.input.write().await = None;
                }
            }
            Ok(())
        }

        fn unreliable(response: &Self::Out) -> bool {
            crate::session::is_frame(response)
        }
    }

    impl Drop for RdpStreamResponder {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }

    /// Pack a decoded RGBA frame as zstd-compressed RGB888, which is what the
    /// agent path already sends and the client already decodes.
    fn encode_image(image: &DecodedImage) -> Result<Vec<u8>> {
        let data = image.data();
        let mut rgb = Vec::with_capacity(data.len() / 4 * 3);
        for pixel in data.chunks_exact(4) {
            rgb.extend_from_slice(&[pixel[0], pixel[1], pixel[2]]);
        }
        Ok(zstd::encode_all(rgb.as_slice(), 1)?)
    }

    /// Build the connector configuration for a probe device.
    fn connector_config(config: &RdpProbeConfig) -> Config {
        Config {
            desktop_size: DesktopSize {
                width: 1280,
                height: 720,
            },
            desktop_scale_factor: 0,
            // TLS security, not NLA — see the module header.
            enable_tls: true,
            enable_credssp: false,
            credentials: Credentials::UsernamePassword {
                username: config.username.clone().unwrap_or_default(),
                password: config.password.clone().unwrap_or_default(),
            },
            domain: config.domain.clone(),
            client_build: 0,
            client_name: "sandpolis".to_owned(),
            keyboard_type: KeyboardType::IBM_ENHANCED,
            keyboard_subtype: 0,
            keyboard_functional_keys_count: 12,
            keyboard_layout: 0,
            ime_file_name: String::new(),
            bitmap: None,
            dig_product_id: String::new(),
            client_dir: String::new(),
            alternate_shell: String::new(),
            work_dir: String::new(),
            platform: MajorPlatformType::UNIX,
            hardware_id: None,
            request_data: None,
            autologon: false,
            enable_audio_playback: false,
            performance_flags: PerformanceFlags::empty(),
            license_cache: None,
            timezone_info: TimezoneInfo {
                bias: 0,
                standard_name: String::new(),
                standard_date: OptionalSystemTime(None),
                standard_bias: 0,
                daylight_name: String::new(),
                daylight_date: OptionalSystemTime(None),
                daylight_bias: 0,
            },
            compression_type: None,
            enable_server_pointer: false,
            pointer_software_rendering: true,
            multitransport_flags: None,
            // A single monitor at `desktop_size`.
            monitor_layout: None,
            // Legacy RC4 "Standard RDP Security" stays off: TLS or nothing.
            enable_standard_rdp_security: false,
            connection_type: ConnectionType::Lan,
            // Full desktops only; RemoteApp windows have no place in the
            // desktop subsystem's single-surface frame format.
            remote_application_mode: false,
            rail_support_level: RailSupportLevel::empty(),
            enable_audio_capture: false,
            // No EGFX dynamic channel is registered, so don't advertise it.
            support_dyn_vc_gfx_protocol: false,
        }
    }

    /// Connect to the device and mirror it until cancelled.
    async fn run_session(
        config: RdpProbeConfig,
        ip: IpAddr,
        mut input_rx: Receiver<DesktopStreamInputEvent>,
        sender: Sender<DesktopStreamResponse>,
        cancel: CancellationToken,
    ) -> Result<()> {
        // The config's host wins, but a device registered with only an IP is
        // perfectly usable.
        let host = if config.host.is_empty() {
            ip.to_string()
        } else {
            config.host.clone()
        };
        let port = config.port.unwrap_or(3389);

        let stream = TcpStream::connect((host.as_str(), port)).await?;
        let client_addr = stream.local_addr()?;
        let mut framed = ironrdp_tokio::TokioFramed::new(stream);
        let mut connector = ClientConnector::new(connector_config(&config), client_addr);

        let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
            .await
            .map_err(|e| anyhow::anyhow!("begin RDP connection: {e}"))?;

        // Enhanced RDP Security: negotiate TLS, then finalize over it.
        let (initial_stream, leftover) = framed.into_inner();
        let (tls_stream, tls_cert) = ironrdp_tls::upgrade(initial_stream, host.as_str()).await?;
        let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
        let server_public_key = ironrdp_tls::extract_tls_server_public_key(&tls_cert)
            .ok_or_else(|| anyhow::anyhow!("server TLS certificate has no public key"))?
            .to_owned();

        let mut framed = ironrdp_tokio::TokioFramed::new_with_leftover(tls_stream, leftover);
        let mut network_client = ironrdp_tokio::reqwest::ReqwestNetworkClient::default();
        let result = ironrdp_tokio::connect_finalize(
            upgraded,
            connector,
            &mut framed,
            &mut network_client,
            host.clone().into(),
            server_public_key,
            None,
        )
        .await
        .map_err(|e| anyhow::anyhow!("finalize RDP connection: {e}"))?
        // A server redirection (e.g. a connection broker handing off to a
        // session host) surfaces as an error; following it is not yet supported.
        .into_connected()
        .map_err(|e| anyhow::anyhow!("RDP server redirected the connection: {e}"))?;

        let (width, height) = (result.desktop_size.width, result.desktop_size.height);
        if width == 0 || height == 0 {
            bail!("device reported a {width}x{height} desktop");
        }
        debug!(host = %host, width, height, "RDP probe stream opened");
        if sender
            .send(DesktopStreamResponse::Started {
                width: width as i32,
                height: height as i32,
            })
            .await
            .is_err()
        {
            return Ok(());
        }

        let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);
        let mut active_stage = ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: result.compression_type,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();
        let mut input_db = Database::new();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                event = input_rx.recv() => {
                    let Some(event) = event else { break };
                    if apply_input(&mut active_stage, &mut image, &mut input_db, &mut framed, event)
                        .await?
                    {
                        break;
                    }
                }
                pdu = framed.read_pdu() => {
                    let (action, payload) = match pdu {
                        Ok(pdu) => pdu,
                        // A closed connection is a normal end of stream.
                        Err(_) => break,
                    };
                    let outputs = active_stage.process(
                        &mut image,
                        action,
                        &payload,
                        framed.last_read_at(),
                    )?;
                    let mut dirty = false;
                    for output in outputs {
                        match output {
                            ActiveStageOutput::ResponseFrame(frame) => {
                                framed
                                    .write_all(&frame)
                                    .await
                                    .map_err(|e| anyhow::anyhow!("write response: {e}"))?;
                            }
                            ActiveStageOutput::GraphicsUpdate(_) => dirty = true,
                            ActiveStageOutput::Terminate(_) => {
                                debug!(host = %host, "RDP device terminated the session");
                                return Ok(());
                            }
                            // Pointer shapes and the other outputs have nowhere to
                            // go in the desktop subsystem's frame format.
                            _ => {}
                        }
                    }
                    if dirty {
                        let event = DesktopStreamOutputEvent {
                            width: Some(width as i32),
                            height: Some(height as i32),
                            dest_x: Some(0),
                            dest_y: Some(0),
                            source_x: Some(0),
                            source_y: Some(0),
                            color_mode: DesktopStreamColorMode::Rgb888,
                            compression_mode: DesktopStreamCompressionMode::Zstd,
                            pixel_data: Some(encode_image(&image)?),
                            clipboard: None,
                        };
                        if sender.send(DesktopStreamResponse::Frame(event)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }

        debug!(host = %host, "RDP probe stream closed");
        Ok(())
    }

    /// Replay one input event onto the device. Returns `true` if the write side
    /// failed and the session should end.
    async fn apply_input<S>(
        active_stage: &mut ironrdp_session::ActiveStage,
        image: &mut DecodedImage,
        input_db: &mut Database,
        framed: &mut ironrdp_tokio::TokioFramed<S>,
        event: DesktopStreamInputEvent,
    ) -> Result<bool>
    where
        S: tokio::io::AsyncWrite + Send + Sync + Unpin,
    {
        let mut operations = Vec::new();
        if let (Some(x), Some(y)) = (event.pointer_x, event.pointer_y) {
            operations.push(Operation::MouseMove(MousePosition {
                x: x.clamp(0, u16::MAX as i32) as u16,
                y: y.clamp(0, u16::MAX as i32) as u16,
            }));
        }
        if let Some(button) = event.pointer_pressed.and_then(map_button) {
            operations.push(Operation::MouseButtonPressed(button));
        }
        if let Some(button) = event.pointer_released.and_then(map_button) {
            operations.push(Operation::MouseButtonReleased(button));
        }
        // RDP carries Unicode keyboard events, so a `char` maps cleanly — but
        // non-printing keys (Enter, Backspace, arrows) never arrive, the same
        // limit the agent and VNC paths have.
        if let Some(character) = event.key_pressed {
            operations.push(Operation::UnicodeKeyPressed(character));
        }
        if let Some(character) = event.key_released {
            operations.push(Operation::UnicodeKeyReleased(character));
        }
        if operations.is_empty() {
            return Ok(false);
        }

        let events = input_db.apply(operations);
        let outputs = active_stage.process_fastpath_input(image, &events)?;
        for output in outputs {
            if let ActiveStageOutput::ResponseFrame(frame) = output
                && framed.write_all(&frame).await.is_err()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Map a desktop pointer button to its RDP equivalent.
    fn map_button(button: DesktopStreamPointerButton) -> Option<MouseButton> {
        Some(match button {
            DesktopStreamPointerButton::Primary => MouseButton::Left,
            DesktopStreamPointerButton::Middle => MouseButton::Middle,
            DesktopStreamPointerButton::Secondary => MouseButton::Right,
            DesktopStreamPointerButton::Back => MouseButton::X1,
            DesktopStreamPointerButton::Forward => MouseButton::X2,
        })
    }
}

#[cfg(all(feature = "server", feature = "probe"))]
pub use server::RdpStreamResponder;

#[cfg(all(feature = "client", feature = "probe"))]
mod client {
    use super::*;
    use crate::session::{DesktopStreamEvent, decode_frame};
    use anyhow::Result;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_macros::Stream;
    use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// Client side of an RDP probe stream. Identical in shape to
    /// [`VncStreamRequester`](crate::vnc::VncStreamRequester) and
    /// [`DesktopStreamRequester`](crate::session::DesktopStreamRequester) so all
    /// three feed the same viewer.
    #[derive(Stream)]
    pub struct RdpStreamRequester {
        events: UnboundedSender<DesktopStreamEvent>,
    }

    impl RdpStreamRequester {
        /// Construct a requester paired with the receiver the GUI drains.
        pub fn channel() -> (Self, UnboundedReceiver<DesktopStreamEvent>) {
            let (events, rx) = unbounded_channel();
            (Self { events }, rx)
        }
    }

    impl StreamRequester for RdpStreamRequester {
        type In = DesktopStreamResponse;
        type Out = RdpStreamRequest;

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
pub use client::RdpStreamRequester;
