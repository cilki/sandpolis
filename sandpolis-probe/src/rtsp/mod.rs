#[cfg(feature = "server")]
use anyhow::Result;
#[cfg(feature = "server")]
use retina::client::{Credentials, SessionGroup, SetupOptions};
#[cfg(feature = "server")]
use retina::codec::CodecItem;
#[cfg(feature = "server")]
use sandpolis_instance::network::StreamResponder;
#[cfg(feature = "server")]
use sandpolis_macros::Stream;
use serde::{Deserialize, Serialize};
#[cfg(feature = "server")]
use std::sync::Arc;
#[cfg(feature = "server")]
use tokio::sync::mpsc::Sender;
#[cfg(feature = "server")]
use tokio_util::sync::CancellationToken;
#[cfg(feature = "server")]
use tracing::debug;
#[cfg(feature = "server")]
use url::Url;

/// Request message for RTSP stream sessions.
#[derive(Serialize, Deserialize)]
pub enum RtspSessionStreamRequest {
    /// Start streaming from the given RTSP URL
    Start {
        /// RTSP URL *without* credentials (e.g. rtsp://host:554/stream). Retina
        /// rejects a URL carrying userinfo outright, so credentials travel in
        /// the fields below and are applied as digest auth.
        url: String,

        /// Transport protocol preference
        transport: RtspTransport,

        /// Username for RTSP authentication, if the camera requires it.
        #[serde(default)]
        username: Option<String>,

        /// Password paired with `username`.
        #[serde(default)]
        password: Option<String>,
    },
    /// Stop the stream
    Stop,
}

/// Transport protocol for RTSP streaming.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub enum RtspTransport {
    /// UDP transport (lower latency, may have packet loss)
    Udp,
    /// TCP interleaved transport (more reliable)
    #[default]
    Tcp,
}

/// Response message containing video/audio frame data.
#[derive(Serialize, Deserialize)]
pub struct RtspSessionStreamResponse {
    /// The stream index (0 for video, 1 for audio typically)
    pub stream_index: usize,

    /// Frame data
    pub frame: RtspFrame,
}

/// A single frame from the RTSP stream.
#[derive(Serialize, Deserialize)]
pub enum RtspFrame {
    /// Codec parameters (sent once before frames). For H.264 `extra_data` is the
    /// AVCDecoderConfigurationRecord containing SPS/PPS, which the client needs to
    /// initialize its decoder.
    Parameters {
        /// AVCDecoderConfigurationRecord (AVCC) bytes.
        extra_data: Vec<u8>,
        /// Display width in pixels.
        width: u32,
        /// Display height in pixels.
        height: u32,
    },
    /// H.264 video frame
    H264 {
        /// NAL units
        data: Vec<Vec<u8>>,
        /// Presentation timestamp in 90kHz units
        timestamp: i64,
        /// Whether this is a keyframe (IDR)
        is_keyframe: bool,
    },
    /// H.265/HEVC video frame
    H265 {
        /// NAL units
        data: Vec<Vec<u8>>,
        /// Presentation timestamp in 90kHz units
        timestamp: i64,
        /// Whether this is a keyframe
        is_keyframe: bool,
    },
    /// AAC audio frame
    Aac {
        /// Raw AAC data
        data: Vec<u8>,
        /// Presentation timestamp
        timestamp: i64,
    },
    /// G.711 audio frame
    G711 {
        /// Raw audio samples
        data: Vec<u8>,
        /// Presentation timestamp
        timestamp: i64,
    },
    /// Stream ended or error occurred
    End { reason: String },
}

/// Stream responder that connects to an RTSP source and forwards frames.
///
/// The camera session is owned by a background task that
/// [`Start`](RtspSessionStreamRequest::Start) spawns before returning. Connecting
/// and reading inline would stall the whole connection's dispatch loop, since
/// responder handlers run on the socket's receive path — and that same loop is
/// what flushes outbound messages, so not a single frame would ever reach the
/// client and the connection's database sync would wedge along with it.
#[cfg(feature = "server")]
#[derive(Stream, Default)]
pub struct RtspSessionStreamResponder {
    /// Cancels the background streaming task (on `Stop` or on drop).
    cancel: CancellationToken,
}

#[cfg(feature = "server")]
impl StreamResponder for RtspSessionStreamResponder {
    type In = RtspSessionStreamRequest;
    type Out = RtspSessionStreamResponse;

    async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
        match request {
            RtspSessionStreamRequest::Start {
                url,
                transport,
                username,
                password,
            } => {
                let cancel = self.cancel.clone();
                tokio::spawn(async move {
                    let result = tokio::select! {
                        _ = cancel.cancelled() => Ok("Stopped by request".to_string()),
                        result = stream_frames(url, transport, username, password, &sender) => result,
                    };

                    // Always tell the client why the stream ended. Without this a
                    // failure to connect is completely invisible: the responder's
                    // error would be swallowed and the UI would wait forever.
                    let reason = match result {
                        Ok(reason) => reason,
                        Err(e) => {
                            debug!(error = %e, "RTSP stream failed");
                            e.to_string()
                        }
                    };
                    let _ = sender
                        .send(RtspSessionStreamResponse {
                            stream_index: 0,
                            frame: RtspFrame::End { reason },
                        })
                        .await;
                });
            }
            RtspSessionStreamRequest::Stop => {
                self.cancel.cancel();
            }
        }
        Ok(())
    }
}

/// Connect to `url` and forward frames to `sender` until the stream ends,
/// returning the reason it ended.
#[cfg(feature = "server")]
async fn stream_frames(
    url: String,
    transport: RtspTransport,
    username: Option<String>,
    password: Option<String>,
    sender: &Sender<RtspSessionStreamResponse>,
) -> Result<String> {
    let mut parsed_url = Url::parse(&url)?;

    // Retina refuses any URL carrying userinfo, so strip it and fold it into the
    // credentials instead. Normally the client already sends a bare URL; this
    // keeps an older peer working rather than failing cryptically.
    let creds = match (username, password) {
        (Some(username), password) if !username.is_empty() => Some(Credentials {
            username,
            password: password.unwrap_or_default(),
        }),
        _ if !parsed_url.username().is_empty() => Some(Credentials {
            username: parsed_url.username().to_string(),
            password: parsed_url.password().unwrap_or_default().to_string(),
        }),
        _ => None,
    };
    let _ = parsed_url.set_username("");
    let _ = parsed_url.set_password(None);

    debug!(
        "Connecting to RTSP stream: {}",
        parsed_url.host_str().unwrap_or("unknown")
    );

    let session_group = Arc::new(SessionGroup::default());
    let mut session = retina::client::Session::describe(
        parsed_url,
        retina::client::SessionOptions::default()
            .session_group(session_group)
            .creds(creds),
    )
    .await?;

    // Setup all streams
    for i in 0..session.streams().len() {
        let setup_options = match transport {
            RtspTransport::Udp => SetupOptions::default()
                .transport(retina::client::Transport::Udp(Default::default())),
            RtspTransport::Tcp => SetupOptions::default()
                .transport(retina::client::Transport::Tcp(Default::default())),
        };
        session.setup(i, setup_options).await?;
    }

    // Capture codec parameters (SPS/PPS) so the client can decode.
    let mut param_msgs = Vec::new();
    for (i, stream) in session.streams().iter().enumerate() {
        if let Some(retina::codec::ParametersRef::Video(v)) = stream.parameters() {
            let (width, height) = v.pixel_dimensions();
            param_msgs.push(RtspSessionStreamResponse {
                stream_index: i,
                frame: RtspFrame::Parameters {
                    extra_data: v.extra_data().to_vec(),
                    width,
                    height,
                },
            });
        }
    }

    // Start playing
    let mut session = session
        .play(retina::client::PlayOptions::default())
        .await?
        .demuxed()?;

    // Send codec parameters before any frames.
    for msg in param_msgs {
        if sender.send(msg).await.is_err() {
            return Ok("Client disconnected".to_string());
        }
    }

    // Read frames in a loop
    loop {
        use futures::StreamExt;
        match session.next().await {
            Some(Ok(item)) => {
                let response = match item {
                    CodecItem::VideoFrame(frame) => {
                        let stream_id = frame.stream_id();
                        let is_keyframe = frame.is_random_access_point();
                        let timestamp = frame.timestamp().timestamp();
                        let data = frame.into_data();

                        RtspSessionStreamResponse {
                            stream_index: stream_id,
                            frame: RtspFrame::H264 {
                                data: vec![data],
                                timestamp,
                                is_keyframe,
                            },
                        }
                    }
                    CodecItem::AudioFrame(frame) => {
                        let timestamp = frame.timestamp().timestamp();
                        let data = frame.data().to_vec();

                        RtspSessionStreamResponse {
                            stream_index: frame.stream_id(),
                            frame: RtspFrame::Aac { data, timestamp },
                        }
                    }
                    CodecItem::MessageFrame(_) => continue,
                    _ => continue,
                };

                if sender.send(response).await.is_err() {
                    return Ok("Client disconnected".to_string());
                }
            }
            Some(Err(e)) => return Err(e.into()),
            None => return Ok("Stream ended".to_string()),
        }
    }
}

#[cfg(feature = "server")]
impl Drop for RtspSessionStreamResponder {
    fn drop(&mut self) {
        debug!("RTSP session responder dropped");
        // Stop the background task in case the stream is still running.
        self.cancel.cancel();
    }
}

/// Registers [`RtspSessionStreamResponder`] on each connection.
#[cfg(feature = "server")]
pub struct RtspResponderRegistration;

#[cfg(feature = "server")]
impl sandpolis_instance::network::RegisterResponders for RtspResponderRegistration {
    fn register_responders(&self, registry: &sandpolis_instance::network::StreamRegistry) {
        registry.register_responder(RtspSessionStreamResponder::default);
    }
}

#[cfg(feature = "server")]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &RtspResponderRegistration
));

#[cfg(feature = "client")]
mod client {
    use super::{RtspFrame, RtspSessionStreamRequest, RtspSessionStreamResponse};
    use anyhow::Result;
    use openh264::formats::YUVSource;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_macros::Stream;
    use std::sync::Mutex;
    use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

    /// A decoded RGBA8 frame ready to upload to a GUI texture.
    pub struct RtspFrameRgba {
        pub width: u32,
        pub height: u32,
        /// Tightly packed RGBA8 (`width * height * 4` bytes).
        pub rgba: Vec<u8>,
    }

    /// Events surfaced to the GUI as an RTSP stream progresses.
    pub enum RtspStreamEvent {
        /// The stream was accepted by the multiplexer. Carries the id the GUI
        /// needs to look up the stream's byte counters on the connection.
        Opened(sandpolis_instance::network::stream::StreamId),
        Started { width: u32, height: u32 },
        Frame(RtspFrameRgba),
        /// The remote end closed the stream, with the reason it reported.
        Stopped { reason: String },
        /// The stream could not be established or decoded locally.
        Failed(String),
    }

    /// How many consecutive decode failures before we tell the user the stream
    /// is undecodable rather than letting it sit there blank.
    const DECODE_ERROR_LIMIT: u32 = 60;

    /// Per-stream decoder state.
    struct DecoderState {
        decoder: Option<openh264::decoder::Decoder>,
        /// SPS/PPS in Annex-B form, prepended to each keyframe.
        sps_pps: Vec<u8>,
        /// NAL length prefix size from the AVCDecoderConfigurationRecord.
        nal_length_size: usize,
        started: bool,
        /// Consecutive decode failures, reset by each decoded picture.
        decode_errors: u32,
    }

    impl Default for DecoderState {
        fn default() -> Self {
            Self {
                decoder: None,
                sps_pps: Vec::new(),
                nal_length_size: 4,
                started: false,
                decode_errors: 0,
            }
        }
    }

    /// Client side of an RTSP stream: decodes incoming H.264 to RGBA8 and forwards
    /// frames to the GUI through an unbounded channel.
    #[derive(Stream)]
    pub struct RtspSessionStreamRequester {
        events: UnboundedSender<RtspStreamEvent>,
        state: Mutex<DecoderState>,
    }

    impl RtspSessionStreamRequester {
        /// Construct a requester paired with the receiver the GUI drains. The
        /// sender is handed back too so the caller can report failures that
        /// happen before the stream is ever opened.
        pub fn channel() -> (
            Self,
            UnboundedSender<RtspStreamEvent>,
            UnboundedReceiver<RtspStreamEvent>,
        ) {
            let (events, rx) = unbounded_channel();
            (
                Self {
                    events: events.clone(),
                    state: Mutex::new(DecoderState::default()),
                },
                events,
                rx,
            )
        }

        /// Lazily create the H.264 decoder, reporting why it couldn't be built.
        fn ensure_decoder(&self, state: &mut DecoderState) -> Result<(), String> {
            if state.decoder.is_some() {
                return Ok(());
            }
            match openh264::decoder::Decoder::new() {
                Ok(decoder) => {
                    state.decoder = Some(decoder);
                    Ok(())
                }
                Err(e) => Err(format!("Failed to initialize H.264 decoder: {e}")),
            }
        }
    }

    impl StreamRequester for RtspSessionStreamRequester {
        type In = RtspSessionStreamResponse;
        type Out = RtspSessionStreamRequest;

        async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
            match response.frame {
                RtspFrame::Parameters {
                    extra_data,
                    width,
                    height,
                } => {
                    let (sps_pps, nal_length_size) = parse_avcc_record(&extra_data);
                    let mut st = self.state.lock().unwrap();
                    st.sps_pps = sps_pps;
                    st.nal_length_size = nal_length_size.max(1);
                    if let Err(e) = self.ensure_decoder(&mut st) {
                        let _ = self.events.send(RtspStreamEvent::Failed(e));
                        return Ok(());
                    }
                    if !st.started {
                        st.started = true;
                        let _ = self.events.send(RtspStreamEvent::Started { width, height });
                    }
                }
                RtspFrame::H264 {
                    data, is_keyframe, ..
                } => {
                    let mut st = self.state.lock().unwrap();
                    let nls = st.nal_length_size;

                    // Build an Annex-B access unit, prepending SPS/PPS on keyframes.
                    let mut au = Vec::new();
                    if is_keyframe {
                        au.extend_from_slice(&st.sps_pps);
                    }
                    for nal in &data {
                        avcc_to_annexb(nal, nls, &mut au);
                    }

                    if let Err(e) = self.ensure_decoder(&mut st) {
                        let _ = self.events.send(RtspStreamEvent::Failed(e));
                        return Ok(());
                    }
                    // Decoded into an owned frame first so the borrow of `st` ends
                    // before the error/success counters below touch it.
                    let decoded = {
                        let Some(decoder) = st.decoder.as_mut() else {
                            return Ok(());
                        };
                        decoder.decode(&au).map(|picture| {
                            picture.map(|yuv| {
                                let (w, h) = yuv.dimensions();
                                let mut rgba = vec![0u8; w * h * 4];
                                yuv.write_rgba8(&mut rgba);
                                RtspFrameRgba {
                                    width: w as u32,
                                    height: h as u32,
                                    rgba,
                                }
                            })
                        })
                    };

                    match decoded {
                        Ok(Some(frame)) => {
                            st.decode_errors = 0;
                            let _ = self.events.send(RtspStreamEvent::Frame(frame));
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::debug!(error = %e, "H.264 decode error");
                            // Isolated errors are normal until the first keyframe
                            // arrives; a sustained run means we'll never produce a
                            // picture, so say so instead of showing a blank panel.
                            st.decode_errors += 1;
                            if st.decode_errors == DECODE_ERROR_LIMIT {
                                let _ = self.events.send(RtspStreamEvent::Failed(format!(
                                    "Unable to decode video: {e}"
                                )));
                            }
                        }
                    }
                }
                RtspFrame::End { reason } => {
                    let _ = self.events.send(RtspStreamEvent::Stopped { reason });
                }
                // H.265/audio frames are not decoded for MVP.
                _ => {}
            }
            Ok(())
        }
    }

    /// Parse an AVCDecoderConfigurationRecord into Annex-B SPS/PPS bytes and the
    /// NAL length prefix size used by frame data.
    fn parse_avcc_record(data: &[u8]) -> (Vec<u8>, usize) {
        let mut out = Vec::new();
        // Minimum header is 6 bytes; bail to sane defaults otherwise.
        if data.len() < 7 {
            return (out, 4);
        }
        let nal_length_size = (data[4] & 0x03) as usize + 1;
        let mut pos = 5;

        // SPS set.
        let num_sps = (data[pos] & 0x1f) as usize;
        pos += 1;
        for _ in 0..num_sps {
            if pos + 2 > data.len() {
                return (out, nal_length_size);
            }
            let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + len > data.len() {
                return (out, nal_length_size);
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }

        // PPS set.
        if pos >= data.len() {
            return (out, nal_length_size);
        }
        let num_pps = data[pos] as usize;
        pos += 1;
        for _ in 0..num_pps {
            if pos + 2 > data.len() {
                return (out, nal_length_size);
            }
            let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + len > data.len() {
                return (out, nal_length_size);
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }

        (out, nal_length_size)
    }

    /// Convert one AVCC (length-prefixed) buffer of NAL units into Annex-B
    /// (start-code prefixed), appending to `out`.
    fn avcc_to_annexb(data: &[u8], nal_length_size: usize, out: &mut Vec<u8>) {
        let mut pos = 0;
        while pos + nal_length_size <= data.len() {
            let mut len = 0usize;
            for i in 0..nal_length_size {
                len = (len << 8) | data[pos + i] as usize;
            }
            pos += nal_length_size;
            if len == 0 || pos + len > data.len() {
                break;
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn avcc_to_annexb_two_nals() {
            // Two NAL units: lengths 3 and 2, with 4-byte length prefixes.
            let avcc = [
                0, 0, 0, 3, 0xAA, 0xBB, 0xCC, // NAL 1
                0, 0, 0, 2, 0xDD, 0xEE, // NAL 2
            ];
            let mut out = Vec::new();
            avcc_to_annexb(&avcc, 4, &mut out);
            assert_eq!(
                out,
                vec![0, 0, 0, 1, 0xAA, 0xBB, 0xCC, 0, 0, 0, 1, 0xDD, 0xEE]
            );
        }

        #[test]
        fn avcc_to_annexb_truncated_is_safe() {
            // Declares length 9 but only 2 bytes follow: must not panic.
            let avcc = [0, 0, 0, 9, 0x01, 0x02];
            let mut out = Vec::new();
            avcc_to_annexb(&avcc, 4, &mut out);
            assert!(out.is_empty());
        }

        #[test]
        fn parse_record_extracts_sps_pps() {
            // version, profile, compat, level, lengthSizeMinusOne=3
            // numSPS=1, SPS len=2 [0x67,0x42], numPPS=1, PPS len=2 [0x68,0xCE]
            let record = [
                1, 0x42, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x42, 0x01, 0x00, 0x02, 0x68,
                0xce,
            ];
            let (annexb, nls) = parse_avcc_record(&record);
            assert_eq!(nls, 4);
            assert_eq!(
                annexb,
                vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce]
            );
        }
    }
}

#[cfg(feature = "client")]
pub use client::{RtspFrameRgba, RtspSessionStreamRequester, RtspStreamEvent};
