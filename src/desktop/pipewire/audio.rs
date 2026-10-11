//! PipeWire audio capture.
//!
//! Captures desktop audio via PipeWire, forwards PCM sample buffers through a
//! bounded channel, and runs on a dedicated thread because PipeWire objects are
//! not Send.

use std::borrow::Cow;
use std::mem;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pipewire as pw;
use pw::spa;
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::Pod;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

/// Audio sample format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// 32-bit float (little endian)
    F32,
    /// 16-bit signed integer (little endian)
    I16,
}

impl AudioFormat {
    fn to_spa_format(self) -> spa::param::audio::AudioFormat {
        match self {
            Self::F32 => spa::param::audio::AudioFormat::F32LE,
            Self::I16 => spa::param::audio::AudioFormat::S16LE,
        }
    }

    /// Bytes per single sample (one channel)
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::F32 => mem::size_of::<f32>(),
            Self::I16 => mem::size_of::<i16>(),
        }
    }
}

/// Audio capture configuration
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Sample rate in Hz (default: 48000)
    pub sample_rate: u32,
    /// Number of channels (default: 2)
    pub channels: u32,
    /// Output sample format
    pub format: AudioFormat,
    /// Requested latency in frames; PipeWire may choose a different graph quantum.
    pub buffer_frames: u32,
    /// Optional PipeWire remote name or absolute socket path.
    pub remote_name: Option<String>,
}

impl CaptureConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=i32::MAX as u32).contains(&self.sample_rate),
            "audio sample rate is outside the SPA integer range"
        );
        anyhow::ensure!(
            (1..=spa::param::audio::MAX_CHANNELS as u32).contains(&self.channels),
            "audio channels must be between 1 and 64"
        );
        anyhow::ensure!(self.buffer_frames > 0, "audio latency must be nonzero");
        Ok(())
    }

    fn format_pod(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut audio_info = spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(self.format.to_spa_format());
        audio_info.set_rate(self.sample_rate);
        audio_info.set_channels(self.channels);
        let mut positions = [0; spa::param::audio::MAX_CHANNELS];
        match self.channels {
            1 => positions[0] = libspa_sys::SPA_AUDIO_CHANNEL_MONO,
            2 => {
                positions[0] = libspa_sys::SPA_AUDIO_CHANNEL_FL;
                positions[1] = libspa_sys::SPA_AUDIO_CHANNEL_FR;
            }
            _ => {}
        }
        audio_info.set_position(positions);
        let object = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: audio_info.into(),
        };
        Ok(spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(object),
        )
        .context("Failed to serialize audio format pod")?
        .0
        .into_inner())
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            format: AudioFormat::F32,
            buffer_frames: 1024,
            remote_name: None,
        }
    }
}

/// Typed audio sample buffer
#[derive(Debug, Clone)]
pub enum AudioSamples {
    /// 32-bit float samples
    F32(Vec<f32>),
    /// 16-bit signed integer samples
    I16(Vec<i16>),
}

impl AudioSamples {
    /// Number of samples (all channels combined)
    pub fn len(&self) -> usize {
        match self {
            Self::F32(s) => s.len(),
            Self::I16(s) => s.len(),
        }
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Convert to i16 samples
    pub fn to_i16(&self) -> Vec<i16> {
        match self {
            Self::F32(samples) => samples
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect(),
            Self::I16(samples) => samples.clone(),
        }
    }

    /// Convert to f32 samples
    pub fn to_f32(&self) -> Vec<f32> {
        match self {
            Self::F32(samples) => samples.clone(),
            Self::I16(samples) => samples.iter().map(|&s| s as f32 / 32768.0).collect(),
        }
    }
}

/// Handle to a running audio capture session
pub struct AudioCaptureHandle {
    /// Receiver for captured audio samples
    pub receiver: mpsc::Receiver<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
}

impl AudioCaptureHandle {
    /// Clone the thread-safe stop signal for connection-owned tasks.
    pub fn stop_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_signal)
    }

    /// Signal the capture thread to stop
    pub fn stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }

    /// Check if capture has been stopped
    pub fn is_stopped(&self) -> bool {
        self.stop_signal.load(Ordering::SeqCst)
    }
}

impl Drop for AudioCaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Decode complete interleaved frames from the valid SPA memory region.
/// SPA chunk offsets wrap modulo maxsize; size cannot exceed that allocation.
fn decode_chunk(
    data: &[u8],
    offset: usize,
    size: usize,
    format: spa::param::audio::AudioFormat,
    channels: u32,
    output: AudioFormat,
) -> Option<AudioSamples> {
    use spa::param::audio::AudioFormat as SpaFormat;
    if data.is_empty() || channels == 0 {
        return None;
    }
    let width = match format {
        SpaFormat::S16LE | SpaFormat::S16BE => 2,
        SpaFormat::F32LE | SpaFormat::F32BE => 4,
        _ => return None,
    };
    let frame_width = width * channels as usize;
    let size = size.min(data.len()) / frame_width * frame_width;
    if size == 0 {
        return None;
    }
    let offset = offset % data.len();
    let bytes = if offset + size <= data.len() {
        Cow::Borrowed(&data[offset..offset + size])
    } else {
        Cow::Owned(
            data[offset..]
                .iter()
                .chain(data[..offset].iter())
                .take(size)
                .copied()
                .collect(),
        )
    };
    let samples = match format {
        SpaFormat::S16LE | SpaFormat::S16BE => {
            let integers = bytes
                .chunks_exact(2)
                .map(|b| {
                    let bytes = [b[0], b[1]];
                    if format == SpaFormat::S16LE {
                        i16::from_le_bytes(bytes)
                    } else {
                        i16::from_be_bytes(bytes)
                    }
                })
                .collect();
            AudioSamples::I16(integers)
        }
        SpaFormat::F32LE | SpaFormat::F32BE => {
            let floats = bytes
                .chunks_exact(4)
                .map(|b| {
                    let bytes = [b[0], b[1], b[2], b[3]];
                    if format == SpaFormat::F32LE {
                        f32::from_le_bytes(bytes)
                    } else {
                        f32::from_be_bytes(bytes)
                    }
                })
                .collect();
            AudioSamples::F32(floats)
        }
        _ => return None,
    };
    Some(match (output, samples) {
        (AudioFormat::I16, samples @ AudioSamples::I16(_))
        | (AudioFormat::F32, samples @ AudioSamples::F32(_)) => samples,
        (AudioFormat::I16, samples) => AudioSamples::I16(samples.to_i16()),
        (AudioFormat::F32, samples) => AudioSamples::F32(samples.to_f32()),
    })
}

/// Compositor autostart can publish its socket after RDPSND negotiation.
/// Readiness waits stay on the capture thread and stop with the connection.
fn wait_for_remote(remote: Option<&str>, stopped: &AtomicBool, timeout: Duration) -> Result<bool> {
    let Some(path) = remote.map(Path::new).filter(|path| path.is_absolute()) else {
        return Ok(!stopped.load(Ordering::SeqCst));
    };
    let deadline = Instant::now() + timeout;
    loop {
        if stopped.load(Ordering::SeqCst) {
            return Ok(false);
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_socket(),
                    "PipeWire remote is not a Unix socket: {}",
                    path.display()
                );
                return Ok(true);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Failed to inspect PipeWire socket"),
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "PipeWire socket did not become ready: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct CaptureUserData {
    format: spa::param::audio::AudioInfoRaw,
    output_format: AudioFormat,
    sender: mpsc::Sender<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
    samples_captured: u64,
    samples_dropped: u64,
}

/// PipeWire audio capture engine.
///
/// Captures desktop audio and forwards PCM sample buffers through a channel.
/// Must run on a dedicated thread via [`spawn_audio_capture`].
pub struct AudioCapture {
    config: CaptureConfig,
    sender: mpsc::Sender<AudioSamples>,
    stop_signal: Arc<AtomicBool>,
}

impl AudioCapture {
    /// Create a new capture instance and its handle
    pub fn new(config: CaptureConfig, channel_size: usize) -> (Self, AudioCaptureHandle) {
        let (sender, receiver) = mpsc::channel(channel_size);
        let stop_signal = Arc::new(AtomicBool::new(false));

        let capture = Self {
            config,
            sender,
            stop_signal: Arc::clone(&stop_signal),
        };

        let handle = AudioCaptureHandle {
            receiver,
            stop_signal,
        };

        (capture, handle)
    }

    /// Run the PipeWire main loop for audio capture (blocking).
    ///
    /// Call from a dedicated thread. Connects to the PipeWire daemon,
    /// negotiates audio format, and delivers samples until stopped.
    pub fn start_capture(&self, node_id: Option<u32>) -> Result<()> {
        let pod_bytes = self.config.format_pod()?;
        if !wait_for_remote(
            self.config.remote_name.as_deref(),
            &self.stop_signal,
            Duration::from_secs(5),
        )? {
            return Ok(());
        }
        info!(
            "Starting audio capture: {}Hz, {} channels, format={:?}, node_id={:?}",
            self.config.sample_rate, self.config.channels, self.config.format, node_id
        );

        // PipeWire 0.9 Box types for owned resources
        let mainloop =
            pw::main_loop::MainLoopBox::new(None).context("Failed to create PipeWire MainLoop")?;
        let context = pw::context::ContextBox::new(mainloop.loop_(), None)
            .context("Failed to create PipeWire Context")?;
        let remote_properties = self.config.remote_name.as_ref().map(|remote| {
            pw::properties::properties! {
                *pw::keys::REMOTE_NAME => remote.as_str()
            }
        });
        let core = context
            .connect(remote_properties)
            .context("Failed to connect to PipeWire daemon")?;

        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "wrdp-audio-capture",
            *pw::keys::APP_NAME => "wrdp",
        };

        if let Some(id) = node_id {
            props.insert("target.object", id.to_string());
        }

        props.insert("stream.capture.sink", "true");
        props.insert(
            "node.latency",
            format!("{}/{}", self.config.buffer_frames, self.config.sample_rate),
        );

        let stream = pw::stream::StreamBox::new(&core, "wrdp-audio-capture", props)
            .context("Failed to create PipeWire stream")?;

        let user_data = CaptureUserData {
            format: spa::param::audio::AudioInfoRaw::default(),
            output_format: self.config.format,
            sender: self.sender.clone(),
            stop_signal: Arc::clone(&self.stop_signal),
            samples_captured: 0,
            samples_dropped: 0,
        };

        let stop_signal_for_callback = Arc::clone(&self.stop_signal);

        let _listener = stream
            .add_local_listener_with_user_data(user_data)
            .state_changed(move |_stream, _user_data, old, new| {
                debug!("Audio stream state: {:?} -> {:?}", old, new);

                match new {
                    pw::stream::StreamState::Error(err) => {
                        error!("Audio stream error: {}", err);
                        stop_signal_for_callback.store(true, Ordering::SeqCst);
                    }
                    pw::stream::StreamState::Streaming => {
                        info!("Audio capture streaming started");
                    }
                    pw::stream::StreamState::Paused => {
                        debug!("Audio stream paused");
                    }
                    _ => {}
                }
            })
            .param_changed(|_stream, user_data, id, param| {
                let Some(param) = param else {
                    return;
                };

                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }

                let (media_type, media_subtype) = match format_utils::parse_format(param) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to parse audio format: {:?}", e);
                        return;
                    }
                };

                if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                    debug!(
                        "Ignoring non-raw audio format: {:?}/{:?}",
                        media_type, media_subtype
                    );
                    return;
                }

                if let Err(e) = user_data.format.parse(param) {
                    warn!("Failed to parse audio info: {:?}", e);
                    return;
                }

                info!(
                    "Audio format negotiated: rate={}, channels={}, format={:?}",
                    user_data.format.rate(),
                    user_data.format.channels(),
                    user_data.format.format()
                );
            })
            .process(|stream, user_data| {
                if user_data.stop_signal.load(Ordering::Relaxed) {
                    return;
                }

                let Some(mut buffer) = stream.dequeue_buffer() else {
                    trace!("No buffer available");
                    return;
                };

                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }

                let data = &mut datas[0];
                let chunk = data.chunk();
                let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
                let Some(slice) = data.data() else {
                    return;
                };
                let Some(samples) = decode_chunk(
                    slice,
                    offset,
                    size,
                    user_data.format.format(),
                    user_data.format.channels(),
                    user_data.output_format,
                ) else {
                    return;
                };

                let sample_count = samples.len();

                // Non-blocking send preserves realtime capture behavior.
                match user_data.sender.try_send(samples) {
                    Ok(()) => {
                        user_data.samples_captured += sample_count as u64;
                        trace!("Captured {} samples", sample_count);
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        user_data.samples_dropped += sample_count as u64;
                        trace!("Dropped {} samples (channel full)", sample_count);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        user_data.stop_signal.store(true, Ordering::SeqCst);
                        debug!("Audio sample channel closed");
                    }
                }
            })
            .register()
            .context("Failed to register stream listener")?;

        let pod = Pod::from_bytes(&pod_bytes).context("Failed to create pod from bytes")?;

        let mut params = [pod];

        let flags = pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS;

        stream
            .connect(spa::utils::Direction::Input, node_id, flags, &mut params)
            .context("Failed to connect PipeWire stream")?;

        info!("Audio capture stream connected, starting main loop");

        let loop_ref = mainloop.loop_();
        while !self.stop_signal.load(Ordering::Relaxed) {
            loop_ref.iterate(std::time::Duration::from_millis(100));
        }

        info!("Audio capture stopped");
        Ok(())
    }

    /// Signal the capture to stop
    pub fn stop(&self) {
        self.stop_signal.store(true, Ordering::SeqCst);
    }
}

/// Spawn audio capture on a dedicated thread
///
/// Returns a handle with a receiver for audio samples. The capture runs
/// until the handle is dropped or `stop()` is called.
///
/// # Arguments
///
/// * `config` - Audio capture configuration
/// * `node_id` - Optional PipeWire node ID (from portal session)
/// * `channel_size` - Bounded channel capacity for sample buffers
pub fn spawn_audio_capture(
    config: CaptureConfig,
    node_id: Option<u32>,
    channel_size: usize,
) -> Result<AudioCaptureHandle> {
    config.validate()?;
    anyhow::ensure!(channel_size > 0, "audio channel capacity must be nonzero");
    let (capture, handle) = AudioCapture::new(config, channel_size);

    std::thread::Builder::new()
        .name("pipewire-audio".into())
        .spawn(move || {
            pw::init();

            if let Err(e) = capture.start_capture(node_id) {
                error!("Audio capture error: {:#}", e);
            }

            capture.stop();

            // init() is process-wide and once-only. Per-capture teardown drops
            // stream/core/context, but must preserve the library for reconnects.
        })
        .context("Failed to spawn audio capture thread")?;

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_formats_report_storage_width() {
        for (format, width) in [(AudioFormat::I16, 2), (AudioFormat::F32, 4)] {
            assert_eq!(format.bytes_per_sample(), width);
        }
    }

    #[test]
    fn default_capture_is_stereo_at_48_khz() {
        let actual = CaptureConfig::default();
        assert_eq!((actual.sample_rate, actual.channels), (48_000, 2));
        assert_eq!(actual.format, AudioFormat::F32);
    }

    #[test]
    fn conversion_preserves_zero_and_half_scale() {
        let integer = AudioSamples::F32(vec![0.0, -0.5, 0.5]).to_i16();
        assert_eq!(integer[0], 0);
        assert!((i32::from(integer[1]) + 16_383).abs() <= 1);
        assert!((i32::from(integer[2]) - 16_383).abs() <= 1);

        let float = AudioSamples::I16(vec![0, -16_384, 16_384]).to_f32();
        for (actual, expected) in float.into_iter().zip([0.0, -0.5, 0.5]) {
            assert!((actual - expected).abs() < 0.001);
        }
    }

    #[test]
    fn negotiation_pod_preserves_pcm_rate_channels_and_positions() {
        let config = CaptureConfig {
            format: AudioFormat::I16,
            ..CaptureConfig::default()
        };
        let bytes = config.format_pod().unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let mut actual = spa::param::audio::AudioInfoRaw::new();
        actual.parse(pod).unwrap();
        assert_eq!(actual.format(), spa::param::audio::AudioFormat::S16LE);
        assert_eq!(actual.rate(), 48_000);
        assert_eq!(actual.channels(), 2);
        assert_eq!(
            &actual.position()[..2],
            &[
                libspa_sys::SPA_AUDIO_CHANNEL_FL,
                libspa_sys::SPA_AUDIO_CHANNEL_FR
            ]
        );
    }

    #[test]
    fn invalid_capture_settings_fail_before_thread_creation() {
        let cases = [
            CaptureConfig {
                sample_rate: 0,
                ..CaptureConfig::default()
            },
            CaptureConfig {
                channels: 0,
                ..CaptureConfig::default()
            },
            CaptureConfig {
                channels: 65,
                ..CaptureConfig::default()
            },
            CaptureConfig {
                buffer_frames: 0,
                ..CaptureConfig::default()
            },
        ];
        for config in cases {
            assert!(spawn_audio_capture(config, None, 1).is_err());
        }
        assert!(spawn_audio_capture(CaptureConfig::default(), None, 0).is_err());
    }

    #[test]
    fn decoding_uses_chunk_offset_and_complete_stereo_frames() {
        let bytes = [0xaa, 0xbb, 0x34, 0x12, 0xfe, 0xff, 0x78, 0x56, 0xdd];
        let samples = decode_chunk(
            &bytes,
            2,
            7,
            spa::param::audio::AudioFormat::S16LE,
            2,
            AudioFormat::I16,
        )
        .unwrap();
        assert_eq!(samples.to_i16(), [0x1234, -2]);
        assert!(
            decode_chunk(
                &[],
                0,
                4,
                spa::param::audio::AudioFormat::S16LE,
                2,
                AudioFormat::I16
            )
            .is_none()
        );
        assert!(
            decode_chunk(
                &bytes,
                0,
                3,
                spa::param::audio::AudioFormat::S16LE,
                2,
                AudioFormat::I16
            )
            .is_none()
        );
    }

    #[test]
    fn decoding_wraps_offsets_and_clamps_chunk_size() {
        let bytes = [0x22, 0x22, 0x33, 0x33, 0x44, 0x44, 0x11, 0x11];
        let samples = decode_chunk(
            &bytes,
            14,
            100,
            spa::param::audio::AudioFormat::S16LE,
            2,
            AudioFormat::I16,
        )
        .unwrap();
        assert_eq!(samples.to_i16(), [0x1111, 0x2222, 0x3333, 0x4444]);
    }

    #[test]
    fn decoding_preserves_big_endian_float_signal_and_clips_pcm() {
        let bytes = [-0.5_f32, 0.5, -2.0, 2.0]
            .into_iter()
            .flat_map(f32::to_be_bytes)
            .collect::<Vec<_>>();
        let samples = decode_chunk(
            &bytes,
            0,
            bytes.len(),
            spa::param::audio::AudioFormat::F32BE,
            2,
            AudioFormat::I16,
        )
        .unwrap();
        assert_eq!(samples.to_i16(), [-16_383, 16_383, -32_767, 32_767]);
    }

    #[test]
    fn stopped_audio_workers_preserve_process_wide_pipewire_initialization() {
        let directory = tempfile::tempdir().unwrap();
        let invalid_remote = directory.path().join("not-a-socket");
        std::fs::write(&invalid_remote, []).unwrap();
        for _ in 0..2 {
            let config = CaptureConfig {
                remote_name: Some(invalid_remote.to_string_lossy().into_owned()),
                ..CaptureConfig::default()
            };
            let mut handle = spawn_audio_capture(config, None, 1).unwrap();
            // Sender closure occurs after the worker teardown has completed.
            assert!(handle.receiver.blocking_recv().is_none());
            assert!(handle.is_stopped());
            let main_loop = pw::main_loop::MainLoopBox::new(None);
            assert!(
                main_loop.is_ok(),
                "another connection must be able to create a PipeWire loop"
            );
        }
    }

    #[test]
    fn dropping_capture_handle_stops_a_silent_worker() {
        let (capture, handle) = AudioCapture::new(CaptureConfig::default(), 1);
        drop(handle);
        assert!(capture.stop_signal.load(Ordering::SeqCst));
        assert!(capture.sender.is_closed());
    }

    #[test]
    fn socket_readiness_is_bounded_and_accepts_delayed_unix_socket() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("pipewire-0");
        let stopped = AtomicBool::new(false);
        assert!(wait_for_remote(socket.to_str(), &stopped, Duration::ZERO).is_err());
        let path = socket.clone();
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            std::os::unix::net::UnixListener::bind(path).unwrap()
        });
        assert!(wait_for_remote(socket.to_str(), &stopped, Duration::from_secs(1)).unwrap());
        let listener = publisher.join().unwrap();
        drop(listener);
        stopped.store(true, Ordering::SeqCst);
        assert!(!wait_for_remote(socket.to_str(), &stopped, Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn stopping_capture_cancels_pending_socket_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("pipewire-0");
        let stopped = Arc::new(AtomicBool::new(false));
        let signal = stopped.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            signal.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        assert!(!wait_for_remote(socket.to_str(), &stopped, Duration::from_secs(5)).unwrap());
        assert!(started.elapsed() < Duration::from_secs(1));
        canceller.join().unwrap();
    }

    #[test]
    fn socket_readiness_rejects_regular_files_and_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("file");
        std::fs::write(&file, []).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        for path in [&file, &link] {
            assert!(
                wait_for_remote(path.to_str(), &AtomicBool::new(false), Duration::ZERO).is_err()
            );
        }
    }

    #[test]
    fn empty_buffer_and_stop_handle_have_observable_state() {
        let samples = AudioSamples::I16(Vec::new());
        assert_eq!(samples.len(), 0);
        assert!(samples.is_empty());

        let (_, handle) = AudioCapture::new(CaptureConfig::default(), 1);
        assert!(!handle.is_stopped());
        handle.stop();
        assert!(handle.is_stopped());
    }
}
