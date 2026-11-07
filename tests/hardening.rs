// Checks of the NASTRA branch (see PATCHES.md): damaged streams never panic, every
// supported layout round-trips, and libFLAC (`flac`) and ffmpeg decode what this crate
// writes to the same samples.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use flac_codec::decode::FlacSampleReader;
use flac_codec::encode::{FlacSampleWriter, Options};
use std::io::Cursor;

/// Deterministic xorshift, so every run checks the same streams.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Tones,
    Noise,
    Square,
}

/// Interleaved samples: tones with a little noise, full-scale noise, or a full-scale
/// square wave (the hardest case for prediction and decorrelation).
fn signal(kind: Kind, bits: u32, channels: usize, frames: usize, seed: u64) -> Vec<i32> {
    let max = ((1i64 << (bits - 1)) - 1) as f64;
    let min = -(1i64 << (bits - 1)) as f64;
    let mut rng = Rng(seed | 1);
    (0..frames * channels)
        .map(|i| {
            let (n, c) = ((i / channels) as f64, (i % channels) as f64);
            let v = match kind {
                Kind::Tones => {
                    let t = 0.45 * (n * 0.031 + c).sin() + 0.25 * (n * 0.0071).sin();
                    t * max + (rng.below(17) as f64 - 8.0)
                }
                Kind::Noise => min + (rng.next() >> 11) as f64 / (1u64 << 53) as f64 * (max - min),
                Kind::Square => {
                    if (i / channels + i % channels).is_multiple_of(2) {
                        max
                    } else {
                        min
                    }
                }
            };
            v.round().clamp(min, max) as i32
        })
        .collect()
}

fn encode(options: Options, rate: u32, bits: u32, channels: u8, samples: &[i32]) -> Vec<u8> {
    let mut flac = Cursor::new(vec![]);
    let mut writer = FlacSampleWriter::new(
        &mut flac,
        options,
        rate,
        bits,
        channels,
        Some(samples.len() as u64),
    )
    .unwrap();
    // in uneven pieces, as a recorder hands them over
    for piece in samples.chunks(usize::from(channels) * 997) {
        writer.write(piece).unwrap();
    }
    writer.finalize().unwrap();
    flac.into_inner()
}

fn decode(flac: &[u8]) -> Result<Vec<i32>, flac_codec::Error> {
    let mut samples = vec![];
    FlacSampleReader::new(Cursor::new(flac))?.read_to_end(&mut samples)?;
    Ok(samples)
}

#[test]
fn round_trip_every_depth_layout_and_option() {
    let presets = || {
        [
            ("default", Options::default()),
            ("fast", Options::fast()),
            ("best", Options::best()),
            (
                "partition order 15",
                Options::default().max_partition_order(15).unwrap(),
            ),
        ]
    };
    for bits in [4, 8, 12, 16, 20, 24, 32] {
        for channels in [1u8, 2, 3, 6, 8] {
            for kind in [Kind::Tones, Kind::Noise, Kind::Square] {
                let samples = signal(kind, bits, channels.into(), 5000, u64::from(bits));
                for (name, opts) in presets() {
                    let flac = encode(opts, 96000, bits, channels, &samples);
                    assert_eq!(
                        decode(&flac).unwrap(),
                        samples,
                        "{bits} bit, {channels} channels, {kind:?}, {name}"
                    );
                }
            }
        }
    }
}

#[test]
fn damaged_streams_never_panic() {
    // HARDENING_ROUNDS raises the count (CI runs a release build with many more)
    let rounds: usize = std::env::var("HARDENING_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let seeds: Vec<Vec<u8>> = [
        (Kind::Tones, 16, 2u8, Options::default()),
        (Kind::Noise, 24, 2, Options::best()),
        (Kind::Square, 32, 2, Options::default()),
        (Kind::Tones, 8, 1, Options::fast()),
        (Kind::Tones, 24, 6, Options::default()),
        (
            Kind::Tones,
            16,
            2,
            Options::default().max_partition_order(15).unwrap(),
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(k, (kind, bits, channels, opts))| {
        let samples = signal(kind, bits, channels.into(), 6000, 7 + k as u64);
        encode(opts, 48000, bits, channels, &samples)
    })
    .collect();

    let mut rng = Rng(0xF1AC_5EED);
    let mut panics = vec![];
    for round in 0..rounds {
        let mut flac = seeds[round % seeds.len()].clone();
        match rng.below(4) {
            0 => {
                // flip a few bits anywhere
                for _ in 0..1 + rng.below(8) {
                    let at = rng.below(flac.len());
                    flac[at] ^= 1 << rng.below(8);
                }
            }
            1 => {
                // cut the stream short
                let keep = rng.below(flac.len());
                flac.truncate(keep);
            }
            2 => {
                // garbage in the metadata
                for _ in 0..1 + rng.below(4) {
                    let at = rng.below(flac.len().min(128));
                    flac[at] = rng.next() as u8;
                }
            }
            _ => {
                // a run of garbage in the frames
                let at = rng.below(flac.len());
                let len = rng.below(64).min(flac.len() - at);
                flac[at..at + len]
                    .iter_mut()
                    .for_each(|b| *b = rng.next() as u8);
            }
        }
        if std::panic::catch_unwind(|| {
            let _ = decode(&flac);
        })
        .is_err()
        {
            panics.push(round);
        }
    }
    assert!(panics.is_empty(), "panics in rounds {panics:?}");
}

/// Decodes `flac` with an external program into signed 32-bit little-endian samples,
/// shifted back to `bits`.
fn external(program: &str, args: &[&str], raw: &std::path::Path, bits: u32) -> Vec<i32> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(status.success(), "{program} {args:?} failed");
    let bytes = std::fs::read(raw).unwrap();
    let (words, rest) = bytes.as_chunks::<4>();
    assert!(rest.is_empty(), "{program} wrote a partial sample");
    words
        .iter()
        .map(|w| i32::from_le_bytes(*w) >> (32 - bits))
        .collect()
}

#[test]
#[ignore = "needs the flac (libFLAC) and ffmpeg programs; run in CI"]
fn reference_decoders_agree() {
    let dir = std::env::temp_dir().join(format!("flac-hardening-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut checked = 0;
    for bits in [8u32, 16, 24, 32] {
        for channels in [1u8, 2, 6] {
            for rate in [44100u32, 96000, 192000, 384000] {
                for kind in [Kind::Tones, Kind::Noise, Kind::Square] {
                    let samples = signal(kind, bits, channels.into(), 20000, u64::from(rate));
                    for opts in [Options::default(), Options::best()] {
                        let flac = dir.join("t.flac");
                        std::fs::write(&flac, encode(opts, rate, bits, channels, &samples))
                            .unwrap();
                        let f = flac.to_str().unwrap();
                        // libFLAC checks the frame CRCs and the MD5 of the whole stream
                        let status = std::process::Command::new("flac")
                            .args(["--test", "--silent", f])
                            .status()
                            .expect("flac");
                        assert!(
                            status.success(),
                            "flac --test: {bits} bit {channels} ch {rate} Hz {kind:?}"
                        );
                        // libFLAC's samples go through a WAV file; ffmpeg widens both
                        // sides to 32 bits, which the comparison shifts back
                        let wav = dir.join("t.wav");
                        let status = std::process::Command::new("flac")
                            .args(["--decode", "--silent", "--force", "-o"])
                            .arg(&wav)
                            .arg(&flac)
                            .status()
                            .expect("flac");
                        assert!(status.success(), "flac --decode");
                        let raw = dir.join("t.raw");
                        let w = wav.to_str().unwrap();
                        let r = raw.to_str().unwrap();
                        let ff = ["-v", "error", "-nostdin", "-y", "-i"];
                        let to_s32 = ["-f", "s32le", "-c:a", "pcm_s32le"];
                        let from_libflac = external(
                            "ffmpeg",
                            &[&ff[..], &[w], &to_s32[..], &[r]].concat(),
                            &raw,
                            bits,
                        );
                        assert_eq!(
                            from_libflac, samples,
                            "libFLAC: {bits} bit {channels} ch {rate} Hz {kind:?}"
                        );
                        let from_ffmpeg = external(
                            "ffmpeg",
                            &[&ff[..], &[f], &to_s32[..], &[r]].concat(),
                            &raw,
                            bits,
                        );
                        assert_eq!(
                            from_ffmpeg, samples,
                            "ffmpeg: {bits} bit {channels} ch {rate} Hz {kind:?}"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
    println!("{checked} streams decoded identically by libFLAC and ffmpeg");
}
