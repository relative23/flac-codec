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
| `65abdc8` | Never write fewer residual partitions than the partition order says (the encoder wrote streams no decoder can read, e.g. with the default options for a last block of 4 samples) | `fix-encoder-short-block-partitions` | also fixed in #4 (its commit `2503845`); not reported separately |
| `ebfd19b` | Allow LPC order 32 in debug builds (a debug assertion rejected the highest order the options accept) | `fix-lpc-order-32-debug-assertion` | not in #4; not reported yet |
| `4474e6b` | Write a residual of i32::MIN without overflowing (debug builds; found by the `roundtrip` fuzz target) | `fix-residual-min-negation` | also fixed in #4 |
| `f57ffea` | Write STREAMINFO's bit depth without unwrapping (a damaged STREAMINFO with 1 bit per sample panicked when written back, release builds too; found by the `metadata` fuzz target) | `fix-streaminfo-low-bit-depth-write` | also fixed in #4, with the same line |

## Speed

Each change keeps the encoder's output byte-identical and sits alone on a `perf-*`
branch based on upstream `master`.

| Commit | Change | `perf-*` branch | Upstream status |
|---|---|---|---|
| `772b45a` | Count both subframe candidates (FIXED and LPC) and write only the smaller | `perf-encoder-count-candidates` | not offered yet |

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
(every block that reads must write back and read again unchanged) and `roundtrip`
(any samples, layout and options must encode and decode to the same samples):
`cargo +nightly fuzz run <target>`.

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
- All 111 tests pass on this branch; the encoder output for the 64 IETF subset files is
  byte-identical to upstream 1.3.3 for the default, best and fast settings.

## Speed measurements (2026-11-07, one pinned core, 64 IETF subset files with 98.6 million samples)

Best of 5 runs, three rounds alternating between upstream 1.3.3 and this branch (head
`772b45a`, all fixes and the speed change above); libFLAC 1.5.0's `flac` on the same core
in a container:

| | upstream 1.3.3 | this branch | change | libFLAC 1.5.0 |
|---|---|---|---|---|
| encode, default (`flac -5`) | 5.639 s | 4.637 s | −17.8 % | 1.139 s |
| encode, best (`flac -8`) | 6.111 s | 5.155 s | −15.6 % | 2.859 s |
| encode, fast (`flac -0`) | 2.412 s | 2.372 s | −1.7 % | 0.682 s |
| decode to samples | 1.566 s | 1.534 s | −2.0 % | |
| decode with MD5 check (`verify`, `flac -t`) | 1.925 s | 1.925 s | none | 0.837 s |

With `rayon` on all 32 threads, the default encode takes 2.84 s. The presets do not
match libFLAC's levels exactly, and libFLAC's times include reading WAV files and writing
FLAC files on tmpfs, while flac-codec works in memory. libFLAC is still 1.8 to 4.1 times
faster on one core.

Where the time goes (callgrind, after the speed change): in the encoder, the LPC search
and residuals about 13 %, partition estimates 7 %, bit recording 9 %, FIXED residuals 5 %,
copies 4 %, MD5 2 %; in the decoder, 39 % of all instructions fetch single bytes through
`CrcReader` for bitstream-io (decoding from memory instead of a `BufReader` saves only
9 %), residual decoding and prediction most of the rest.
