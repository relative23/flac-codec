# Patches on top of flac-codec 1.3.3

`nastra-1.3.3-hardened` is upstream release 1.3.3 (`a7b1ffb`) plus the commits below,
without merges: the fixes and the fork-only commits, in the order they were made. NASTRA
pins an exact commit of this branch, never the branch name.

## Fixes

Each fix is one commit with a regression test that fails without it. The same change
also sits alone on a `fix-*` branch based on upstream `master`, ready for an upstream
pull request.

| Commit | Change | `fix-*` branch | Upstream status (2026-11-07) |
|---|---|---|---|
| `32b8b64` | Reject residual partition orders larger than the block (decoder panic, release builds too) | `fix-residual-partition-order-panic` | also fixed in the open tuffy/flac-codec#4 (based on 1.3.2); a separate PR is offered there |
| `ed1b2a9` | Return `TooManySamples` when a block runs past the STREAMINFO total | `fix-samples-beyond-streaminfo-total` | also fixed in #4 |
| `021a487` | Let prediction wrap around instead of overflowing (debug builds) | `fix-prediction-overflow` | also fixed in #4 |
| `d78a347` | Support partition orders above 6 in the encoder (encoding 2–3 % faster as a side effect) | `fix-encoder-partition-order-limit` | not in #4: issue tuffy/flac-codec#7, PR tuffy/flac-codec#8 (open) |
| `96ce42e` | Use wrapping arithmetic in stereo decorrelation (debug builds; decoding to samples about 4 % faster, no change with MD5 verification) | `fix-stereo-decorrelation-overflow` | also fixed in #4 |
| `65abdc8` | Never write fewer residual partitions than the partition order says (the encoder wrote streams no decoder can read, e.g. with the default options for a last block of 4 samples) | `fix-encoder-short-block-partitions` | also fixed in #4 (its commit `2503845`); pointed out in a comment there |
| `ebfd19b` | Allow LPC order 32 in debug builds (a debug assertion rejected the highest order the options accept) | `fix-lpc-order-32-debug-assertion` | not in #4: issue tuffy/flac-codec#9, PR tuffy/flac-codec#10 (open) |
| `4474e6b` | Write a residual of i32::MIN without overflowing (debug builds; found by the `roundtrip` fuzz target) | `fix-residual-min-negation` | also fixed in #4 |
| `f57ffea` | Write STREAMINFO's bit depth without unwrapping (a damaged STREAMINFO with 1 bit per sample panicked when written back, release builds too; found by the `metadata` fuzz target) | `fix-streaminfo-low-bit-depth-write` | also fixed in #4, with the same line |
| `711ca41` | Finalize sample and channel writers when they are dropped (`FlacSampleWriter` and `FlacChannelWriter` lost their last, partly filled block without an error; found by the thread tests) | `fix-writers-finalize-on-drop` | not in #4: issue tuffy/flac-codec#11, PR tuffy/flac-codec#12 (open) |

## Speed

Every change keeps the encoder's output byte-identical and the decoder's samples and
errors the same. The first one sits alone on a `perf-*` branch based on upstream
`master`; the later ones build on the fixes and on each other (owner's choice
2026-11-08: build them in the fork), each `perf-*` branch ending at its last commit.

| Commit | Change | `perf-*` branch | Upstream status |
|---|---|---|---|
| `772b45a` | Count both subframe candidates (FIXED and LPC) and write only the smaller | `perf-encoder-count-candidates` | not offered yet |
| `2ddd1c9` | Decode frames from memory with a 64-bit bit reader (`src/frames.rs`): read ahead in 256 KiB pieces, CRC-16 over the frame's bytes afterwards, at most two refills per frame, then the stream path (also for frames over 4 MiB) | `perf-decoder-frames-from-memory` | not offered |
| `ed19bc0` | Unroll prediction for orders up to 12 | same | not offered |
| `e1162d8` | Interleave frame samples without collecting each PCM frame | same | not offered |
| `6b41340` | Read Rice codes in one step (`ReadRice`) | same | not offered |
| `4fb995d` | Compute CRC-16 eight bytes at a time | same | not offered |
| `5caa7f5` | Sum residual magnitudes once at the finest partition order | `perf-encoder-partition-sums` | not offered |
| `7a4c1e0` | Unroll LPC residuals for orders up to 12 | `perf-encoder-unrolled-residuals` | not offered |
| `532cbf3` | Compute FIXED differences without stopping at each one | `perf-encoder-fixed-differences` | not offered |
| `1d045ed` | Plan subframes and compute their sizes before writing (the partition code moves out of `write_residuals`; residual sizes are computed, not written into a counter) | `perf-encoder-plan-subframes` | not offered |
| `b70f882` | Write only the two stereo subframes that are used | `perf-encoder-stereo-winners` | not offered |
| `e46ccb1` | Write frames into memory with a 64-bit bit writer (`FrameWriter` in `src/frames.rs`, Rice codes in one step, CRC-16 over the finished frame) | `perf-encoder-frame-writer` | not offered |
| `dcc0553` | Take whole frames from the input and hash and deinterleave them in bulk | `perf-encoder-input-path` | not offered |
| `56026bf` | Encode frames on several threads: `Options::threads`, new public API of this branch; the output is the same for any number of threads | `perf-encoder-parallel-frames` | not offered |
| `388d2bb` | Compute FIXED differences in a loop that vectorizes | `perf-encoder-fixed-vectorized` | not offered |
| `6552a4b` | Compute all autocorrelation lags in one pass (each lag's sum in the same order, so bit for bit the same) | `perf-encoder-autocorrelation` | not offered |
| `442deb7` | Keep the FIXED buffers' length between blocks | `perf-encoder-fixed-no-refill` | not offered |
| `c26405c` | Add the newest sample's product last in the prediction (the decoder's loop waited on a chain of additions per sample) | `perf-decoder-prediction-order` | not offered |
| `7248eba` | Hash the samples on a thread of its own next to the encoding threads (with `Options::threads` above 1) | `perf-encoder-threads-md5` | not offered |
| `b32cde9` | Compute residual sizes without a branch per residual | `perf-encoder-size-loop` | not offered |
| `c51a15b` | Fold residuals without a branch when writing Rice codes | `perf-encoder-rice-fold` | not offered |

Each comes with its own equivalence test: `SliceBitReader` and `FrameWriter` against
bitstream-io's reader and writer on random operations (Rice codes included), prediction
and LPC residuals per order against the general loops, FIXED parameters against the
previous algorithm, interleaving and deinterleaving against `MultiZip`, CRC-16 by
slices against bytes, planned residual sizes against written ones (failures included),
MD5 by buffers against MD5 by samples, the autocorrelation in one pass against summing
lag by lag (compared as bits), and several threads against one for all three writers. In debug builds every written subframe is checked against its planned size.

## Fork only

`PATCHES.md`, `tests/hardening.rs`, `tests/memory.rs`, `deny.toml`, `fuzz/` and
`.github/workflows/checks.yml`. They are not part of any upstream pull request.

`tests/hardening.rs`:

- `round_trip_every_depth_layout_and_option`: 4 to 32 bits, 1 to 8 channels, tones,
  noise and full-scale square waves, default, fast and best settings and partition
  order 15.
- `damaged_streams_never_panic`: bit flips, truncation and garbage in the metadata and
  frames, with a fixed seed. `HARDENING_ROUNDS` sets the count (3,000 by default; CI
  runs 1,000,000 in a release build with overflow checks).
- `encoder_random_inputs_round_trip`: random bit depths, channel counts, rates up to
  1,048,575 Hz, lengths, block sizes, LPC and partition orders, windows (also NaN,
  negative and infinite Tukey parameters), seek tables and padding, ten kinds of
  signal, written in random pieces; every stream must decode to its input.
  `ENCODER_ROUNDS` sets the count (200 by default; CI runs 20,000 in a release build
  with overflow checks and debug assertions). `HARDENING_DUMP=<dir>` keeps the input
  and the stream of every failure.
- `reference_decoders_agree` (ignored by default; needs `flac` and `ffmpeg`): libFLAC's
  `flac --test` accepts every stream (frame CRCs and MD5), and libFLAC and ffmpeg decode
  the same samples that were encoded. 288 streams: 8 to 32 bits, 1, 2 and 6 channels,
  44.1 to 384 kHz.

`tests/memory.rs` counts heap use while decoding: damaged streams, and hostile metadata
(16 MiB blocks, a comment count of 4 billion, strings and pictures claiming 4 GiB).

`deny.toml` is the `cargo deny` policy: no RustSec advisory (unmaintained and yanked
crates included), MIT or Apache-2.0 only, one version per crate, crates.io only.

`fuzz/` holds the cargo-fuzz targets `decode` (samples and MD5), `seek`, `metadata`
(every block that reads must write back and read again unchanged), `roundtrip`
(any samples, layout and options must encode and decode to the same samples) and
`decode_reference` (any input, read in pieces of any size or after a seek, must give
the samples and the ending of commit `69e7537`, the branch before the decoder speed
work, fetched from this fork) and `encode_reference` (any samples, layout and options
on one to four threads must give the stream, or the error, of the same commit):
`cargo +nightly fuzz run <target>`.

`tests/hardening.rs` also checks that decoding from sources handing out 1, 3 or 4093
bytes per read matches decoding from memory, for undamaged and damaged streams, and
that a 5 MB frame decodes through the stream fallback, that 2, 3 and 8 threads write
the bytes of one thread (also when the writer is dropped instead of finalized), and
that writing more samples than declared fails the same way on several threads.

## Moving to a new upstream release

1. Branch `nastra-<version>-hardened` from the release commit.
2. For each fix, run its regression test on the release without the fix. If it passes,
   upstream has fixed it: drop the commit and note the upstream commit here.
   Otherwise cherry-pick it.
3. Cherry-pick the fork-only commits and update `UPSTREAM_BASE` in the workflow.
4. When all checks pass, NASTRA moves its pin to the new commit.

## Evidence (2026-11-07, x86_64 Linux, Rust 1.99.1)

- Damaged streams, from an independent harness with 26 seed files (`spikes/flac-audit`
  in NASTRA). Upstream 1.3.3 panicked 221 times in 5,000 debug runs and 47 times in
  20,000 release runs. This branch: 0 panics in 40,000 debug and 200,000 release runs.
- `tests/hardening.rs` on this branch: 0 panics in 1,000,000 release rounds with
  overflow checks. Against upstream 1.3.3 it fails: the encoder panics at partition
  order 15, and without that seed there are 3 decoder panics in 3,000 rounds.
- `reference_decoders_agree` with flac 1.5.0 and ffmpeg 8.0.1: 288 of 288 streams
  identical.
- Encoder output is byte-identical to upstream 1.3.3 for the default, fast and best
  settings on 15 test files and on the 64 IETF subset files.
- The upstream test suite passes with and without `rayon`, on stable and on Rust 1.88.
  clippy reports as many warnings as on upstream 1.3.3 (counted from its JSON output).
- `encoder_random_inputs_round_trip` found the last two fixes: within 200 rounds a
  stream no decoder could read (flac-codec, libFLAC 1.5.0 and ffmpeg 8.0.1 all reject
  it; upstream 1.3.3 writes the same stream), and with debug assertions the order-32
  panic. With both fixes: 20,000 rounds clean with overflow checks and debug
  assertions. Encoder output for the 64 IETF subset files is unchanged by them.
- Heap peaks (`tests/memory.rs`): below 0.1 MiB for the clean and the 2,000 damaged
  streams and for every claim of 4 billion entries or 4 GiB (they fail cleanly); 16 MiB
  for a 16 MiB application block, 21.4 MiB for a 16 MiB seek table, and 96 MiB for a
  16 MiB comment block of 4.2 million empty comments, the largest factor (a stream may
  hold one comment block, so about 100 MB is the most it can cost).
- `cargo deny check`: advisories, bans, licenses and sources ok.
- cargo-fuzz, 30 minutes per target with AddressSanitizer and debug assertions: the first
  run found the last two fixes within a minute (`metadata`, `roundtrip`). After them:
  `decode` 430,658 runs, `seek` 216,667,403, `metadata` 128,754,571, `roundtrip`
  4,404,684, nothing found.
- All 120 tests pass on this branch; the encoder output for the 64 IETF subset files is
  byte-identical to upstream 1.3.3 for the default, best and fast settings.
- After the decoder speed work (2026-11-08), 30 minutes each with AddressSanitizer:
  `decode_reference` 103,365 runs with the same samples and endings as `69e7537`
  (pieces of 1 to 128 bytes, 64 KiB, or after a seek), `decode` 536,271, `seek`
  210,296,650, nothing found. 1,000,000 damaged streams and 20,000 random encoder rounds
  with overflow checks and debug assertions: clean. libFLAC 1.5.0 and ffmpeg 8.0.1:
  288 of 288 streams identical. Heap of a small clean stream: about 300 KiB (the
  read-ahead), damaged streams at most 527 KiB.
- After the encoder speed work (2026-11-10): 128 tests pass, also on Rust 1.88 with
  `rayon`; 1,000,000 damaged streams and 20,000 random encoder rounds with overflow
  checks and debug assertions (which check every written subframe against its planned
  size): clean; libFLAC 1.5.0 and ffmpeg 8.0.1: 288 of 288 streams identical; the
  encoder output for the 64 IETF subset files is byte-identical to upstream 1.3.3 at
  every commit, and for 1, 2, 4, 8 and 16 threads.
- cargo-fuzz on the encoder, 30 minutes per target with AddressSanitizer and debug
  assertions, one to four threads: `encode_reference` 2,850,910 runs before the last
  three encoder commits and 1,817,397 after them, each with the stream or error of
  `69e7537`; `roundtrip` 1,141,118 and 890,351 runs; nothing found.
- After the later speed commits (`388d2bb` to `c51a15b`, 2026-11-10): 128 tests, also
  on Rust 1.88 with `rayon`; 1,000,000 damaged streams and 20,000 random encoder rounds
  with overflow checks and debug assertions: clean; libFLAC 1.5.0 and ffmpeg 8.0.1:
  288 of 288; encoder output byte-identical to upstream 1.3.3 at every commit and for
  1 to 16 threads.
- cargo-fuzz after `c51a15b`, 30 minutes per target: `decode_reference` 84,950 runs,
  `encode_reference` 1,197,445, `roundtrip` 890,210, nothing found.
- The thread tests found fix 10: dropping a `FlacSampleWriter` or `FlacChannelWriter`
  lost the last partial block in upstream 1.3.3 too (10,000 samples written, 8,192
  decoded).

## Speed measurements (2026-11-10, one pinned core, 64 IETF subset files with 98.6 million samples)

Best of 5 runs, three rounds alternating between upstream 1.3.3 and this branch
(`c51a15b`); libFLAC 1.5.0's `flac` on the same core in a container, right after. The
spread between rounds was below 1 %.

| | upstream 1.3.3 | `c51a15b` | libFLAC 1.5.0 |
|---|---|---|---|
| encode, default (`flac -5`) | 5.731 s | 1.356 s | 1.158 s |
| encode, best (`flac -8`) | 6.206 s | 1.571 s | 2.911 s |
| encode, fast (`flac -0`) | 2.464 s | 0.757 s | 0.691 s |
| decode to samples | 1.594 s | 0.751 s | |
| decode with MD5 check (`verify`, `flac -t`) | 1.955 s | 1.031 s | 0.846 s |

Against upstream 1.3.3, encoding is 3.3 to 4.2 times as fast and decoding 2.1 times
(1.9 times with the MD5 check). The output of the encoder is byte-identical. Against
libFLAC on one core: 1.85 times as fast at the best setting, 1.17 times slower at the
default, 1.10 times slower at the fast setting, 1.22 times slower at checking.

With `Options::threads` (plus the hashing thread), the default encode of the 64 files
takes 0.74 s on 2 threads, 0.50 s on 4 and 0.45 s on 8. A long stream, default
settings, one core against 8 threads against libFLAC `-5` on one core:

| | one core | 8 threads | libFLAC `-5` |
|---|---|---|---|
| 24 bit, 96 kHz, stereo, 175 s | 0.59 s (298 times real time) | 0.18 s | 0.49 s |
| 16 bit, 44.1 kHz, stereo, 140 s | 0.18 s (760 times real time) | 0.05 s | 0.15 s |

Beyond 8 threads the MD5 of the samples, which cannot be split, sets the pace.

The presets do not match libFLAC's levels exactly, and libFLAC's times include reading
WAV files and writing FLAC files on tmpfs, while flac-codec works in memory.

Where the time goes now (perf, by time, default settings on one core, 24 bit at
96 kHz): LPC residuals 21 %, MD5 23 %, window, autocorrelation and planning 11 %, the
FIXED search 12 %, writing Rice codes 8 %, exact residual sizes 7 % (before `b32cde9`),
partition sums 6 %. The multiplications and the MD5 are at what scalar code does;
libFLAC uses AVX2 for the former, chosen at run time.

Building for newer processors (`-C target-cpu=x86-64-v3`, AVX2) is a choice of the
application, not of this crate: measured on an AMD Ryzen 9 9950X3D, the default encode
of the 64 files takes 1.11 s instead of 1.41 s (same bytes), below libFLAC's 1.16 s,
but decoding takes 0.96 s instead of 0.75 s, because the vectorized prediction reads
back samples it has only just written. Until that is solved the baseline build is the
better one for an application that also decodes.

The decode figures above are dominated by three test files that use only 32nd-order
predictors (one of them 8 channels at 192 kHz): ordinary streams decode at about 5 ns
per sample, 0.38 s for the other 61 files.

Tried and dropped because they were slower: summing FIXED magnitudes inside the
difference loop, a sign-bit overflow check inside `Vec::extend`, and keeping the whole
prediction history in a local array.
