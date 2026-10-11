# Preserve color-conversion pixel order

Status: accepted

AVX2 BGRA-to-YUV444 conversion must produce the same ordered planes as the scalar
converter. AVX2 packing operates inside each 128-bit lane. Selecting only the low
lane duplicated pixels 0–3 into positions 4–7 and discarded their original colors.

Join the first four packed bytes from each lane before storing eight pixels.
Exact scalar comparisons cover distinct colors, three matrices, multiple rows,
and partial SIMD batches. A separate block-average oracle checks chroma
subsampling across SIMD boundaries.

This repairs pixel order before H.264 encoding. Native codec compatibility and
lossy chroma detail require separate decoder and display checks.
