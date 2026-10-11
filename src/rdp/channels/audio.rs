//! Connection-scoped PipeWire desktop audio output over RDPSND.

use std::{path::PathBuf, sync::Arc, time::Instant};

use ironrdp_rdpsnd::{
    pdu::{AudioFormat, WaveFormat},
    server::{NegotiatedFormat, RdpsndError, RdpsndServerHandler},
};
use ironrdp_server::{RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::desktop::pipewire::audio::{
    AudioFormat as CaptureFormat, AudioSamples, CaptureConfig, spawn_audio_capture,
};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

/// Shared post-auth audio target installed by the session binder.
#[derive(Debug, Clone, Default)]
pub struct AudioTarget {
    runtime_dir: Arc<std::sync::RwLock<Option<PathBuf>>>,
}

impl AudioTarget {
    pub fn set_runtime_dir(&self, runtime_dir: PathBuf) {
        if let Ok(mut target) = self.runtime_dir.write() {
            *target = Some(runtime_dir);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut target) = self.runtime_dir.write() {
            target.take();
        }
    }
}

#[derive(Debug)]
pub struct WrdpSoundFactory {
    target: AudioTarget,
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl WrdpSoundFactory {
    pub fn new(target: AudioTarget) -> Self {
        Self {
            target,
            event_sender: None,
        }
    }
}

impl ServerEventSender for WrdpSoundFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

impl SoundServerFactory for WrdpSoundFactory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(PipeWireRdpsndHandler::new(
            self.target.clone(),
            self.event_sender.clone(),
        ))
    }
}

/// Serializes producer sends with connection teardown. The lock covers only
/// a nonblocking queue send; it never covers PipeWire or server state.
#[derive(Debug)]
struct AudioForwarding {
    active: std::sync::Mutex<bool>,
}

impl AudioForwarding {
    fn new() -> Self {
        Self {
            active: std::sync::Mutex::new(true),
        }
    }

    fn stop(&self) {
        *self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    }

    fn send(
        &self,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        message: RdpsndServerMessage,
    ) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active && sender.send(ServerEvent::Rdpsnd(message)).is_ok()
    }
}

struct PipeWireRdpsndHandler {
    target: AudioTarget,
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    formats: Vec<AudioFormat>,
    stop_signal: Option<Arc<std::sync::atomic::AtomicBool>>,
    forward_task: Option<tokio::task::JoinHandle<()>>,
    forwarding: Option<Arc<AudioForwarding>>,
}

impl std::fmt::Debug for PipeWireRdpsndHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireRdpsndHandler")
            .field("has_event_sender", &self.event_sender.is_some())
            .field("capture_active", &self.stop_signal.is_some())
            .finish()
    }
}

impl PipeWireRdpsndHandler {
    fn new(target: AudioTarget, event_sender: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        Self {
            target,
            event_sender,
            formats: vec![pcm_format()],
            stop_signal: None,
            forward_task: None,
            forwarding: None,
        }
    }

    fn stop_capture(&mut self) {
        // Disable sends first. When this returns, a producer that runs after
        // task abort cannot enqueue audio for a later RDP connection.
        if let Some(forwarding) = self.forwarding.take() {
            forwarding.stop();
        }
        if let Some(signal) = self.stop_signal.take() {
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(task) = self.forward_task.take() {
            task.abort();
        }
    }
}

impl Drop for PipeWireRdpsndHandler {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

impl RdpsndServerHandler for PipeWireRdpsndHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        common.first()
    }

    fn start(&mut self, negotiated: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        self.stop_capture();
        let format = negotiated.format();
        if format != &self.formats[0] {
            return Err(Box::new(std::io::Error::other(
                "RDPSND selected an unsupported PCM format",
            )));
        }
        let event_sender = self.event_sender.clone().ok_or_else(|| {
            Box::new(std::io::Error::other("RDPSND event sender unavailable"))
                as Box<dyn RdpsndError>
        })?;
        let runtime_dir = self
            .target
            .runtime_dir
            .read()
            .map_err(|_| {
                Box::new(std::io::Error::other("audio target lock poisoned"))
                    as Box<dyn RdpsndError>
            })?
            .clone()
            .ok_or_else(|| {
                Box::new(std::io::Error::other(
                    "authenticated PipeWire runtime is not bound",
                )) as Box<dyn RdpsndError>
            })?;
        let socket = runtime_dir.join("pipewire-0");

        // Connect directly to the authenticated user's PipeWire socket rather
        // than mutating process-wide environment in the multi-user daemon.
        let config = CaptureConfig {
            sample_rate: SAMPLE_RATE,
            channels: u32::from(CHANNELS),
            format: CaptureFormat::I16,
            buffer_frames: 960,
            remote_name: Some(socket.to_string_lossy().into_owned()),
        };
        let mut capture = spawn_audio_capture(config, None, 8).map_err(|error| {
            Box::new(std::io::Error::other(error.to_string())) as Box<dyn RdpsndError>
        })?;
        self.stop_signal = Some(capture.stop_signal());
        let forwarding = Arc::new(AudioForwarding::new());
        self.forwarding = Some(Arc::clone(&forwarding));
        self.forward_task = Some(tokio::spawn(async move {
            let started = Instant::now();
            while let Some(samples) = capture.receiver.recv().await {
                let integer_samples = match samples {
                    AudioSamples::I16(samples) => samples,
                    samples => samples.to_i16(),
                };
                let pcm = integer_samples
                    .into_iter()
                    .flat_map(i16::to_le_bytes)
                    .collect::<Vec<_>>();
                if pcm.is_empty() {
                    continue;
                }
                let timestamp = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                if !forwarding.send(&event_sender, RdpsndServerMessage::Wave(pcm, timestamp)) {
                    debug!("RDPSND forwarding stopped or event channel closed");
                    capture.stop();
                    return;
                }
            }
            warn!("PipeWire audio capture ended");
            let _ = forwarding.send(&event_sender, RdpsndServerMessage::Close);
        }));
        info!(runtime = %runtime_dir.display(), "PipeWire RDPSND PCM capture requested");
        Ok(())
    }

    fn stop(&mut self) {
        // IronRDP writes SNDC_CLOSE on the active connection before its
        // transport closes. A queued Close here could reach the next client.
        self.stop_capture();
    }
}

fn pcm_format() -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: SAMPLE_RATE,
        n_avg_bytes_per_sec: SAMPLE_RATE * u32::from(CHANNELS) * 2,
        n_block_align: CHANNELS * 2,
        bits_per_sample: BITS_PER_SAMPLE,
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_rdpsnd_backend_stops_connection_owned_capture() {
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handler = PipeWireRdpsndHandler::new(AudioTarget::default(), None);
        handler.stop_signal = Some(signal.clone());
        drop(handler);
        assert!(signal.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn advertises_standard_stereo_pcm() {
        let format = pcm_format();
        assert_eq!(format.format, WaveFormat::PCM);
        assert_eq!(format.n_samples_per_sec, 48_000);
        assert_eq!(format.n_channels, 2);
        assert_eq!(format.bits_per_sample, 16);
        assert_eq!(format.n_block_align, 4);
        assert_eq!(format.n_avg_bytes_per_sec, 192_000);
    }

    #[test]
    fn stopping_backend_rejects_late_waves_and_close_for_next_connection() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let signal = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let forwarding = Arc::new(AudioForwarding::new());
        let mut handler = PipeWireRdpsndHandler::new(AudioTarget::default(), Some(sender.clone()));
        handler.stop_signal = Some(signal.clone());
        handler.forwarding = Some(Arc::clone(&forwarding));

        handler.stop();
        assert!(signal.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!forwarding.send(&sender, RdpsndServerMessage::Wave(vec![0; 4], 10)));
        assert!(!forwarding.send(&sender, RdpsndServerMessage::Close));
        drop(handler);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn stopping_backend_aborts_forwarding_without_waiting_for_capture_thread() {
        struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let (started_sender, started) = tokio::sync::oneshot::channel();
        let (dropped_sender, dropped) = tokio::sync::oneshot::channel();
        let mut handler = PipeWireRdpsndHandler::new(AudioTarget::default(), None);
        handler.forward_task = Some(tokio::spawn(async move {
            let _guard = OnDrop(Some(dropped_sender));
            let _ = started_sender.send(());
            std::future::pending::<()>().await;
        }));
        tokio::time::timeout(std::time::Duration::from_secs(1), started)
            .await
            .expect("forwarder starts")
            .expect("started signal");
        handler.stop();
        tokio::time::timeout(std::time::Duration::from_secs(1), dropped)
            .await
            .expect("forwarder is cancelled")
            .expect("dropped signal");
    }
}
