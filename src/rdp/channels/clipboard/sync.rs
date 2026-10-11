//! Clipboard ownership and loop-suppression policy.

pub use crate::rdp::channels::clipboard::core::loop_detector::ClipboardSource;
pub use crate::rdp::channels::clipboard::core::{
    ClipboardFormat, LoopDetectionConfig, LoopDetector,
};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncDirection {
    RdpToPortal,
    PortalToRdp,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalSyncDecision {
    Allow,
    Block,
}

#[derive(Debug, Clone)]
pub enum ClipboardState {
    Idle,
    RdpOwned(Vec<ClipboardFormat>, Instant),
    PortalOwned(Vec<String>),
    Syncing(SyncDirection),
}
impl PartialEq for ClipboardState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Idle, Self::Idle) => true,
            (Self::RdpOwned(a, _), Self::RdpOwned(b, _)) => {
                a.iter().map(|f| f.id).eq(b.iter().map(|f| f.id))
            }
            (Self::PortalOwned(a), Self::PortalOwned(b)) => a == b,
            (Self::Syncing(a), Self::Syncing(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for ClipboardState {}

#[derive(Debug)]
pub struct SyncManager {
    state: ClipboardState,
    detector: LoopDetector,
}
impl Default for SyncManager {
    fn default() -> Self {
        Self::new()
    }
}
impl SyncManager {
    pub fn new() -> Self {
        Self {
            state: ClipboardState::Idle,
            detector: LoopDetector::new(),
        }
    }
    pub fn with_config(config: LoopDetectionConfig) -> Self {
        Self {
            state: ClipboardState::Idle,
            detector: LoopDetector::with_config(config),
        }
    }
    pub const fn state(&self) -> &ClipboardState {
        &self.state
    }
    pub fn handle_rdp_formats(&mut self, formats: Vec<ClipboardFormat>) -> bool {
        // A format list carries ownership, not a content identity. Consecutive
        // copies commonly have the same formats and must still fetch new bytes.
        self.detector.record_formats(&formats, ClipboardSource::Rdp);
        self.detector.record_sync(ClipboardSource::Rdp);
        self.state = ClipboardState::RdpOwned(formats, Instant::now());
        true
    }
    pub fn handle_portal_formats(
        &mut self,
        mime_types: Vec<String>,
        authoritative: bool,
    ) -> PortalSyncDecision {
        if matches!(self.state, ClipboardState::RdpOwned(..)) && !authoritative {
            return PortalSyncDecision::Block;
        }
        if !authoritative && self.detector.would_cause_loop_mime(&mime_types) {
            return PortalSyncDecision::Block;
        }
        self.detector
            .record_mime_types(&mime_types, ClipboardSource::Local);
        self.detector.record_sync(ClipboardSource::Local);
        self.state = ClipboardState::PortalOwned(mime_types);
        PortalSyncDecision::Allow
    }
    pub fn check_content(&mut self, content: &[u8], from_rdp: bool) -> bool {
        let source = if from_rdp {
            ClipboardSource::Rdp
        } else {
            ClipboardSource::Local
        };
        if self.detector.would_cause_content_loop(content, source) {
            false
        } else {
            self.detector.record_content(content, source);
            true
        }
    }
    pub fn set_syncing(&mut self, direction: SyncDirection) {
        self.state = ClipboardState::Syncing(direction);
    }
    pub fn reset(&mut self) {
        self.state = ClipboardState::Idle;
    }
    pub fn reset_loop_detector(&mut self) {
        self.detector.clear();
    }
    pub fn would_cause_loop_rdp(&self, formats: &[ClipboardFormat]) -> bool {
        self.detector.would_cause_loop(formats)
    }
    pub fn would_cause_loop_portal(&self, mimes: &[String]) -> bool {
        self.detector.would_cause_loop_mime(mimes)
    }
    pub fn set_rdp_formats(&mut self, formats: Vec<ClipboardFormat>) {
        self.detector.record_formats(&formats, ClipboardSource::Rdp);
        self.state = ClipboardState::RdpOwned(formats, Instant::now());
    }
    pub fn set_portal_formats(&mut self, mimes: Vec<String>) {
        self.detector
            .record_mime_types(&mimes, ClipboardSource::Local);
        self.state = ClipboardState::PortalOwned(mimes);
    }
    pub fn is_rate_limited(&self, from_rdp: bool) -> bool {
        self.detector.is_rate_limited(if from_rdp {
            ClipboardSource::Rdp
        } else {
            ClipboardSource::Local
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text() -> Vec<ClipboardFormat> {
        vec![ClipboardFormat::unicode_text()]
    }
    #[test]
    fn first_rdp_owner_is_allowed() {
        let mut s = SyncManager::new();
        assert!(s.handle_rdp_formats(text()));
        assert!(matches!(s.state(), ClipboardState::RdpOwned(..)));
    }
    #[test]
    fn non_authoritative_local_echo_is_blocked() {
        let mut s = SyncManager::new();
        s.handle_rdp_formats(text());
        assert_eq!(
            s.handle_portal_formats(vec!["text/plain".into()], false),
            PortalSyncDecision::Block
        );
    }
    #[test]
    fn fast_authoritative_local_copy_replaces_remote_owner() {
        let mut s = SyncManager::new();
        assert!(s.handle_rdp_formats(text()));
        assert_eq!(
            s.handle_portal_formats(vec!["text/plain".into()], true),
            PortalSyncDecision::Allow
        );
        assert_eq!(
            s.state(),
            &ClipboardState::PortalOwned(vec!["text/plain".into()])
        );
    }
    #[test]
    fn repeated_remote_formats_fetch_each_new_selection() {
        let mut s = SyncManager::new();
        assert!(s.handle_rdp_formats(text()));
        assert_eq!(
            s.handle_portal_formats(vec!["text/plain".into()], true),
            PortalSyncDecision::Allow
        );
        assert!(s.handle_rdp_formats(text()));
        assert!(s.handle_rdp_formats(text()));
        assert!(matches!(s.state(), ClipboardState::RdpOwned(..)));
    }
    #[test]
    fn content_echo_is_blocked() {
        let mut s = SyncManager::new();
        assert!(s.check_content(b"x", true));
        assert!(!s.check_content(b"x", false));
    }
    #[test]
    fn reset_only_releases_owner() {
        let mut s = SyncManager::new();
        s.handle_rdp_formats(text());
        s.reset();
        assert_eq!(s.state(), &ClipboardState::Idle);
    }
}
