# Enforce the hardware fallback policy

Status: accepted

The display pipeline used OpenH264 after a VA-API initialization or encoding
failure even when hardware_encoding.fallback_to_software was false. An empty
hardware payload took the same path. This hid device and driver failures from
operators who required GPU encoding.

All three failures now use one recovery path. Software recovery requires both
the fallback flag and negotiated AVC420. The software constructor must also
succeed. Otherwise the server pauses that client's display pipeline and sends
GraphicsSubsystemFailed through IronRDP's connection disconnect event
(MS-RDPBCGR, error code 0x112F). The daemon and compositor remain running.

An encoder's supported skip/delay result is not a hardware failure. A failed
network submission retains its encoder and requests an IDR. Negotiated AVC444
does not enter this single-stream hardware path. A new connection retries
hardware after a prior connection used software recovery.

Tests capture disconnect events and count software factory calls for each
failure, both flag values, negotiated codec mismatches, and a failed software
constructor.
