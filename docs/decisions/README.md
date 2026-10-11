# Architecture decisions

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-separate-server-and-session-manager.md) | Separate the RDP frontend from per-user desktop lifecycle management | accepted |
| [0002](0002-use-ironrdp-for-the-protocol-core.md) | Use IronRDP for RDP protocol state machines | accepted |
| [0003](0003-own-managed-wayland-sessions.md) | Create and own managed Wayland sessions | accepted |
| [0004](0004-prefer-egfx-with-bitmap-fallback.md) | Prefer EGFX/H.264 and keep bitmap fallback | accepted |
| [0005](0005-bundle-a-managed-compositor.md) | Bundle the managed compositor as a separate GPL program | accepted |
| [0006](0006-pin-ironrdp-revisions.md) | Pin one tested IronRDP revision | accepted |
| [0007](0007-separate-authentication-from-session-binding.md) | Separate credential validation from session binding | accepted |
| [0008](0008-use-ini-configuration.md) | Use INI files for server and session-manager configuration | accepted |
| [0009](0009-persist-session-process-identity.md) | Persist PID reuse-safe process identity for sessions | accepted |
| [0010](0010-use-one-server-lifecycle.md) | Use one RDP listener and connection lifecycle | accepted |
| [0011](0011-own-session-audio.md) | Own audio in the managed desktop session | accepted |
| [0012](0012-match-h264-driver-and-wire-headers.md) | Match H.264 packed driver headers, negotiated codec, and exclusive region bounds | accepted |
| [0013](0013-enforce-hardware-fallback-policy.md) | Enforce the hardware fallback flag and negotiated codec on graphics recovery | accepted |
| [0014](0014-encode-avc444-as-one-picture-sequence.md) | Encode both AVC444 views as one continuous, correctly budgeted H.264 sequence | accepted |
| [0015](0015-preserve-color-conversion-pixel-order.md) | Preserve pixel order across AVX2 color-conversion lanes | accepted |
