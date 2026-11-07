# Patches on top of flac-codec 1.3.3

`nastra-1.3.3-hardened` is upstream release 1.3.3 (`a7b1ffb`) plus the commits below,
in this order, without merges. NASTRA pins an exact commit of this branch, never the
branch name.

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

## Fork only

`PATCHES.md`, `tests/hardening.rs` and `.github/workflows/checks.yml`. They are not
part of any upstream pull request.

`tests/hardening.rs`:

- `round_trip_every_depth_layout_and_option`: 4 to 32 bits, 1 to 8 channels, tones,
  noise and full-scale square waves, default, fast and best settings and partition
  order 15.
- `damaged_streams_never_panic`: bit flips, truncation and garbage in the metadata and
  frames, with a fixed seed. `HARDENING_ROUNDS` sets the count (3,000 by default; CI
  runs 1,000,000 in a release build with overflow checks).
- `reference_decoders_agree` (ignored by default; needs `flac` and `ffmpeg`): libFLAC's
  `flac --test` accepts every stream (frame CRCs and MD5), and libFLAC and ffmpeg decode
  the same samples that were encoded. 288 streams: 8 to 32 bits, 1, 2 and 6 channels,
  44.1 to 384 kHz.

## Moving to a new upstream release

1. Branch `nastra-<version>-hardened` from the release commit.
2. For each fix, run its regression test on the release without the fix. If it passes,
   upstream has fixed it: drop the commit and note the upstream commit here.
   Otherwise cherry-pick it.
3. Cherry-pick the fork-only commit and update `UPSTREAM_BASE` in the workflow.
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
  clippy reports 18 warnings, the same as upstream 1.3.3.

## Speed (2026-11-07, one pinned core, 64 IETF subset files with 98.6 million samples)

Best of 5 runs, three rounds (two for `verify`) alternating between upstream 1.3.3
and this branch:

| | upstream 1.3.3 | this branch | change | libFLAC 1.5.0 |
|---|---|---|---|---|
| decode to samples | 1.553 s | 1.490 s | −4.1 % | |
| decode with MD5 check (`verify`, `flac -t`) | 1.965 s | 1.968 s | none | 0.837 s |
| encode, fast (`flac -0`) | 2.405 s | 2.352 s | −2.2 % | 0.682 s |
| encode, default (`flac -5`) | 5.626 s | 5.467 s | −2.8 % | 1.139 s |
| encode, best (`flac -8`) | 6.085 s | 5.979 s | −1.7 % | 2.859 s |

The encoder executes 0.8 % fewer instructions (cachegrind, three files); the decoder
executes 0.18 % fewer, so its gain comes from inlining rather than from less work. The
presets do not match libFLAC's levels exactly, and libFLAC's times include reading WAV
files and writing FLAC files on tmpfs, while flac-codec works in memory. libFLAC is
2.1 to 4.8 times faster.
