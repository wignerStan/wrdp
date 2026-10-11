# Match H.264 driver parameters and EGFX wire bounds

The VA-API backend emits AVC420. Use it only after AVC420 negotiation. Retain
the dual software encoder after AVC444 negotiation; a single-stream fallback
would violate the selected channel format.

Mesa's H.264 encoder uses application-supplied packed headers to configure NAL
type and entropy coding. The previous backend submitted only parameter buffers,
then prefixed SPS/PPS to coded output. On the Radeon 610M with Mesa 25.0.7 this
produced reserved NAL type 0. A real decoder rejected every tested picture.

Query and enable packed sequence, picture, and slice support before context
creation. Submit matching Annex-B SPS, PPS, and slice headers for each picture.
Reset frame_num and picture-order count at each IDR. Reject coded output without
a matching IDR or P slice. Drivers that lack the required packed-header support
use software encoding.

cros-libva 0.0.13 does not expose packed-header buffers. Pin its focused fork to
commit c6ef66f62920e6893bbe9f5fc0cab83686424a86, based on released upstream commit
ad2ec9f0a3b66d38310b93c5a25d4c415c022a04. The extension owns header bytes until
vaCreateBuffer copies them. It adds no access to private display/context handles.

EGFX region masks use exclusive right/bottom bounds. Preserve these bounds when
converting damage rectangles, including single pixels at the display edge.

A decoder fixture must verify IDR, predicted, and forced-IDR pictures. Nonempty
coded bytes and SPS/PPS alone do not establish that hardware encoding works.
Native client checks remain necessary after wire-format changes.

References:
- [libva packed-header contract](https://github.com/intel/libva/blob/master/va/va.h).
- [Mesa H.264 packed-header parsing](https://gitlab.freedesktop.org/mesa/mesa/-/blob/mesa-25.0.7/src/gallium/frontends/va/picture_h264_enc.c).
- [MS-RDPEGFX RDPGFX_RECT16](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/).
