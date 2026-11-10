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
| `c8b74cd` | Compute LPC residuals exactly in `f64`, eight predictions side by side (`float_residuals`; replaces the twelve fixed-order loops of `7a4c1e0`) | `perf-encoder-float-residuals` | not offered |
| `08c71fe` | Add residual magnitudes and Rice quotients in 32-bit runs of 256 values (`sum_of`), with a 64-bit pass for a run that could have overflowed | `perf-encoder-short-sums` | not offered |
| `9b96238` | Keep the autocorrelation sums in registers | `perf-encoder-autocorrelation-registers` | not offered |
| `1fb15d0` | An MD5 of our own (`src/md5.rs`) with fewer operations between its steps; the `md5` crate is now a dev-dependency, the reference in the tests | `perf-md5-short-steps` | not offered |
| `87da9cb` | Sum the older samples of predictors above order 12 exactly in `f64` (`predict_long`); the exactness argument, its check and the conversion live in `src/exact.rs` for encoder and decoder | `perf-decoder-long-predictors` | not offered |
| `7dd73b2` | Interleave decoded samples with plain loops (`Frame::interleave`), copy whole slices out of the sample reader, and hash from the byte reader's own buffer in `verify_reader` | `perf-decoder-interleave` | not offered |
| `99d472b` | Convert a buffer of samples at a time before hashing | `perf-encoder-md5-input` | not offered |
| `fdcd6f2` | Compute CRC-16 sixteen bytes at a time | `perf-crc16-sixteen-bytes` | not offered |
| `6b174e8` | Drop the MD5's `io::Write`, unused since `7dd73b2` | same | not offered |
| `510c45c` | Check the products of the fixed-size prediction loops in builds for x86 processors with AVX2 (`CHECK_PRODUCTS`), where the compiler otherwise turns them into vector operations every sample has to pass through; an overflow leaves the rest of the block to the general loop, with the same samples | `perf-decoder-checked-products` | not offered |
| `f8c60a4` | Read a partition's Rice codes in one run, with the bit reader's state in local values (`SliceBitReader::rice_run`) | `perf-decoder-rice-runs` | not offered |
| `9f1cad7` | Tie `CHECK_PRODUCTS` to x86-64 (it was true for 32-bit x86 with AVX2 too, where a checked 64-bit multiplication is a call) | same | not offered |

Each comes with its own equivalence test: `SliceBitReader` and `FrameWriter` against
bitstream-io's reader and writer on random operations (Rice codes included), prediction
per order and for long predictors against the general loop, LPC residuals in floating
point against the integer loop (overflowing residuals and the edges of the exact bound
included), FIXED parameters against the previous algorithm, `sum_of` against 64-bit
sums, interleaving and deinterleaving against `MultiZip` and against the sample
iterator, CRC-16 by slices against bytes, planned residual sizes against written ones
(failures included), MD5 by buffers against MD5 by samples, the autocorrelation in one
pass against summing lag by lag (compared as bits), runs of Rice codes against single
codes (from any bit position, with codes longer than the cache and bytes that run out),
checked against unchecked products and both against the general loop (also with
products that overflow in the middle of a block), and several threads against one for
all three writers. `src/md5.rs` is compared with the `md5` crate for every length
around the block and padding limits and for random input in random pieces, next to the
test suite of RFC 1321. In debug builds every written subframe is checked against its
planned size.

Two of these changes rest on an argument instead of on the shape of the old code, and
both arguments are written down where the code is. `src/exact.rs`: sums of 32-bit
samples times coefficients whose magnitudes add up to less than 2^20 are exact in
`f64`, and where a predictor is beyond that bound the integer loops run. `src/md5.rs`:
the round constants are read through `std::hint::black_box`, because the compiler
otherwise adds a known constant last, after the term each step has to wait for; this
only changes the order of additions, never the result. The crate still forbids unsafe
code.

## Fork only

`PATCHES.md`, `tests/hardening.rs`, `tests/memory.rs`, `deny.toml`, `fuzz/` and
`.github/workflows/checks.yml`. They are not part of any upstream pull request.

The workflow runs the tests, the reference decoders and the large hardening round a
second time in a build for x86-64-v3 (`fec3958`): such a build takes other paths (the
checked products above, wider vectors wherever the compiler vectorizes), and an
application may well ship it.

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
- After the speed commits of 2026-11-10 (`c8b74cd` to `fdcd6f2`; `6b174e8` after them
  only removes an unused implementation): 136 tests, also on Rust 1.88 with `rayon`;
  1,000,000 damaged streams and 20,000 random encoder rounds with overflow checks and
  debug assertions: clean; libFLAC 1.5.0 and ffmpeg 8.0.1: 288 of 288; encoder output
  byte-identical to upstream 1.3.3 at every commit and for 1 to 16 threads;
  `cargo deny check` ok with the `md5` crate as a dev-dependency.
- cargo-fuzz at `fdcd6f2`, 30 minutes per target: `decode_reference` 98,264 runs,
  `encode_reference` 1,060,314, `roundtrip` 841,679, `decode` 2,228,948, nothing found.
- After `510c45c` to `9f1cad7` (2026-11-10), in the baseline build and in one for
  x86-64-v3 alike: 138 tests (also on Rust 1.88 with `rayon` in the baseline build);
  1,000,000 damaged streams and 20,000 random encoder rounds with overflow checks and
  debug assertions: clean; libFLAC 1.5.0 and ffmpeg 8.0.1: 288 of 288; encoder output
  byte-identical to upstream 1.3.3 for the default, best and fast settings.
- cargo-fuzz at `f8c60a4`, 30 minutes per target (`9f1cad7` after it changes nothing
  in an x86-64 build): `decode_reference` 84,525 runs, `encode_reference` 736,236,
  `roundtrip` 662,025, `decode` 706,954; built for x86-64-v3 (the reference commit
  too): `decode_reference` 86,208 and `roundtrip` 664,892; nothing found.
- The thread tests found fix 10: dropping a `FlacSampleWriter` or `FlacChannelWriter`
  lost the last partial block in upstream 1.3.3 too (10,000 samples written, 8,192
  decoded).

## Speed measurements (2026-11-10, one pinned core, 64 IETF subset files with 98.6 million samples)

Best of 5 runs, three rounds alternating between upstream 1.3.3, this branch at
`a79986d` and at `f8c60a4`, the latter built for plain x86-64 (the baseline) and for
x86-64-v3 (AVX2, BMI2, FMA); libFLAC 1.5.0's `flac` on the same core in a container,
right after. The machine was idle. The spread between rounds was at most 2 %, and 4 %
for decoding to bytes.

| | upstream 1.3.3 | `a79986d` | `f8c60a4` | `f8c60a4`, x86-64-v3 | libFLAC 1.5.0 |
|---|---|---|---|---|---|
| encode, default (`flac -5`) | 5.748 s | 1.045 s | 1.042 s | 0.826 s | 1.220 s |
| encode, best (`flac -8`) | 6.237 s | 1.187 s | 1.178 s | 0.967 s | 2.931 s |
| encode, fast (`flac -0`) | 2.471 s | 0.620 s | 0.613 s | 0.551 s | 0.689 s |
| decode to samples | 1.601 s | 0.590 s | 0.497 s | 0.457 s | |
| decode to bytes (`flac -d`, without its MD5) | 1.675 s | 0.605 s | 0.507 s | 0.472 s | 0.629 s |
| decode with MD5 check (`verify`, `flac -t`) | 1.964 s | 0.807 s | 0.718 s | 0.678 s | 0.845 s |

Against upstream 1.3.3, encoding is 4.0 to 5.5 times as fast, decoding 3.2 times and
checking 2.7 times. The output of the encoder is byte-identical, in both builds.
Against libFLAC on one core, the baseline build takes 0.85 of its time at the default
setting, 0.40 at the best, 0.89 at the fast one, 0.81 for decoding and 0.85 for
checking; the x86-64-v3 build 0.68, 0.33, 0.80, 0.75 and 0.80.

How libFLAC was timed: `flac -t` for checking; for decoding `flac -d -c --skip=1` to
`/dev/null`, one process per file, with the time of 64 process starts (0.018 s) taken
off. Skipping the first sample makes it seek, which switches its MD5 off (0.91 s with
the MD5). Encoding reads WAV files and writes FLAC files on tmpfs, while flac-codec
works in memory, and the presets do not match libFLAC's levels exactly. Its default
setting took 1.158 s and 1.170 s in two earlier runs of the same day.

With `Options::threads` (plus the hashing thread), the default encode of the 64 files
takes 0.62 s on 2 threads, 0.43 s on 4 and 0.39 s on 8 in the baseline build, and
0.53 s, 0.40 s and 0.39 s in the x86-64-v3 build. A long stream at the default
setting; libFLAC `-5` on one core was measured earlier the same day, and the
x86-64-v3 column while other programs were using the machine, so it is on the slow
side:

| | one core | 8 threads | x86-64-v3, one core | libFLAC `-5` |
|---|---|---|---|---|
| 24 bit, 96 kHz, stereo, 175 s | 0.44 s (394 times real time) | 0.15 s | 0.36 s | 0.49 s |
| 16 bit, 44.1 kHz, stereo, 140 s | 0.14 s (986 times real time) | 0.04 s | 0.11 s | 0.15 s |

Beyond 8 threads the MD5 of the samples, which cannot be split, sets the pace.

### Which build

Both builds are measured, go through the same checks in CI and are faster than libFLAC
in every row above. Which one an application ships is its choice
(`-C target-cpu=...`):

- **Baseline** (plain x86-64) runs on every 64-bit x86 processor.
- **x86-64-v3** needs AVX2, BMI2 and FMA (Intel since 2013, AMD since 2015) and takes
  about 20 % less time for encoding at the default and best settings, 10 % less at the
  fast one, 8 % less for decoding and 6 % less for checking. x86-64-v4 and
  `-C target-cpu=native` on the test machine (an AMD Ryzen 9 9950X3D) measured within
  6 % of it.
- **x86-64-v2** is not worth choosing: decoding as in the baseline, but checking 5 %
  and encoding 3 to 6 % slower on this AMD processor, because for that level the
  compiler writes the MD5's rotations as `shld` instructions, which are slow on AMD.

Before `510c45c` the x86-64-v3 build decoded 14 % slower than the baseline, ordinary
streams up to twice as slowly: the compiler gathered the products of the newest
samples in `predict_order` into vector operations, each 64-bit multiplication put
together from three 32-bit ones, and every sample had to pass through them before the
next could begin. Checked products (`CHECK_PRODUCTS`, set for x86-64 with AVX2) stay
single instructions; with them that build decodes the 64 files in 0.57 s instead of
0.70 s (before the Rice runs). The speed of such a build depends on what the compiler
chooses to vectorize, which no test can pin down: after a new Rust release, time the
decoding of both builds again (measured here with Rust 1.99.1).

### Where the time goes

perf, by time, on one core in the baseline build. Encoding at the default setting,
24 bit at 96 kHz: window, autocorrelation and LPC residuals 30 %, MD5 19 % and the
samples' bytes for it 3 %, writing Rice codes 11 %, the FIXED search 11 %, partition
sums and headers 9 %, exact residual sizes 3 %, CRC-16 2 %. Checking the 64 files: MD5
30 %, long predictors 21 %, reading Rice codes 19 %, the fixed-size prediction loops
14 %, the samples' bytes 4 %, metadata blocks 3 %, CRC-16 2 %.

What each of the speed commits of 2026-11-10 brought, measured against the commit
before it on the 64 files: the MD5 took 6 % off the default encode, 10 % off the fast
one and 6 % off checking (hashing alone: 895 to 1,182 MB/s); `sum_of` 8 %, 6 % and 7 %
off the default, fast and best encodes; the float residuals 6 % and 9 % off the
default and best encodes; converting a buffer of samples at a time 4 % and 5 % off the
default and fast encodes; the autocorrelation in registers 3 % off the best encode;
the Rice runs 15 % off decoding (17 % in the x86-64-v3 build); the long predictors
10 %, the interleaving 8 % and CRC-16 by sixteen 6 % off decoding.

The decode figures are shaped by three test files that use only 32nd-order predictors.
The largest, 8 channels at 192 kHz and 24 bits, takes 0.20 s of the 0.497 s. It is the
one file of the 64 that the baseline build does not check clearly faster than libFLAC:
0.268 s against 0.272 s (decoding to bytes 0.206 s against 0.209 s); the x86-64-v3
build takes 0.225 s and 0.164 s. At 24 bits its products need 64 bits, and without
AVX2 that is two at a time at best.

Tried and dropped because they were slower or no faster: summing FIXED magnitudes
inside the difference loop; a sign-bit overflow check inside `Vec::extend`; keeping
the whole prediction history in a local array; adding the message words and constants
of the MD5 beforehand in a function of its own (1,018 MB/s against 1,182 MB/s); other
splits between integer and `f64` samples for long predictors (2 to 12 integer samples
all within 4 %); laying the long predictors' groups of products out without a loop
(the compiler then no longer vectorized them: 0.234 s instead of 0.196 s for the
8-channel file); checked products in the long predictors' loop too (0.225 s instead of
0.194 s for that file in the x86-64-v3 build) or only for the newest samples of the
fixed-size loops (0.62 s instead of 0.57 s for the 64 files there); and finding a Rice
code's end with `checked_ilog2` (2 % slower in the baseline build).

Not measured or tested as of 2026-11-10: the x86-64-v3 build on Intel processors (the
figures are from one AMD processor); the speed on aarch64 (CI only tests there);
32-bit targets; libFLAC with several threads (`flac -j`).
