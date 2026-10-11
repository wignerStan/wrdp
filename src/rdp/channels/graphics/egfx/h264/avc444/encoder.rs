//! AVC444 encoder for EGFX runtime paths.
//!
//! Encodes BGRA frames into main/aux H.264 streams, keeps stream state aligned
//! across paired updates, and exposes timing/stats for runtime diagnostics.

#[cfg(feature = "h264")]
use tracing::{debug, info, trace, warn};

use crate::rdp::channels::graphics::egfx::{
    color::{convert::ColorMatrix, space::ColorSpaceConfig},
    h264::{
        encoder::{EncoderConfig, EncoderError, EncoderResult},
        level::H264Level,
    },
};

#[cfg(feature = "h264")]
use crate::rdp::channels::graphics::egfx::{
    color::{convert::bgra_to_yuv444, space::ColorRange, yuv444_packing::pack_dual_views},
    h264::encoder,
};

/// AVC444 encoded frame (dual H.264 bitstreams)
///
/// Every full-frame main update is followed by an auxiliary update. MS-RDPEGFX
/// 3.3.8.3.2 converts received main regions as YUV420; cached auxiliary data alone
/// does not preserve YUV444 color after another main update.
#[derive(Debug)]
pub struct Avc444Frame {
    /// Main view bitstream (full luma + subsampled chroma)
    ///
    /// Always present - contains primary visual information
    pub stream1_data: Vec<u8>,

    /// Auxiliary view bitstream (additional chroma data).
    ///
    /// The encoder produces `Some(data)` for every submitted full-frame update.
    /// `None` represents a main-only LC 1 update and has YUV420 color fidelity.
    pub stream2_data: Option<Vec<u8>>,

    /// Whether this is a keyframe (IDR) in main stream
    pub is_keyframe: bool,

    /// Frame timestamp in milliseconds
    pub timestamp_ms: u64,

    /// Total encoded size (stream1 + stream2 if present)
    ///
    /// When stream2 is omitted, this only reflects main stream size,
    /// providing accurate bandwidth measurement
    pub total_size: usize,

    /// Encoding time breakdown (for performance monitoring)
    pub timing: Avc444Timing,
}

/// Timing breakdown for AVC444 encoding
#[derive(Debug, Clone, Default)]
pub struct Avc444Timing {
    /// Time for BGRA → YUV444 conversion (ms)
    pub color_convert_ms: f32,
    /// Time for YUV444 → dual YUV420 packing (ms)
    pub packing_ms: f32,
    /// Time for dual H.264 encoding (ms)
    pub encoding_ms: f32,
    /// Total time (ms)
    pub total_ms: f32,
}

/// AVC444 encoder statistics
#[derive(Debug, Clone)]
pub struct Avc444Stats {
    /// Total AVC444 frames produced
    pub frames_encoded: u64,
    /// Total bytes encoded (both streams)
    pub bytes_encoded: u64,
    /// Average encoding time (ms)
    pub avg_encode_time_ms: f32,
    /// Configured bitrate (kbps)
    pub bitrate_kbps: u32,
    /// Color matrix in use
    pub color_matrix: ColorMatrix,
}

/// AVC444 Encoder
///
/// Encodes BGRA frames to dual YUV420 H.264 bitstreams for AVC444 transmission.
///
/// # Usage
///
/// ```rust,ignore
/// use wrdp::rdp::channels::graphics::egfx::{Avc444Encoder, EncoderConfig};
///
/// let config = EncoderConfig::default();
/// let mut encoder = Avc444Encoder::new(config)?;
///
/// let frame = encoder.encode_bgra(&bgra_data, 1920, 1080, timestamp)?;
/// if let Some(frame) = frame {
///     // Send frame.stream1_data and frame.stream2_data via EGFX
/// }
/// ```
#[cfg(feature = "h264")]
pub struct Avc444Encoder {
    /// SINGLE H.264 encoder for BOTH Main and Aux subframes
    ///
    /// MS-RDPEGFX spec requirement: "The two subframe bitstreams MUST be
    /// encoded using the same H.264 encoder" (Section 3.3.8.3.2)
    ///
    /// This ensures unified DPB (Decoded Picture Buffer) timeline between
    /// Main and Aux, preventing cross-stream reference corruption.
    encoder: ::openh264::encoder::Encoder,

    /// Configuration
    config: EncoderConfig,

    /// Color space configuration (includes matrix + VUI parameters)
    color_space: ColorSpaceConfig,

    /// Color matrix for RGB→YUV conversion (derived from color_space)
    color_matrix: ColorMatrix,

    /// Frame counter
    frame_count: u64,

    /// Total bytes encoded
    bytes_encoded: u64,

    /// Sum of encoding times (for average calculation)
    total_encode_time_ms: f64,

    /// Cached SPS/PPS (shared across both subframes with single encoder)
    cached_sps_pps: Option<Vec<u8>>,

    /// Current H.264 level
    current_level: Option<H264Level>,

    // === DIAGNOSTIC FLAGS ===
    /// Force all frames to be keyframes (disable P-frames)
    /// Set to true to diagnose P-frame specific color issues
    force_all_keyframes: bool,

    // === PERIODIC IDR (ARTIFACT RECOVERY) ===
    /// Time of last IDR keyframe (for periodic forced IDR)
    last_idr_time: std::time::Instant,

    /// Interval in seconds for forced IDR keyframes (0 = disabled)
    /// Forces full IDR at regular intervals to clear accumulated artifacts.
    /// Recommended: 5-10 seconds for VDI, 2-3 for unreliable networks.
    periodic_idr_interval_secs: u32,

    /// Flag to force next frame as IDR (set by client PLI or periodic timer)
    force_next_idr: bool,
}

#[cfg(feature = "h264")]
impl Avc444Encoder {
    pub fn new(config: EncoderConfig) -> EncoderResult<Self> {
        // Determine color space configuration:
        // 1. Use explicit config if provided
        // 2. Otherwise, auto-select based on resolution (BT.709 for HD, BT.601 for SD)
        let color_space = config.color_space.unwrap_or({
            match (config.width, config.height) {
                (Some(w), Some(h)) if w >= 1280 && h >= 720 => ColorSpaceConfig::BT709_FULL,
                (Some(_), Some(_)) => ColorSpaceConfig::BT601_LIMITED,
                // Default to BT.709 when dimensions unknown (will be HD in most cases)
                _ => ColorSpaceConfig::BT709_FULL,
            }
        });
        let color_matrix = color_space.matrix;

        // Both views share one H.264 sequence. Rate control and Annex A level
        // limits must cover two coded pictures per desktop frame.
        let coded_fps = config.max_fps * 2.0;
        let level = config
            .width
            .zip(config.height)
            .map(|(w, h)| H264Level::for_config(w, h, coded_fps));

        // Build VuiConfig from ColorSpaceConfig for H.264 SPS signaling
        let vui = match (color_space.matrix, color_space.range) {
            (ColorMatrix::BT709, ColorRange::Full) => ::openh264::encoder::VuiConfig::bt709_full(),
            (ColorMatrix::BT709, ColorRange::Limited) => ::openh264::encoder::VuiConfig::bt709(),
            (ColorMatrix::BT601 | ColorMatrix::OpenH264, _) => {
                ::openh264::encoder::VuiConfig::bt601()
            }
        };

        use ::openh264::encoder::{
            BitRate, Complexity, EncoderConfig as UpstreamConfig, FrameRate, UsageType,
        };
        // OpenH264 2.6 forces scene-change detection on in screen mode,
        // even when requested off. Alternating packed views then become new
        // scenes. Camera mode permits one continuous main/aux sequence.
        let mut compat_config = UpstreamConfig::new()
            .bitrate(BitRate::from_bps(config.bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(coded_fps))
            .usage_type(UsageType::CameraVideoRealTime)
            // Per-picture skips could drop auxiliary while submitting main,
            // which downgrades its full-frame region to YUV420 color.
            .skip_frames(false)
            .complexity(Complexity::High)
            .scene_change_detect(false)
            .vui(vui);
        if let Some(level) = level {
            compat_config = compat_config.level(level.to_upstream());
        }

        info!("AVC444: High complexity, OpenH264 default QP (0-51), VUI enabled");

        // Create SINGLE encoder for both Main and Aux (MS-RDPEGFX spec compliant)
        let api = encoder::load_openh264_api()?;
        let encoder =
            ::openh264::encoder::Encoder::with_api_config(api, compat_config).map_err(|e| {
                EncoderError::InitFailed(format!("AVC444 single encoder init failed: {e}"))
            })?;

        debug!(
            "Created AVC444 SINGLE encoder: {} color space, {}kbps, level={:?}",
            color_space.description(),
            config.bitrate_kbps,
            level
        );
        info!(
            "AVC444: VUI enabled ({}, primaries={}, transfer={}, matrix={})",
            if color_space.range == ColorRange::Full {
                "full range"
            } else {
                "limited range"
            },
            color_space.vui_colour_primaries(),
            color_space.vui_transfer_characteristics(),
            color_space.vui_matrix_coefficients()
        );

        Ok(Self {
            encoder,
            config,
            color_space,
            color_matrix,
            frame_count: 0,
            bytes_encoded: 0,
            total_encode_time_ms: 0.0,
            cached_sps_pps: None,
            current_level: level,
            // DIAGNOSTIC FLAG: Force all keyframes to disable P-frame inter-prediction
            force_all_keyframes: false, // Re-enabled P-frames with temporal logging
            // Periodic IDR defaults (overridden by configure_periodic_idr)
            last_idr_time: std::time::Instant::now(),
            periodic_idr_interval_secs: 5, // Default 5 seconds
            force_next_idr: false,
        })
    }

    /// Create encoder with specific color matrix
    ///
    /// Compatibility note: use `EncoderConfig::with_color_space()` for full VUI support.
    /// This method only affects the conversion matrix, not VUI signaling.
    #[deprecated(note = "Use EncoderConfig::with_color_space() for full VUI support")]
    pub fn with_color_matrix(config: EncoderConfig, matrix: ColorMatrix) -> EncoderResult<Self> {
        let mut encoder = Self::new(config)?;
        encoder.color_matrix = matrix;
        Ok(encoder)
    }

    pub fn with_color_space(
        mut config: EncoderConfig,
        color_space: ColorSpaceConfig,
    ) -> EncoderResult<Self> {
        config.color_space = Some(color_space);
        Self::new(config)
    }

    /// Accept legacy auxiliary-omission configuration without reducing color fidelity.
    ///
    /// Full-frame main regions overwrite color using YUV420 (MS-RDPEGFX
    /// 3.3.8.3.2). The encoder must therefore send auxiliary on every update.
    /// Omission needs region-aware main masks before it can be enabled safely.
    pub fn configure_aux_omission(
        &mut self,
        enable: bool,
        _max_interval: u32,
        _force_idr_on_return: bool,
    ) {
        if enable {
            warn!("AVC444 auxiliary omission ignored: full-frame main updates require auxiliary");
        }
    }

    /// Configure periodic IDR keyframe insertion
    ///
    /// Forces a full IDR keyframe at regular intervals to clear accumulated
    /// compression artifacts. This is especially important for VDI where
    /// artifacts from window movement can persist.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut encoder = Avc444Encoder::new(config)?;
    /// encoder.configure_periodic_idr(5); // Force IDR every 5 seconds
    /// ```
    pub fn configure_periodic_idr(&mut self, interval_secs: u32) {
        self.periodic_idr_interval_secs = interval_secs;
        self.last_idr_time = std::time::Instant::now();

        if interval_secs > 0 {
            info!(
                "🎬 Periodic IDR ENABLED: interval={}s (clears artifacts automatically)",
                interval_secs
            );
        } else {
            debug!("Periodic IDR disabled");
        }
    }

    /// Request immediate IDR keyframe (for client PLI - Picture Loss Indication)
    ///
    /// Called when the client reports visual artifacts or packet loss.
    /// The next encoded frame will be a full IDR keyframe.
    pub fn request_idr(&mut self) {
        self.force_next_idr = true;
        debug!("IDR requested (PLI or manual trigger)");
    }

    /// Check if periodic IDR is due (non-consuming check)
    ///
    /// This allows callers to know if the next encode will trigger a periodic IDR
    /// WITHOUT actually triggering it. Useful for forcing full-frame damage when
    /// periodic IDR is about to fire, ensuring the entire screen gets refreshed.
    pub fn is_periodic_idr_due(&self) -> bool {
        if self.force_next_idr {
            return true;
        }
        if self.periodic_idr_interval_secs > 0 {
            let elapsed = self.last_idr_time.elapsed();
            return elapsed
                >= std::time::Duration::from_secs(self.periodic_idr_interval_secs as u64);
        }
        false
    }

    /// Check if the shared main/aux sequence needs an IDR due to the timer or PLI.
    fn should_force_idr(&mut self) -> bool {
        // Check PLI request first
        if self.force_next_idr {
            self.force_next_idr = false;
            self.last_idr_time = std::time::Instant::now();
            info!("Forcing IDR (client PLI request) - both Main and Aux will refresh");
            return true;
        }

        // Check periodic interval
        if self.periodic_idr_interval_secs > 0 {
            let elapsed = self.last_idr_time.elapsed();
            if elapsed >= std::time::Duration::from_secs(self.periodic_idr_interval_secs as u64) {
                self.last_idr_time = std::time::Instant::now();
                info!(
                    "Forcing periodic IDR ({}s elapsed) - BOTH Main and Aux will refresh to clear artifacts",
                    elapsed.as_secs()
                );
                return true;
            }
        }

        false
    }

    #[expect(clippy::unwrap_used, reason = "timing subtraction is always valid")]
    pub fn encode_bgra(
        &mut self,
        bgra_data: &[u8],
        width: u32,
        height: u32,
        timestamp_ms: u64,
    ) -> EncoderResult<Option<Avc444Frame>> {
        let start = std::time::Instant::now();

        // Validate dimensions
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(EncoderError::InvalidDimensions { width, height });
        }

        let expected_size = (width * height * 4) as usize;
        if bgra_data.len() < expected_size {
            return Err(EncoderError::EncodeFailed(format!(
                "BGRA buffer too small: {} < {}",
                bgra_data.len(),
                expected_size
            )));
        }

        // Step 1: BGRA → YUV444
        let yuv444 = bgra_to_yuv444(
            bgra_data,
            width as usize,
            height as usize,
            self.color_matrix,
        );
        let convert_time = start.elapsed();

        // Step 2: YUV444 → Dual YUV420
        let (main_yuv420, aux_yuv420) = pack_dual_views(&yuv444);
        let pack_time = start.elapsed().checked_sub(convert_time).unwrap();

        // Step 3: Encode both views using direct YUV input via version-aware FFI
        //
        // NOTE: We use logical dimensions (width x height) not actual buffer dimensions.
        // The YUV420 frames may have padded chroma planes for macroblock alignment,
        // but OpenH264 expects logical dimensions and will do its own padding internally.

        // === PERIODIC IDR: Force keyframe at regular intervals or on PLI ===
        if self.should_force_idr() {
            self.encoder.force_intra_frame();
        }

        // === DIAGNOSTIC: Force keyframes if flag is set ===
        if self.force_all_keyframes {
            self.encoder.force_intra_frame();
            if self.frame_count == 0 {
                debug!("DIAGNOSTIC: force_all_keyframes=true - All frames will be IDR");
            }
        }

        // === SINGLE ENCODER: SEQUENTIAL ENCODING ===
        //
        // MS-RDPEGFX Section 3.3.8.3.2: "The two subframe bitstreams MUST be
        // encoded using the same H.264 encoder"
        //
        // 1. Encode Main subframe -> Updates unified DPB
        // 2. Encode Aux subframe -> SAME DPB, sequential call

        // Encode Main subframe FIRST (luma + subsampled chroma)
        let main_encoded = self
            .encoder
            .encode_at(
                &main_yuv420,
                ::openh264::Timestamp::from_millis(timestamp_ms),
            )
            .map_err(|e| {
                EncoderError::EncodeFailed(format!("Main subframe encoding failed: {e}"))
            })?;
        let main_is_keyframe = matches!(
            main_encoded.frame_type(),
            ::openh264::encoder::FrameType::IDR | ::openh264::encoder::FrameType::I
        );
        let main_frame_type_str = if main_is_keyframe { "IDR" } else { "P" };
        // EncodedBitStream borrows the encoder; copy before the second encode.
        let mut stream1_data = main_encoded.to_vec();

        // Main regions cover the full frame. Each main update is converted as
        // YUV420 by the client, so auxiliary must follow even if it is unchanged.
        // Encode and submit both pictures in the same shared decoder sequence.
        // A skipped main picture cannot be submitted as a complete desktop
        // update. Do not advance the shared encoder with an auxiliary picture
        // that the client will never receive.
        if stream1_data.is_empty() {
            trace!("AVC444 encoder skipped frame (main stream empty)");
            return Ok(None);
        }

        // The main IDR resets the shared decoded-picture buffer. The following
        // auxiliary picture can reference that main without another IDR.
        let aux_encoded = self
            .encoder
            .encode_at(
                &aux_yuv420,
                ::openh264::Timestamp::from_millis(timestamp_ms),
            )
            .map_err(|e| {
                EncoderError::EncodeFailed(format!("Aux subframe encoding failed: {e}"))
            })?;
        let aux_is_keyframe = matches!(
            aux_encoded.frame_type(),
            ::openh264::encoder::FrameType::IDR | ::openh264::encoder::FrameType::I
        );
        let aux_data = aux_encoded.to_vec();
        if aux_data.is_empty() {
            // The shared encoder has already advanced past the unsent main.
            // Suppress this incomplete update and reset its references before retry.
            self.request_idr();
            warn!("AVC444 auxiliary picture empty; requesting a complete IDR update");
            return Ok(None);
        }
        let aux_bitstream_opt = Some((aux_data, aux_is_keyframe));

        let encode_time = start
            .elapsed()
            .checked_sub(convert_time)
            .and_then(|d| d.checked_sub(pack_time))
            .unwrap_or_default();

        let (stream2_data_opt, aux_is_keyframe) = match aux_bitstream_opt {
            Some((data, keyframe)) => (Some(data), keyframe),
            None => (None, false),
        };

        // Diagnostic logging for bandwidth analysis
        if let Some(ref aux_data) = stream2_data_opt {
            let aux_type_str = if aux_is_keyframe { "IDR" } else { "P" };
            debug!(
                "[AVC444 Frame #{}] Main: {} ({}B), Aux: {} ({}B) [BOTH SENT]",
                self.frame_count,
                main_frame_type_str,
                stream1_data.len(),
                aux_type_str,
                aux_data.len()
            );
        } else {
            debug!(
                "[AVC444 Frame #{}] Main: {} ({}B), Aux: OMITTED (LC=1) [BANDWIDTH SAVE]",
                self.frame_count,
                main_frame_type_str,
                stream1_data.len()
            );
        }

        // Handle SPS/PPS caching for main stream P-frames.
        // Cached SPS/PPS from the latest IDR is prepended to P-frames so the
        // Windows MFT decoder always has parameter context.
        stream1_data = self.handle_sps_pps(stream1_data, main_is_keyframe);

        // Retain any parameter sets supplied by the auxiliary encode. The
        // two views form one decoder sequence (MS-RDPEGFX 2.2.4.5), and their
        // Annex B parameter sets must remain in their original order.

        // Update statistics
        self.frame_count += 1;
        let total_size =
            stream1_data.len() + stream2_data_opt.as_ref().map_or(0, std::vec::Vec::len);
        self.bytes_encoded += total_size as u64;

        let total_time = start.elapsed();
        self.total_encode_time_ms += total_time.as_secs_f64() * 1000.0;

        let timing = Avc444Timing {
            color_convert_ms: convert_time.as_secs_f32() * 1000.0,
            packing_ms: pack_time.as_secs_f32() * 1000.0,
            encoding_ms: encode_time.as_secs_f32() * 1000.0,
            total_ms: total_time.as_secs_f32() * 1000.0,
        };

        // Periodic logging with omission statistics
        if self.frame_count.is_multiple_of(30) {
            let aux_size_display = stream2_data_opt.as_ref().map_or(0, std::vec::Vec::len);
            let omission_status = if stream2_data_opt.is_some() {
                "sent"
            } else {
                "omitted"
            };

            debug!(
                "AVC444 frame {}: {}×{} → {}b (main: {}b, aux: {}b [{}]) in {:.1}ms",
                self.frame_count,
                width,
                height,
                total_size,
                stream1_data.len(),
                aux_size_display,
                omission_status,
                timing.total_ms,
            );
        }

        Ok(Some(Avc444Frame {
            stream1_data,
            stream2_data: stream2_data_opt, // Now Option<Vec<u8>>
            is_keyframe: main_is_keyframe,
            timestamp_ms,
            total_size,
            timing,
        }))
    }

    /// Handle SPS/PPS for main stream (cache on IDR, prepend on P-frame)
    ///
    /// With single encoder, SPS/PPS is shared across both subframes.
    /// Cache from main stream IDRs, prepend to main stream P-frames.
    #[expect(
        clippy::unwrap_used,
        reason = "cached_sps_pps is always Some when keyframe has been seen"
    )]
    fn handle_sps_pps(&mut self, mut data: Vec<u8>, is_keyframe: bool) -> Vec<u8> {
        if is_keyframe {
            // Cache SPS/PPS from this IDR
            if let Some(sps_pps) = Self::extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
                trace!(
                    "Cached SPS/PPS ({} bytes) from IDR",
                    self.cached_sps_pps.as_ref().unwrap().len()
                );
            }
        } else if let Some(ref sps_pps) = self.cached_sps_pps {
            // Prepend cached SPS/PPS to P-frame
            let mut combined = sps_pps.clone();
            combined.extend_from_slice(&data);
            data = combined;
            trace!("Prepended SPS/PPS to P-frame");
        }
        data
    }

    /// Extract SPS and PPS NAL units from Annex B bitstream
    fn extract_sps_pps(data: &[u8]) -> Option<Vec<u8>> {
        let mut sps_pps = Vec::new();
        let mut i = 0;

        while i < data.len() {
            // Find start code
            let start_code_len =
                if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
                    4
                } else if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
                    3
                } else {
                    i += 1;
                    continue;
                };

            let nal_start = i + start_code_len;
            if nal_start >= data.len() {
                break;
            }

            let nal_type = data[nal_start] & 0x1F;

            // Find next start code
            let mut nal_end = data.len();
            let mut j = nal_start + 1;
            while j + 2 < data.len() {
                if (data[j..j + 3] == [0x00, 0x00, 0x01])
                    || (j + 3 < data.len() && data[j..j + 4] == [0x00, 0x00, 0x00, 0x01])
                {
                    nal_end = j;
                    break;
                }
                j += 1;
            }

            // NAL type 7 = SPS, NAL type 8 = PPS
            if nal_type == 7 || nal_type == 8 {
                sps_pps.extend_from_slice(&data[i..nal_end]);
            }

            i = nal_end;
            if i == data.len() {
                break;
            }
        }

        if sps_pps.is_empty() {
            None
        } else {
            Some(sps_pps)
        }
    }

    /// Refresh both views after a main IDR resets the shared decoder state.
    pub fn force_keyframe(&mut self) {
        self.request_idr();
    }

    pub fn stats(&self) -> Avc444Stats {
        Avc444Stats {
            frames_encoded: self.frame_count,
            bytes_encoded: self.bytes_encoded,
            avg_encode_time_ms: if self.frame_count > 0 {
                (self.total_encode_time_ms / self.frame_count as f64) as f32
            } else {
                0.0
            },
            bitrate_kbps: self.config.bitrate_kbps * 2, // Two streams
            color_matrix: self.color_matrix,
        }
    }

    pub fn color_matrix(&self) -> ColorMatrix {
        self.color_matrix
    }

    pub fn color_space(&self) -> &ColorSpaceConfig {
        &self.color_space
    }

    pub fn level(&self) -> Option<H264Level> {
        self.current_level
    }
}

// Stub implementation when h264 feature is disabled
#[cfg(not(feature = "h264"))]
pub struct Avc444Encoder;

#[cfg(not(feature = "h264"))]
impl Avc444Encoder {
    pub fn new(_config: EncoderConfig) -> EncoderResult<Self> {
        Err(EncoderError::FeatureDisabled)
    }

    pub fn encode_bgra(
        &mut self,
        _bgra_data: &[u8],
        _width: u32,
        _height: u32,
        _timestamp_ms: u64,
    ) -> EncoderResult<Option<Avc444Frame>> {
        Err(EncoderError::FeatureDisabled)
    }

    pub fn force_keyframe(&mut self) {}

    pub fn stats(&self) -> Avc444Stats {
        Avc444Stats {
            frames_encoded: 0,
            bytes_encoded: 0,
            avg_encode_time_ms: 0.0,
            bitrate_kbps: 0,
            color_matrix: ColorMatrix::BT709,
        }
    }

    pub fn color_matrix(&self) -> ColorMatrix {
        ColorMatrix::BT709
    }

    pub fn color_space(&self) -> &ColorSpaceConfig {
        // Return a static reference for the stub
        &ColorSpaceConfig::BT709_FULL
    }

    pub fn level(&self) -> Option<H264Level> {
        None
    }

    pub fn configure_aux_omission(
        &mut self,
        _enable: bool,
        _max_interval: u32,
        _force_idr_on_return: bool,
    ) {
    }
    pub fn request_idr(&mut self) {}
    pub fn is_periodic_idr_due(&self) -> bool {
        false
    }
    pub fn configure_periodic_idr(&mut self, _interval_frames: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Try to create an encoder, returning early if OpenH264 library is not installed.
    /// Tests that require the library call this instead of unwrap() so CI environments
    /// without an OpenH264 runtime skip gracefully rather than failing.
    #[cfg(feature = "h264")]
    macro_rules! require_openh264 {
        ($expr:expr) => {
            match $expr {
                Ok(enc) => enc,
                Err(
                    crate::rdp::channels::graphics::egfx::h264::encoder::EncoderError::InitFailed(
                        _,
                    ),
                ) => return,
                Err(e) => panic!("unexpected encoder error: {e:?}"),
            }
        };
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "comparing against zero defaults")]
    fn test_avc444_timing_default() {
        let timing = Avc444Timing::default();
        assert_eq!(timing.color_convert_ms, 0.0);
        assert_eq!(timing.packing_ms, 0.0);
        assert_eq!(timing.encoding_ms, 0.0);
        assert_eq!(timing.total_ms, 0.0);
    }

    #[test]
    fn test_avc444_stats_default() {
        let stats = Avc444Stats {
            frames_encoded: 0,
            bytes_encoded: 0,
            avg_encode_time_ms: 0.0,
            bitrate_kbps: 5000,
            color_matrix: ColorMatrix::BT709,
        };
        assert_eq!(stats.frames_encoded, 0);
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_avc444_encoder_creation() {
        let config = EncoderConfig::default();
        let _encoder = require_openh264!(Avc444Encoder::new(config));
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_avc444_encoder_with_resolution() {
        let config = EncoderConfig {
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        };
        let encoder = require_openh264!(Avc444Encoder::new(config));
        assert_eq!(encoder.color_matrix(), ColorMatrix::BT709);
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_avc444_encoder_sd_resolution() {
        let config = EncoderConfig {
            width: Some(640),
            height: Some(480),
            ..Default::default()
        };
        let encoder = require_openh264!(Avc444Encoder::new(config));
        assert_eq!(encoder.color_matrix(), ColorMatrix::BT601);
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_encode_black_frame() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let width = 64u32;
        let height = 64u32;
        let bgra_data = vec![0u8; (width * height * 4) as usize];

        let result = encoder.encode_bgra(&bgra_data, width, height, 0);
        assert!(result.is_ok(), "Encoding failed: {:?}", result.err());

        if let Ok(Some(frame)) = result {
            assert!(!frame.stream1_data.is_empty(), "Stream 1 is empty");
            // stream2_data is Option<Vec<u8>> - may be None with aux omission
            if let Some(ref stream2) = frame.stream2_data {
                assert!(!stream2.is_empty(), "Stream 2 is empty");
                assert_eq!(frame.total_size, frame.stream1_data.len() + stream2.len());
            }
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_encode_colored_frame() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let width = 64u32;
        let height = 64u32;
        let mut bgra_data = vec![0u8; (width * height * 4) as usize];

        // Create a gradient pattern
        for y in 0..height {
            for x in 0..width {
                let idx = ((y * width + x) * 4) as usize;
                bgra_data[idx] = ((x * 4) % 256) as u8; // B
                bgra_data[idx + 1] = ((y * 4) % 256) as u8; // G
                bgra_data[idx + 2] = 128; // R
                bgra_data[idx + 3] = 255; // A
            }
        }

        let result = encoder.encode_bgra(&bgra_data, width, height, 0);
        assert!(result.is_ok());
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_invalid_dimensions() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        // Odd width
        let bgra_data = vec![0u8; 63 * 64 * 4];
        let result = encoder.encode_bgra(&bgra_data, 63, 64, 0);
        assert!(matches!(
            result,
            Err(EncoderError::InvalidDimensions { .. })
        ));

        // Odd height
        let bgra_data = vec![0u8; 64 * 63 * 4];
        let result = encoder.encode_bgra(&bgra_data, 64, 63, 0);
        assert!(matches!(
            result,
            Err(EncoderError::InvalidDimensions { .. })
        ));

        // Zero dimension
        let result = encoder.encode_bgra(&[], 0, 64, 0);
        assert!(matches!(
            result,
            Err(EncoderError::InvalidDimensions { .. })
        ));
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_buffer_too_small() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        // Buffer smaller than expected
        let bgra_data = vec![0u8; 64 * 32 * 4]; // Only half the expected size
        let result = encoder.encode_bgra(&bgra_data, 64, 64, 0);
        assert!(matches!(result, Err(EncoderError::EncodeFailed(_))));
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_force_keyframe() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        // Should not panic
        encoder.force_keyframe();
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_stats() {
        let config = EncoderConfig {
            bitrate_kbps: 5000,
            ..Default::default()
        };
        let encoder = require_openh264!(Avc444Encoder::new(config));
        let stats = encoder.stats();

        assert_eq!(stats.frames_encoded, 0);
        assert_eq!(stats.bytes_encoded, 0);
        assert_eq!(stats.bitrate_kbps, 10000); // 2× for dual streams
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_multiple_frames() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let width = 64u32;
        let height = 64u32;
        let bgra_data = vec![128u8; (width * height * 4) as usize];

        // Encode multiple frames
        for i in 0..5 {
            let result = encoder.encode_bgra(&bgra_data, width, height, i * 33);
            assert!(result.is_ok(), "Frame {} failed: {:?}", i, result.err());
        }

        let stats = encoder.stats();
        assert!(stats.frames_encoded >= 1, "No frames encoded");
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_extract_sps_pps() {
        // Test data with SPS (NAL type 7) and PPS (NAL type 8)
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e, // SPS
            0x00, 0x00, 0x01, 0x68, 0xce, 0x3c, 0x80, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, // IDR slice
        ];

        #[cfg(feature = "h264")]
        {
            let sps_pps = Avc444Encoder::extract_sps_pps(&data);
            assert!(sps_pps.is_some());
            let headers = sps_pps.unwrap();
            // Should contain both SPS and PPS
            assert!(headers.len() > 8);
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    #[allow(deprecated)]
    fn test_with_color_matrix() {
        // Test explicit BT.601 on HD resolution (override auto-detection)
        let config = EncoderConfig {
            width: Some(1920),
            height: Some(1080),
            ..Default::default()
        };
        let encoder =
            require_openh264!(Avc444Encoder::with_color_matrix(config, ColorMatrix::BT601));
        assert_eq!(encoder.color_matrix(), ColorMatrix::BT601);
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_720p_hd_edge_case() {
        // 1280×720 is exactly at HD threshold (should use BT.709)
        let config = EncoderConfig {
            width: Some(1280),
            height: Some(720),
            ..Default::default()
        };
        let encoder = require_openh264!(Avc444Encoder::new(config));
        assert_eq!(encoder.color_matrix(), ColorMatrix::BT709);
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_below_720p_sd() {
        // 1279×719 is just below HD threshold (should use BT.601)
        let config = EncoderConfig {
            width: Some(1279),
            height: Some(719),
            ..Default::default()
        };
        let encoder = require_openh264!(Avc444Encoder::new(config));
        assert_eq!(encoder.color_matrix(), ColorMatrix::BT601);
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_timing_breakdown() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let width = 64u32;
        let height = 64u32;
        let bgra_data = vec![128u8; (width * height * 4) as usize];

        let result = encoder.encode_bgra(&bgra_data, width, height, 0);
        assert!(result.is_ok());

        if let Ok(Some(frame)) = result {
            // Verify timing breakdown is populated
            assert!(frame.timing.total_ms >= 0.0);
            assert!(frame.timing.color_convert_ms >= 0.0);
            assert!(frame.timing.packing_ms >= 0.0);
            assert!(frame.timing.encoding_ms >= 0.0);

            // Total should be sum of parts (with some tolerance for measurement)
            let sum =
                frame.timing.color_convert_ms + frame.timing.packing_ms + frame.timing.encoding_ms;
            assert!((frame.timing.total_ms - sum).abs() < 1.0);
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_encoder_level() {
        // 1080p at 30 desktop fps is a 60-picture/s decoder sequence.
        let config = EncoderConfig {
            width: Some(1920),
            height: Some(1080),
            max_fps: 30.0,
            ..Default::default()
        };
        let encoder = require_openh264!(Avc444Encoder::new(config));
        assert_eq!(encoder.level(), Some(H264Level::L4_2));
    }

    #[cfg(feature = "h264")]
    #[test]
    fn shared_encoder_uses_dual_picture_level_and_main_idr_reference() {
        for (width, height, level) in [(1280, 800, H264Level::L4_0), (1712, 1056, H264Level::L4_2)]
        {
            let config = EncoderConfig {
                width: Some(width),
                height: Some(height),
                max_fps: 30.0,
                enable_skip_frame: false,
                ..Default::default()
            };
            let mut encoder = require_openh264!(Avc444Encoder::new(config));
            encoder.configure_aux_omission(false, 30, false);
            encoder.configure_periodic_idr(0);
            let pixels = vec![100; usize::from(width) * usize::from(height) * 4];
            let frame = encoder
                .encode_bgra(&pixels, u32::from(width), u32::from(height), 0)
                .unwrap()
                .unwrap();
            assert_eq!(encoder.level(), Some(level));
            let nals = |data: &[u8]| {
                data.windows(5)
                    .filter(|v| v[..4] == [0, 0, 0, 1])
                    .map(|v| v[4] & 0x1f)
                    .collect::<Vec<_>>()
            };
            assert!(nals(&frame.stream1_data).contains(&5));
            let aux = frame.stream2_data.as_ref().unwrap();
            assert!(nals(aux).contains(&1), "aux NALs: {:?}", nals(aux));
            assert!(!nals(aux).contains(&5));
            // The actual SPS level_idc must match the dual-picture budget.
            let sps = frame
                .stream1_data
                .windows(8)
                .find(|v| v[..5] == [0, 0, 0, 1, 0x67])
                .unwrap();
            assert_eq!(sps[7], level as u8);
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    fn full_frame_updates_preserve_auxiliary_despite_legacy_omission_and_skip_settings() {
        for enable_omission in [false, true] {
            let config = EncoderConfig {
                width: Some(64),
                height: Some(64),
                bitrate_kbps: 1,
                enable_skip_frame: true,
                ..Default::default()
            };
            let mut encoder = require_openh264!(Avc444Encoder::new(config));
            encoder.configure_aux_omission(enable_omission, 120, true);
            encoder.configure_periodic_idr(0);
            // Red/blue stripes require the auxiliary view for full chroma.
            let pixels: Vec<u8> = (0..64 * 64)
                .flat_map(|pixel| {
                    if pixel % 2 == 0 {
                        [0, 0, 255, 255]
                    } else {
                        [255, 0, 0, 255]
                    }
                })
                .collect();
            for index in 0..6 {
                if index == 3 {
                    encoder.force_keyframe();
                }
                let frame = encoder
                    .encode_bgra(&pixels, 64, 64, index * 33)
                    .unwrap()
                    .expect("complete main/aux update");
                assert!(!frame.stream1_data.is_empty());
                assert!(
                    frame
                        .stream2_data
                        .as_ref()
                        .is_some_and(|aux| !aux.is_empty())
                );
                if index == 0 || index == 3 {
                    assert!(frame.is_keyframe);
                }
            }
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_1080p_encoding() {
        // Test encoding a 1080p frame
        let config = EncoderConfig {
            width: Some(1920),
            height: Some(1080),
            bitrate_kbps: 8000,
            ..Default::default()
        };
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let width = 1920u32;
        let height = 1080u32;
        let bgra_data = vec![100u8; (width * height * 4) as usize];

        let result = encoder.encode_bgra(&bgra_data, width, height, 0);
        assert!(result.is_ok(), "1080p encoding failed: {:?}", result.err());

        if let Ok(Some(frame)) = result {
            assert!(!frame.stream1_data.is_empty());
            // stream2_data is Option<Vec<u8>> - may be None with aux omission
            if let Some(ref stream2) = frame.stream2_data {
                assert!(!stream2.is_empty());
            }
            // 1080p keyframe should be substantial but not enormous
            assert!(frame.total_size > 1000, "1080p frame too small");
            assert!(
                frame.total_size < 10_000_000,
                "1080p frame unreasonably large"
            );
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_force_keyframe_produces_idr() {
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let width = 64u32;
        let height = 64u32;
        let bgra_data = vec![64u8; (width * height * 4) as usize];

        // First frame is always keyframe
        let result1 = encoder.encode_bgra(&bgra_data, width, height, 0);
        assert!(result1.is_ok());
        if let Ok(Some(frame)) = result1 {
            assert!(frame.is_keyframe, "First frame should be keyframe");
        }

        // Encode a few P-frames
        for i in 1..5 {
            let _ = encoder.encode_bgra(&bgra_data, width, height, i * 33);
        }

        // Force keyframe and verify
        encoder.force_keyframe();
        let result = encoder.encode_bgra(&bgra_data, width, height, 200);
        assert!(result.is_ok());
        if let Ok(Some(frame)) = result {
            assert!(frame.is_keyframe, "Forced frame should be keyframe");
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_stats_after_encoding() {
        let config = EncoderConfig {
            bitrate_kbps: 3000,
            ..Default::default()
        };
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let width = 64u32;
        let height = 64u32;
        let bgra_data = vec![200u8; (width * height * 4) as usize];

        // Encode several frames
        for i in 0..10 {
            let _ = encoder.encode_bgra(&bgra_data, width, height, i * 33);
        }

        let stats = encoder.stats();
        assert!(
            stats.frames_encoded >= 1,
            "Should have encoded at least 1 frame"
        );
        assert!(stats.bytes_encoded > 0, "Should have bytes encoded");
        assert!(
            stats.avg_encode_time_ms > 0.0,
            "Should have non-zero encode time"
        );
        assert_eq!(stats.bitrate_kbps, 6000, "Bitrate should be 2× configured");
    }

    #[test]
    fn test_extract_sps_pps_3byte_start_code() {
        // Test with only 3-byte start codes
        #[cfg(feature = "h264")]
        {
            let data = vec![
                0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e, // SPS (3-byte)
                0x00, 0x00, 0x01, 0x68, 0xce, 0x3c, 0x80, // PPS (3-byte)
            ];
            let sps_pps = Avc444Encoder::extract_sps_pps(&data);
            assert!(sps_pps.is_some());
        }
    }

    #[test]
    fn test_extract_sps_pps_empty() {
        // Test with no SPS/PPS
        #[cfg(feature = "h264")]
        {
            let data = vec![
                0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, // IDR only
            ];
            let sps_pps = Avc444Encoder::extract_sps_pps(&data);
            assert!(sps_pps.is_none());
        }
    }

    #[test]
    fn test_extract_sps_pps_sps_only() {
        // Test with only SPS (no PPS)
        #[cfg(feature = "h264")]
        {
            let data = vec![
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e, // SPS only
                0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, // IDR slice
            ];
            let sps_pps = Avc444Encoder::extract_sps_pps(&data);
            assert!(sps_pps.is_some());
            // Should contain just the SPS
            let headers = sps_pps.unwrap();
            assert!(headers.len() >= 8);
        }
    }

    #[cfg(feature = "h264")]
    #[test]
    fn test_variable_frame_sizes() {
        // Test that encoder handles different frame sizes correctly
        let config = EncoderConfig::default();
        let mut encoder = require_openh264!(Avc444Encoder::new(config));

        let test_sizes = [(64, 64), (128, 128), (256, 256), (320, 240)];

        for (width, height) in test_sizes {
            let bgra_data = vec![128u8; (width * height * 4) as usize];
            let result = encoder.encode_bgra(&bgra_data, width as u32, height as u32, 0);
            assert!(
                result.is_ok(),
                "Encoding {}×{} failed: {:?}",
                width,
                height,
                result.err()
            );
        }
    }

    #[test]
    fn test_avc444_frame_debug() {
        // Test that Avc444Frame derives Debug
        let frame = Avc444Frame {
            stream1_data: vec![1, 2, 3],
            stream2_data: Some(vec![4, 5, 6]), // Option<Vec<u8>> for aux omission
            is_keyframe: true,
            timestamp_ms: 100,
            total_size: 6,
            timing: Avc444Timing::default(),
        };
        let debug_str = format!("{frame:?}");
        assert!(debug_str.contains("Avc444Frame"));
        assert!(debug_str.contains("is_keyframe: true"));
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "comparing cloned float literals")]
    fn test_avc444_timing_clone() {
        let timing = Avc444Timing {
            color_convert_ms: 1.5,
            packing_ms: 2.5,
            encoding_ms: 10.0,
            total_ms: 14.0,
        };
        let cloned = timing.clone();
        assert_eq!(cloned.color_convert_ms, 1.5);
        assert_eq!(cloned.packing_ms, 2.5);
        assert_eq!(cloned.encoding_ms, 10.0);
        assert_eq!(cloned.total_ms, 14.0);
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "comparing cloned float literals")]
    fn test_avc444_stats_clone() {
        let stats = Avc444Stats {
            frames_encoded: 100,
            bytes_encoded: 50000,
            avg_encode_time_ms: 15.5,
            bitrate_kbps: 5000,
            color_matrix: ColorMatrix::BT709,
        };
        let cloned = stats.clone();
        assert_eq!(cloned.frames_encoded, 100);
        assert_eq!(cloned.bytes_encoded, 50000);
        assert_eq!(cloned.avg_encode_time_ms, 15.5);
        assert_eq!(cloned.color_matrix, ColorMatrix::BT709);
    }
}
