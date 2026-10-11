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

Actual SPS tests cover the dual-picture levels, main IDR with auxiliary P,
auxiliary return after omission, and full recovery. Strict FFmpeg and FreeRDP
sequence checks cover full and clipped regions. Native Windows App
interoperability is verified separately from these decoder fixtures.

References:
- [MS-RDPEGFX AVC444 stream](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/844018a5-d717-4bc9-bddb-8b4d6be5dd3f)
- [OpenH264 2.6 parameter validation](https://github.com/cisco/openh264/blob/v2.6.0/codec/encoder/core/src/encoder_ext.cpp)
