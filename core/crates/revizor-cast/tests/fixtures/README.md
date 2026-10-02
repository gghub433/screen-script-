# Test fixtures

Real encoded media, so the MPEG-TS / HLS tests are judged by an **independent decoder (ffmpeg)** and not only by our own parser.

| File | Content |
|---|---|
| `video.h264` | Annex-B H.264 (libx264), 160×90, Constrained Baseline, 120 access units, an IDR every 30 frames (frames 0, 30, 60, 90), 30 fps |
| `video.idx` | one line per access unit: `<pts_us> <bytes> <is_keyframe 0/1>` |
| `audio.adts` | AAC-LC, 48 kHz stereo, ADTS frames (≈ 65 kbit/s) |

Equivalent files can be regenerated with ffmpeg (`-c:v libx264 -profile:v baseline -g 30 -bf 0`, `-c:a aac`); they do not need to be
byte-identical for the tests to mean the same thing.

Limitation: this is a tiny Baseline picture. Real encoders on phones and PCs emit High-profile 1080p. The muxer treats the
bitstream as opaque apart from NAL unit types (AUD, SPS/PPS, IDR), so this is a fair check of the container, but a real
encoder's output has not been run through these tests.
