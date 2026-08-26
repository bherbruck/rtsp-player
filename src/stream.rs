//! Live RTSP playback: one worker per open stream.
//!
//! Each stream runs two threads. A network thread owns a single-threaded tokio
//! runtime and pulls access units off the wire with `retina`; a decode thread
//! turns those into BGRA frames. Splitting them keeps decode time (tens of ms
//! for a 1080p keyframe) from stalling RTCP and the TCP read loop.

use crate::model::{Connection, Transport};
use futures::StreamExt as _;
use retina::client::{PlayOptions, SessionOptions, SetupOptions};
use retina::codec::{CodecItem, FrameFormat, ParameterSetInsertion, ParametersRef};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many access units may decode without producing a picture before we
/// declare the stream broken.
const STARVATION_LIMIT: usize = 30;

/// Cap on the RTSP handshake, so an unreachable camera reports rather than hangs.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Access units to wait for a flagged key frame before decoding regardless.
const KEY_FRAME_WAIT: usize = 60;

/// Set `RTSP_PLAYER_DEBUG=1` to trace what arrives on the wire.
fn debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("RTSP_PLAYER_DEBUG").is_some())
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("[rtsp] {}", format!($($arg)*));
        }
    };
}

/// A decoded frame in the byte order gpui's `RenderImage` expects.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    Playing,
    Reconnecting(String),
    Failed(String),
}

impl Status {
    pub fn message(&self) -> &str {
        match self {
            Status::Connecting => "Connecting…",
            Status::Playing => "Playing",
            Status::Reconnecting(e) | Status::Failed(e) => e,
        }
    }
}

struct Shared {
    /// Single-slot mailbox. The decoder overwrites it; the UI takes it. A slow
    /// UI therefore skips frames instead of falling behind.
    latest: Mutex<Option<Frame>>,
    status: Mutex<Status>,
    stop: AtomicBool,
    decoded_frames: AtomicU64,
}

/// Handle to a running stream. Dropping it stops the worker threads.
pub struct Player {
    pub connection: Connection,
    shared: Arc<Shared>,
    started: Instant,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Player {
    pub fn start(connection: Connection) -> Self {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            status: Mutex::new(Status::Connecting),
            stop: AtomicBool::new(false),
            decoded_frames: AtomicU64::new(0),
        });

        {
            let shared = shared.clone();
            let connection = connection.clone();
            std::thread::Builder::new()
                .name(format!("rtsp-{}", connection.name))
                .spawn(move || network_thread(connection, shared))
                .expect("spawn rtsp thread");
        }

        Self {
            connection,
            shared,
            started: Instant::now(),
        }
    }

    /// Takes the newest decoded frame, if one has arrived since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.shared.latest.lock().unwrap().take()
    }

    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap().clone()
    }

    /// Total pictures the decoder has produced, including ones the UI skipped.
    pub fn decoded_count(&self) -> u64 {
        self.shared.decoded_frames.load(Ordering::Relaxed)
    }

    /// Average decoded frames per second since the stream was opened.
    pub fn fps(&self) -> f32 {
        let secs = self.started.elapsed().as_secs_f32();
        if secs < 0.5 {
            return 0.0;
        }
        self.shared.decoded_frames.load(Ordering::Relaxed) as f32 / secs
    }
}

fn set_status(shared: &Shared, status: Status) {
    *shared.status.lock().unwrap() = status;
}

fn network_thread(connection: Connection, shared: Arc<Shared>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            set_status(&shared, Status::Failed(format!("runtime: {e}")));
            return;
        }
    };

    let mut backoff = Duration::from_millis(500);
    while !shared.stop.load(Ordering::Relaxed) {
        let result = runtime.block_on(session(&connection, &shared));
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let reason = match result {
            Ok(()) => "stream ended".to_string(),
            Err(e) => trim_error(&e.to_string()),
        };
        set_status(&shared, Status::Reconnecting(reason));

        // Sleep in slices so a close during backoff is still responsive.
        let deadline = Instant::now() + backoff;
        while Instant::now() < deadline && !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// RTSP error strings carry a lot of context that does not fit a status line.
fn trim_error(text: &str) -> String {
    let first = text.lines().next().unwrap_or(text).trim();
    if first.len() > 120 {
        format!("{}…", &first[..119])
    } else {
        first.to_string()
    }
}

async fn session(connection: &Connection, shared: &Arc<Shared>) -> anyhow::Result<()> {
    set_status(shared, Status::Connecting);

    // A camera that is off or firewalled otherwise leaves the connect hanging
    // with no feedback at all.
    let setup = tokio::time::timeout(CONNECT_TIMEOUT, connect(connection));
    let (mut demuxed, video_stream, codec) = match setup.await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("timed out connecting to {}", connection.url),
    };

    // Bounded so a slow decoder drops old access units instead of growing a
    // queue that would show ever-staler video.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);

    // Prime the decoder with the VPS/SPS/PPS from the SDP, so it is configured
    // even if the server never sends them in-band.
    if let Some(ParametersRef::Video(params)) = demuxed.streams()[video_stream].parameters() {
        let extra = params.extra_data().to_vec();
        if !extra.is_empty() {
            trace!("priming decoder with {} bytes of parameter sets", extra.len());
            let _ = tx.try_send(extra);
        }
    }

    let decode_shared = shared.clone();
    let decode_thread = std::thread::Builder::new()
        .name("rtsp-decode".into())
        .spawn(move || decode_thread(codec, rx, decode_shared))?;

    let result = pump(&mut demuxed, &tx, shared, video_stream).await;
    drop(tx);
    let _ = decode_thread.join();
    result
}

/// DESCRIBE, SETUP and PLAY. Split out so the whole handshake can be bounded by
/// a single timeout.
async fn connect(
    connection: &Connection,
) -> anyhow::Result<(retina::client::Demuxed, usize, Codec)> {
    let url = url::Url::parse(&connection.url)?;
    let mut options = SessionOptions::default();
    if let Some((username, password)) = connection.credentials() {
        options = options.creds(Some(retina::client::Credentials { username, password }));
    }

    let mut session = retina::client::Session::describe(url, options).await?;

    let video_stream = session
        .streams()
        .iter()
        .position(|s| s.media() == "video")
        .ok_or_else(|| anyhow::anyhow!("no video stream in this URL"))?;
    let encoding = session.streams()[video_stream]
        .encoding_name()
        .to_ascii_lowercase();
    trace!("video stream {video_stream} encoding {encoding}");
    let codec = Codec::for_encoding(&encoding)
        .ok_or_else(|| anyhow::anyhow!("unsupported video codec: {encoding}"))?;

    let transport = match connection.transport {
        Transport::Tcp => retina::client::Transport::Tcp(Default::default()),
        Transport::Udp => retina::client::Transport::Udp(Default::default()),
    };
    session
        .setup(
            video_stream,
            // Annex B is what the pure-Rust decoders want. Inline the parameter
            // sets on change rather than on each key frame: some servers never
            // flag a key frame, and then EachKeyFrame never inlines anything.
            SetupOptions::default()
                .transport(transport)
                .frame_format(FrameFormat {
                    h26x_framing: retina::codec::h26x::Framing::AnnexB,
                    parameter_set_insertion: ParameterSetInsertion::OnChange,
                    ..FrameFormat::SIMPLE
                }),
        )
        .await?;

    let demuxed = session.play(PlayOptions::default()).await?.demuxed()?;
    Ok((demuxed, video_stream, codec))
}

async fn pump(
    demuxed: &mut retina::client::Demuxed,
    tx: &SyncSender<Vec<u8>>,
    shared: &Arc<Shared>,
    video_stream: usize,
) -> anyhow::Result<()> {
    let mut seen_key_frame = false;
    let mut waiting_for_key = 0usize;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let item = match tokio::time::timeout(Duration::from_secs(15), demuxed.next()).await {
            Ok(Some(item)) => item?,
            Ok(None) => return Ok(()),
            Err(_) => anyhow::bail!("timed out waiting for video"),
        };

        let CodecItem::VideoFrame(frame) = item else {
            continue;
        };
        trace!(
            "frame stream={} rap={} bytes={} loss={}",
            frame.stream_id(),
            frame.is_random_access_point(),
            frame.data().len(),
            frame.loss()
        );
        if frame.stream_id() != video_stream {
            continue;
        }
        // Feeding a decoder mid-GOP produces green garbage, so wait for an IDR
        // first. Not every server flags them correctly though, so give up
        // waiting after a while and let the decoder sort itself out rather than
        // sitting silent forever.
        if !seen_key_frame {
            waiting_for_key += 1;
            if frame.is_random_access_point() {
                seen_key_frame = true;
            } else if waiting_for_key < KEY_FRAME_WAIT {
                continue;
            } else {
                trace!("no key frame flagged after {waiting_for_key} frames; decoding anyway");
                seen_key_frame = true;
            }
        }

        match tx.try_send(frame.into_data()) {
            Ok(()) => {}
            // Decoder is behind. Dropping is the right call for live video.
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => return Ok(()),
        }
    }
}

/// Which decoder a stream needs. `rust_h265::Decoder` holds `Rc`s and so is
/// `!Send`; only this tag crosses the thread boundary, and the decoder itself is
/// built on the decode thread.
#[derive(Clone, Copy)]
enum Codec {
    H264,
    H265,
}

impl Codec {
    fn for_encoding(encoding: &str) -> Option<Self> {
        match encoding {
            "h264" => Some(Codec::H264),
            "h265" | "hevc" => Some(Codec::H265),
            _ => None,
        }
    }
}

enum Decoder {
    H264(Box<rusty_h264::Decoder>),
    H265(Box<rust_h265::Decoder>),
}

fn decode_thread(codec: Codec, rx: std::sync::mpsc::Receiver<Vec<u8>>, shared: Arc<Shared>) {
    let mut decoder = match codec {
        Codec::H264 => Decoder::H264(Box::new(rusty_h264::Decoder::new())),
        Codec::H265 => Decoder::H265(Box::new(rust_h265::Decoder::new())),
    };
    trace!("decode thread started");
    let mut announced_playing = false;
    let mut starved = 0usize;
    let mut last_error: Option<String> = None;
    while let Ok(access_unit) = rx.recv() {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        trace!("decoding access unit of {} bytes", access_unit.len());

        let frame = match &mut decoder {
            Decoder::H264(d) => match d.decode(&access_unit) {
                Ok(Some(yuv)) => Some(yuv_to_bgra(
                    yuv.width,
                    yuv.height,
                    &yuv.y,
                    &yuv.u,
                    &yuv.v,
                    yuv.width,
                    yuv.width / 2,
                )),
                Ok(None) => None,
                Err(e) => {
                    last_error = Some(e.to_string());
                    None
                }
            },
            Decoder::H265(d) => match decode_h265(d, &access_unit) {
                Ok(frame) => frame,
                Err(e) => {
                    last_error = Some(e);
                    None
                }
            },
        };

        let Some(frame) = frame else {
            // A decoder that never yields a picture is a dead stream as far as
            // the user is concerned, so surface why rather than sitting on
            // "Connecting" forever.
            starved += 1;
            if !announced_playing && starved == STARVATION_LIMIT {
                let reason = last_error
                    .clone()
                    .unwrap_or_else(|| "decoder produced no pictures".to_string());
                set_status(&shared, Status::Failed(reason));
            }
            continue;
        };
        starved = 0;

        *shared.latest.lock().unwrap() = Some(frame);
        shared.decoded_frames.fetch_add(1, Ordering::Relaxed);
        if !announced_playing {
            set_status(&shared, Status::Playing);
            announced_playing = true;
        }
    }
}

fn decode_h265(
    decoder: &mut rust_h265::Decoder,
    access_unit: &[u8],
) -> Result<Option<Frame>, String> {
    let mut out = None;
    for nal in rust_h265::parse_annex_b(access_unit) {
        match decoder.decode_nal(&nal) {
            // Keep the newest picture if an access unit yields several.
            Ok(Some(frame)) => out = Some(frame),
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    let Some(frame) = out else {
        return Ok(None);
    };
    let width = frame.width as usize;
    let height = frame.height as usize;

    // Main 10 gives 16-bit samples; scale them down rather than refusing to play.
    let y = plane_to_u8(&frame.y, frame.bit_depth);
    let u = plane_to_u8(&frame.u, frame.bit_depth);
    let v = plane_to_u8(&frame.v, frame.bit_depth);

    Ok(Some(yuv_to_bgra(
        width,
        height,
        &y,
        &u,
        &v,
        width,
        width.div_ceil(2),
    )))
}

fn plane_to_u8(plane: &rust_h265::PixelData, bit_depth: u8) -> std::borrow::Cow<'_, [u8]> {
    match plane {
        rust_h265::PixelData::U8(data) => std::borrow::Cow::Borrowed(data),
        rust_h265::PixelData::U16(data) => {
            let shift = bit_depth.saturating_sub(8);
            std::borrow::Cow::Owned(data.iter().map(|s| (s >> shift) as u8).collect())
        }
    }
}

/// YUV 4:2:0 to BGRA. Uses BT.709 coefficients at 720p and above, BT.601 below,
/// which is what cameras signal in practice.
fn yuv_to_bgra(
    width: usize,
    height: usize,
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_stride: usize,
    uv_stride: usize,
) -> Frame {
    let (cr_r, cb_g, cr_g, cb_b) = if height >= 720 {
        (459i32, -55i32, -136i32, 541i32) // BT.709 limited range
    } else {
        (409i32, -100i32, -208i32, 516i32) // BT.601 limited range
    };

    let mut bgra = vec![0u8; width * height * 4];
    for row in 0..height {
        let y_row = row * y_stride;
        let uv_row = (row / 2) * uv_stride;
        let out_row = row * width * 4;
        for col in 0..width {
            let y = (y_plane[y_row + col] as i32 - 16) * 298;
            let u = u_plane[uv_row + col / 2] as i32 - 128;
            let v = v_plane[uv_row + col / 2] as i32 - 128;

            let r = (y + cr_r * v + 128) >> 8;
            let g = (y + cb_g * u + cr_g * v + 128) >> 8;
            let b = (y + cb_b * u + 128) >> 8;

            let px = out_row + col * 4;
            bgra[px] = b.clamp(0, 255) as u8;
            bgra[px + 1] = g.clamp(0, 255) as u8;
            bgra[px + 2] = r.clamp(0, 255) as u8;
            bgra[px + 3] = 255;
        }
    }

    Frame {
        width: width as u32,
        height: height as u32,
        bgra,
    }
}
