# Encode AVC444 as one picture sequence

Status: accepted

MS-RDPEGFX 2.2.4.5 requires the main and auxiliary views to share one H.264
encoder and decoder sequence. The sequence can contain two coded pictures for
each desktop frame. Encoder rate control and Annex A level selection now use
twice the desktop frame rate. For 1712×1056 at 30 desktop frames/s, 423,720
macroblocks/s requires level 4.2 rather than level 4.0.

OpenH264 2.6 forces scene-change detection on in ScreenContentRealTime, even
when the caller disables it. Alternating packed views therefore trigger
additional IDRs. CameraVideoRealTime respects the disabled scene detector and
keeps one continuous reference sequence. A main IDR resets the shared decoded
picture buffer. The following auxiliary P picture can reference that fresh
main picture; a second forced IDR is unnecessary.

Explicit and periodic IDR recovery includes an auxiliary view. A skipped main
picture returns before auxiliary encoding, so no unsent reference advances the
shared sequence. The owned auxiliary bytes move into the output frame.

Full-frame main updates always include the auxiliary view. MS-RDPEGFX
3.3.8.3.2 converts received main rectangles with only main YUV420 data.
Unchanged auxiliary input cannot be omitted safely when the main mask covers
the whole desktop. Legacy omission settings remain accepted but have no effect.
Per-picture encoder skips are disabled because dropping only auxiliary has the
same color loss. An unexpected incomplete pair is suppressed and requests a
fresh shared-sequence IDR before retry.

Actual SPS tests cover the dual-picture levels, main IDR with auxiliary P,
unchanged color under legacy omission/skip settings, and full recovery. Strict FFmpeg and FreeRDP
sequence checks cover full and clipped regions. Native Windows App
interoperability is verified separately from these decoder fixtures.

References:
- [MS-RDPEGFX stream combination](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/8131c1bc-1af8-4907-a05a-f72f4581160f)
- [MS-RDPEGFX AVC444 stream](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/844018a5-d717-4bc9-bddb-8b4d6be5dd3f)
- [OpenH264 2.6 parameter validation](https://github.com/cisco/openh264/blob/v2.6.0/codec/encoder/core/src/encoder_ext.cpp)
