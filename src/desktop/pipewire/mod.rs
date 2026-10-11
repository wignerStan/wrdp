#![cfg_attr(docsrs, feature(doc_cfg))]
// PipeWire integration crosses C FFI, mmap, raw file descriptors, and SPA buffer
// metadata. Keep unsafe allowances scoped to this module instead of relaxing the
// rest of the RDP/session/channel code.
#![allow(unsafe_code)]

//! # desktop::pipewire
//!
//! PipeWire integration for Wayland screen capture, buffer negotiation, and
//! frame delivery into the RDP graphics pipeline.
//!
//! This module owns in-process PipeWire handling for the server runtime. It
//! consumes a provided PipeWire file descriptor, keeps non-`Send` PipeWire state
//! on a dedicated thread, and forwards captured frames to graphics consumers.
//!
//! ## Design constraints
//!
//! - Unsafe interactions with SPA/PipeWire buffers are isolated here.
//! - Consent/session ownership is handled outside this module; it only consumes the FD.
//! - Graphics damage analysis remains in `rdp::channels::graphics::damage`; this
//!   module carries capture metadata.
//!
//! ## Architecture
//!
//! PipeWire bindings rely on non-`Send` internals, so runtime orchestration uses:
//! - async-facing manager APIs for caller coordination
//! - a dedicated PipeWire thread owning MainLoop/Context/Core/Stream state
//! - channel-based command and frame delivery between domains

// =============================================================================
// CORE MODULES
// =============================================================================

pub mod config;
pub mod error;
pub mod ffi;
pub mod format;
pub mod frame;
pub mod monitor;
pub mod pw_thread;
pub mod stream;

// =============================================================================
// FEATURE MODULES
// =============================================================================

/// Audio capture via PipeWire.
pub mod audio;

// =============================================================================
// RE-EXPORTS - PRIMARY API
// =============================================================================

// Configuration
pub use config::{
    AdaptiveBitrateConfig, AdaptiveBitrateConfigBuilder, PipeWireConfig, PipeWireConfigBuilder,
    QualityPreset,
};

// Errors
pub use error::{PipeWireError, Result};

// Stream types
pub use monitor::{MonitorInfo, SourceType, StreamInfo};
pub use stream::{
    NegotiatedFormat, PwStreamState, StreamConfig, StreamMetrics, StreamStateEvent, StreamTime,
};

// Frame types
pub use format::{PixelFormat, convert_format};
pub use frame::{FrameCallback, FrameFlags, FrameStats, VideoFrame};

// =============================================================================
// RE-EXPORTS - ADVANCED API
// =============================================================================

// Thread management
pub use pw_thread::{PipeWireThreadCommand, PipeWireThreadManager};

// FFI utilities
pub use ffi::{
    DamageRegion as FfiDamageRegion, SpaDataType, calculate_buffer_size, calculate_stride,
    drm_fourcc, get_bytes_per_pixel, spa_video_format_to_drm_fourcc,
};

// =============================================================================
// FEATURE RE-EXPORTS
// =============================================================================

pub use audio::{
    AudioCapture, AudioCaptureHandle, AudioFormat, AudioSamples, CaptureConfig, spawn_audio_capture,
};

// =============================================================================
// CRATE-LEVEL ITEMS
// =============================================================================

use libspa::param::video::VideoFormat;

/// Crate version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Initialize PipeWire library
///
/// This should be called once at application startup.
/// It's safe to call multiple times.
///
/// # Examples
///
/// ```rust,ignore
/// fn main() {
///     crate::desktop::pipewire::init();
///     // ... use PipeWire ...
///     crate::desktop::pipewire::deinit();
/// }
/// ```
pub fn init() {
    pipewire::init();
}

/// Deinitialize PipeWire library
///
/// Call at most once, at final process shutdown, after every PipeWire thread
/// has ended and every resource has been dropped. Session workers must not
/// call this: `pipewire::init()` is once-only and cannot initialize again.
///
/// # Safety
///
/// This function is safe to call if:
/// - [`init()`] was called previously
/// - All PipeWire resources (managers, connections, streams) have been dropped
/// - No other PipeWire operations are in progress
pub fn deinit() {
    // SAFETY: Caller ensures init() was called and all resources are dropped.
    // This is a final process-lifetime operation, not per-session cleanup.
    unsafe {
        pipewire::deinit();
    }
}

/// Get supported video formats in order of preference
///
/// Returns formats ordered by preference for screen capture:
/// 1. BGRx/BGRA - Common for desktop compositors
/// 2. RGBx/RGBA - Alternative RGB formats
/// 3. RGB/BGR - 24-bit formats (less common)
/// 4. NV12/YUY2/I420 - YUV formats (compressed, require conversion)
#[must_use]
pub fn supported_formats() -> Vec<VideoFormat> {
    vec![
        VideoFormat::BGRx, // Preferred: no alpha channel overhead
        VideoFormat::BGRA, // Common format with alpha
        VideoFormat::RGBx, // Alternative without alpha
        VideoFormat::RGBA, // Alternative with alpha
        VideoFormat::RGB,  // 24-bit fallback
        VideoFormat::BGR,  // 24-bit fallback
        VideoFormat::NV12, // YUV 4:2:0 (compressed)
        VideoFormat::YUY2, // YUV 4:2:2 (compressed)
        VideoFormat::I420, // YUV 4:2:0 planar
    ]
}

/// Check if DMA-BUF is likely supported
///
/// This is a heuristic check based on DRM device availability.
/// The actual DMA-BUF support is determined during format negotiation.
///
/// # Returns
///
/// `true` if DRM devices are found, suggesting DMA-BUF may be available.
#[must_use]
pub fn is_dmabuf_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::path::Path;

        let drm_paths = ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/renderD128"];
        drm_paths.iter().any(|path| Path::new(path).exists())
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Get recommended buffer count for a given refresh rate
///
/// Higher refresh rates benefit from more buffers to prevent frame drops.
///
/// # Arguments
///
/// * `refresh_rate` - Monitor refresh rate in Hz
///
/// # Returns
///
/// Recommended number of buffers (2-5)
#[must_use]
pub fn recommended_buffer_count(refresh_rate: u32) -> u32 {
    match refresh_rate {
        0..=30 => 2,   // Low refresh: 2 buffers sufficient
        31..=60 => 3,  // Standard: 3 buffers
        61..=120 => 4, // High refresh: 4 buffers
        _ => 5,        // Very high refresh: 5 buffers
    }
}

/// Calculate optimal frame buffer size for a channel
///
/// Returns the recommended channel buffer size to hold approximately
/// 1 second of frames, capped at 144 frames.
///
/// # Arguments
///
/// * `refresh_rate` - Monitor refresh rate in Hz
///
/// # Returns
///
/// Recommended channel buffer size (30-144)
#[must_use]
pub fn recommended_frame_buffer_size(refresh_rate: u32) -> usize {
    (refresh_rate as usize).clamp(30, 144)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_formats() {
        let formats = supported_formats();
        assert!(!formats.is_empty());
        assert_eq!(formats[0], VideoFormat::BGRx);
    }

    #[test]
    fn test_recommended_buffer_count() {
        assert_eq!(recommended_buffer_count(30), 2);
        assert_eq!(recommended_buffer_count(60), 3);
        assert_eq!(recommended_buffer_count(144), 5);
    }

    #[test]
    fn test_recommended_frame_buffer_size() {
        assert_eq!(recommended_frame_buffer_size(30), 30);
        assert_eq!(recommended_frame_buffer_size(60), 60);
        assert_eq!(recommended_frame_buffer_size(144), 144);
        assert_eq!(recommended_frame_buffer_size(200), 144); // Capped at 144
        assert_eq!(recommended_frame_buffer_size(10), 30); // Minimum 30
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_dmabuf_check() {
        // Just verify it doesn't crash
        let _ = is_dmabuf_supported();
    }

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
    }
}
