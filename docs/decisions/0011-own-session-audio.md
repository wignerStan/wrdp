# ADR 0011: Own audio in the managed desktop session

**Status:** accepted
**Date:** 2026-10-11

## Decision

The managed desktop supervisor starts PipeWire, its PulseAudio compatibility
server, and WirePlumber under the desktop session D-Bus process group. Their
Unix sockets use the authenticated user runtime directory. The session owns a
virtual stereo sink; it needs no host sound devices or host audio socket.

The PipeWire daemon uses the packaged base configuration plus the WRDP null-sink
drop-in. Only that daemon receives the private config directory. Audio clients
keep the normal client configuration lookup. Provisioning installs the required
runtime packages and these desktop resources. If audio dependencies are absent,
the desktop remains usable and reports that session audio is unavailable.

RDPSND captures the default sink monitor through the bound session socket. Its
format pod fixes 48 kHz, stereo, signed 16-bit PCM to match the advertised RDP
format. PipeWire controls the graph quantum; the capture buffer setting requests
latency rather than guaranteeing packet size.

## Consequences

- Audio daemons stop with the compositor generation. No global user service or
  second session owner is introduced.
- A missing socket can become ready during compositor autostart. The capture
  thread waits at most five seconds and checks cancellation every 25 ms.
- The readiness check accepts only a Unix socket, not a file or symlink. It does
  not authenticate the server; session ownership and runtime-directory checks
  remain the session binder's responsibility.
- The connection owns capture and forwarding. Dropping either handle or RDPSND
  backend stops the worker, including when the source is silent.
- SPA chunks are decoded from their valid offset and size. Only complete
  interleaved frames reach RDPSND.
- Client microphone redirection is a separate channel and is not enabled here.

## Validation boundary

Unit tests verify the serialized SPA format, PCM byte decoding, cancellation,
and bounded socket readiness. Provisioning tests verify package and resource
ownership. Actual playback needs a client-side signal capture across a real
RDPSND connection; socket presence or a negotiated format alone is not evidence
of audible output.
