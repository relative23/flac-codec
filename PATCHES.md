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

Each comes with its own equivalence test: `SliceBitReader` against bitstream-io's
reader on random operations (Rice codes included), prediction and LPC residuals per
order against the general loops, FIXED parameters against the previous algorithm,
interleaving against `MultiZip`, CRC-16 by slices against bytes.

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
work, fetched from this fork): `cargo +nightly fuzz run <target>`.

`tests/hardening.rs` also checks that decoding from sources handing out 1, 3 or 4093
bytes per read matches decoding from memory, for undamaged and damaged streams, and
that a 5 MB frame decodes through the stream fallback.

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

## Speed measurements (2026-11-08, one pinned core, 64 IETF subset files with 98.6 million samples)

Best of 5 runs, three rounds alternating between this branch before the decoder speed
work (`69e7537`, all fixes and `772b45a`) and after it (`532cbf3`); libFLAC 1.5.0's
`flac` on the same core in a container. Upstream 1.3.3 measured on 2026-11-07 under the
same conditions:

| | upstream 1.3.3 | `69e7537` | `532cbf3` | libFLAC 1.5.0 |
|---|---|---|---|---|
| decode to samples | 1.566 s | 1.493 s | 0.784 s | |
| decode with MD5 check (`verify`, `flac -t`) | 1.925 s | 1.936 s | 1.049 s | 0.837 s |
| encode, default (`flac -5`) | 5.639 s | 4.664 s | 4.174 s | 1.153 s |
| encode, best (`flac -8`) | 6.111 s | 5.204 s | 4.618 s | 2.867 s |
| encode, fast (`flac -0`) | 2.412 s | 2.387 s | 2.307 s | 0.680 s |

Against upstream 1.3.3, decoding is 2.0 times as fast and decoding with the MD5 check
1.8 times; encoding takes 26 % less time at the default and best settings. The output
of the encoder is byte-identical in every column. libFLAC is still 1.25 times as fast
at checking and 1.6 to 3.6 times at encoding; with `rayon` on all 32 threads the
default encode took 2.84 s on 2026-11-07.

The presets do not match libFLAC's levels exactly, and libFLAC's times include
reading WAV files and writing FLAC files on tmpfs, while flac-codec works in memory.

Where the encoder's time goes now (callgrind on a CD file): writing the chosen
subframes into bit recorders about 20 % (four per stereo frame, of which two are used),
counting candidates through `BitsWritten` 9 %, FIXED residuals and their order choice
10 %, copies 4 %, MD5 3 %. Its instructions per sample fell by 22 % with the speed work,
the time by 11 %: what is left is mostly writing memory.

Next, in this order: compute candidate sizes arithmetically and write only the two
stereo subframes that are used (the partition code moves out of `write_residuals` for
that); a word-based bit writer; encoding whole frames in parallel, in order, for the
wall-clock time of long takes.
