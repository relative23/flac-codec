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

/// The undamaged streams that the damage tests start from
fn seed_streams() -> Vec<Vec<u8>> {
    [
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
    .collect()
}

/// Damages `flac` at random: bit flips, truncation, garbage
fn damage(rng: &mut Rng, flac: &mut Vec<u8>) {
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
}

#[test]
fn damaged_streams_never_panic() {
    // HARDENING_ROUNDS raises the count (CI runs a release build with many more)
    let rounds: usize = std::env::var("HARDENING_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let seeds = seed_streams();
    let mut rng = Rng(0xF1AC_5EED);
    let mut panics = vec![];
    for round in 0..rounds {
        let mut flac = seeds[round % seeds.len()].clone();
        damage(&mut rng, &mut flac);
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

/// A stream that hands out at most `chunk` bytes per read, like a pipe
struct Chunked<'d> {
    data: &'d [u8],
    chunk: usize,
}

impl std::io::Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.chunk).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// The samples decoded before the end or the first error, and how it ended
fn decode_all(reader: impl std::io::Read) -> (Vec<i32>, String) {
    fn ending(err: &flac_codec::Error) -> String {
        match err {
            flac_codec::Error::Io(io) => format!("Io({:?})", io.kind()),
            err => format!("{err:?}"),
        }
    }
    let mut samples = vec![];
    let mut reader = match FlacSampleReader::new(reader) {
        Ok(reader) => reader,
        Err(err) => return (samples, ending(&err)),
    };
    let mut buf = vec![0; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return (samples, "end".to_string()),
            Ok(n) => samples.extend_from_slice(&buf[..n]),
            Err(err) => return (samples, ending(&err)),
        }
    }
}

/// Decoding from a source that hands out small pieces, which makes frames
/// run past what has been read ahead, gives the same samples and the same
/// error as decoding from memory, for undamaged and damaged streams
#[test]
fn chunked_input_decodes_the_same() {
    let seeds = seed_streams();
    let mut rng = Rng(0x00C4_0C4E);
    for round in 0..300 {
        let mut flac = seeds[round % seeds.len()].clone();
        if round >= seeds.len() {
            damage(&mut rng, &mut flac);
        }
        let whole = decode_all(Cursor::new(&flac));
        for chunk in [1, 3, 4093] {
            assert_eq!(
                decode_all(Chunked { data: &flac, chunk }),
                whole,
                "round {round}, {chunk} bytes per read"
            );
        }
    }
}

/// CRC-16 of a FLAC frame (polynomial 0x8005), and CRC-8 of its header (0x07)
fn crc(bytes: &[u8], poly: u16, width: u32) -> u16 {
    let top = 1u32 << (width - 1);
    let mask = (1u32 << width) - 1;
    let mut crc = 0u32;
    for byte in bytes {
        crc ^= u32::from(*byte) << (width - 8);
        for _ in 0..8 {
            crc = if crc & top != 0 {
                ((crc << 1) ^ u32::from(poly)) & mask
            } else {
                (crc << 1) & mask
            };
        }
    }
    crc as u16
}

/// A frame larger than the decoder reads ahead (4 MiB) decodes from the
/// stream: a valid 16-sample frame whose first residual is a Rice code with
/// 40 million leading zeros
#[test]
fn a_frame_larger_than_the_read_ahead_decodes() {
    use bitstream_io::{BigEndian, BitWrite, BitWriter};

    let zeros: u32 = 40_000_000;
    let mut frame = BitWriter::endian(Vec::new(), BigEndian);
    // header: sync, fixed blocking; block size 16 (8-bit field), rate and
    // bits per sample from STREAMINFO, mono, frame number 0
    frame.write::<15, u16>(0b111111111111100).unwrap();
    frame.write::<1, u8>(0).unwrap();
    frame.write::<4, u8>(0b0110).unwrap();
    frame.write::<4, u8>(0b0000).unwrap();
    frame.write::<4, u8>(0b0000).unwrap();
    frame.write::<3, u8>(0b000).unwrap();
    frame.write::<1, u8>(0).unwrap();
    frame.write::<8, u8>(0).unwrap();
    frame.write::<8, u8>(16 - 1).unwrap();
    let mut bytes = frame.into_writer();
    bytes.push(crc(&bytes, 0x07, 8) as u8);
    let mut frame = BitWriter::endian(bytes, BigEndian);
    // subframe: FIXED order 0, Rice coding, partition order 0, parameter 0
    frame.write::<8, u8>(0b0001_0000).unwrap();
    frame.write::<2, u8>(0).unwrap();
    frame.write::<4, u8>(0).unwrap();
    frame.write::<4, u8>(0).unwrap();
    // first residual: 40 million zeros, then the stop bit; the rest zero
    for _ in 0..zeros / 32 {
        frame.write::<32, u32>(0).unwrap();
    }
    frame.write::<1, u8>(1).unwrap();
    for _ in 1..16 {
        frame.write::<1, u8>(1).unwrap();
    }
    frame.byte_align().unwrap();
    let mut bytes = frame.into_writer();
    let crc16 = crc(&bytes, 0x8005, 16);
    bytes.extend_from_slice(&crc16.to_be_bytes());
    assert!(bytes.len() > 4 << 20);

    // STREAMINFO: 16-sample blocks, 44.1 kHz, mono, 16 bits, 16 samples
    let mut flac = b"fLaC".to_vec();
    let mut info = BitWriter::endian(Vec::new(), BigEndian);
    info.write::<1, u8>(1).unwrap();
    info.write::<7, u8>(0).unwrap();
    info.write::<24, u32>(34).unwrap();
    info.write::<16, u16>(16).unwrap();
    info.write::<16, u16>(16).unwrap();
    info.write::<24, u32>(0).unwrap();
    info.write::<24, u32>(0).unwrap();
    info.write::<20, u32>(44100).unwrap();
    info.write::<3, u8>(0).unwrap();
    info.write::<5, u8>(15).unwrap();
    info.write::<36, u64>(16).unwrap();
    info.write_bytes(&[0; 16]).unwrap();
    flac.extend(info.into_writer());
    flac.extend(&bytes);

    // the Rice value is the count of zeros; zigzag decoded, it is half that
    let mut expected = vec![(zeros / 2) as i32];
    expected.resize(16, 0);
    assert_eq!(decode(&flac).unwrap(), expected);
    assert_eq!(
        decode_all(Chunked {
            data: &flac,
            chunk: 1 << 20
        }),
        (expected, "end".to_string())
    );
}

/// Interleaved samples of a randomly chosen kind, including the edges of the range.
fn random_signal(rng: &mut Rng, bits: u32, channels: usize, frames: usize) -> Vec<i32> {
    let max = (1i64 << (bits - 1)) - 1;
    let min = -(1i64 << (bits - 1));
    let full = |rng: &mut Rng| min + (rng.next() % (max - min + 1) as u64) as i64;
    let kind = rng.below(10);
    let level = full(rng);
    let shift = rng.below(bits as usize - 1) as u32;
    let mut walk = vec![0i64; channels];
    let mut out: Vec<i32> = Vec::with_capacity(frames * channels);
    for n in 0..frames {
        for c in 0..channels {
            let v = match kind {
                0 => ((n as f64 * (0.001 + 0.05 * c as f64)).sin() * max as f64) as i64,
                1 => full(rng),
                2 => [max, min][(n + c) % 2],
                3 => level,
                4 if rng.below(97) == 0 => full(rng),
                4 => 0,
                5 => rng.below(3) as i64 - 1,
                // wasted bits: the low bits of every sample are zero
                6 => (full(rng) >> shift) << shift,
                7 => {
                    walk[c] = (walk[c] + full(rng) / 64).clamp(min, max);
                    walk[c]
                }
                // channels that are copies or inversions of the first
                8 if c > 0 => i64::from(out[out.len() - c]),
                9 if c > 0 => -i64::from(out[out.len() - c]),
                _ => full(rng),
            };
            out.push(v.clamp(min, max) as i32);
        }
    }
    out
}

/// Random layouts, rates, lengths, block sizes, options and signals, written in random
/// pieces: the encoder must never panic, and every stream must decode to its input.
#[test]
fn encoder_random_inputs_round_trip() {
    use flac_codec::encode::Window;

    // ENCODER_ROUNDS raises the count (CI runs a release build with many more)
    let rounds: usize = std::env::var("ENCODER_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let mut rng = Rng(0x00E4_C0DE_5EED);
    let mut failures = vec![];
    for round in 0..rounds {
        let bits = 4 + rng.below(29) as u32;
        let channels = 1 + rng.below(8) as u8;
        let rate = match rng.below(3) {
            0 => [8000, 44100, 48000, 96000, 192000, 384000][rng.below(6)],
            _ => 1 + rng.below(1_048_575) as u32,
        };
        let block = match rng.below(3) {
            0 => [16, 192, 576, 1152, 4096, 4608, 16384, 32768, 65535][rng.below(9)],
            _ => 16 + rng.below(65520) as u16,
        };
        let frames = match rng.below(4) {
            0 => rng.below(40),
            // around a multiple of the block size
            1 => (usize::from(block) * (1 + rng.below(3)) + rng.below(3)).saturating_sub(1),
            _ => rng.below(30000),
        };
        let window = match rng.below(5) {
            0 => Window::Rectangle,
            1 => Window::Hann,
            2 => Window::Tukey(rng.below(1001) as f32 / 1000.0),
            3 => Window::Tukey([f32::NAN, -1.0, 2.0, f32::INFINITY, 0.0, 1.0][rng.below(6)]),
            _ => Window::default(),
        };
        let mut options = Options::default()
            .block_size(block)
            .unwrap()
            .max_lpc_order([None, Some(1 + rng.below(32) as u8)][rng.below(4).min(1)])
            .unwrap()
            .max_partition_order(rng.below(16) as u32)
            .unwrap()
            .mid_side(rng.below(2) == 0)
            .fast_channel_correlation(rng.below(2) == 0)
            .window(window);
        if rng.below(2) == 0 {
            options = options.no_padding();
        }
        match rng.below(3) {
            0 => options = options.no_seektable(),
            1 => options = options.seektable_frames(1 + rng.below(100)),
            _ => {}
        }
        let samples = random_signal(&mut rng, bits, channels.into(), frames);
        let total = (frames > 0 && rng.below(2) == 0).then_some(samples.len() as u64);
        let piece = usize::from(channels) * (1 + rng.below(5000));
        let config = format!(
            "round {round}: {bits} bit, {channels} ch, {rate} Hz, {frames} frames, \
             total {total:?}, pieces of {piece}, {options:?}"
        );

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut flac = Cursor::new(vec![]);
            let mut writer = FlacSampleWriter::new(&mut flac, options, rate, bits, channels, total)
                .map_err(|e| format!("new: {e}"))?;
            for p in samples.chunks(piece) {
                writer.write(p).map_err(|e| format!("write: {e}"))?;
            }
            writer.finalize().map_err(|e| format!("finalize: {e}"))?;
            let flac = flac.into_inner();
            let checked = match decode(&flac) {
                Ok(decoded) if decoded == samples => Ok(()),
                Ok(_) => Err("decoded samples differ".to_string()),
                Err(e) => Err(format!("decode: {e}")),
            };
            // HARDENING_DUMP=<dir> keeps every stream that failed, for other decoders
            if let (Err(_), Ok(dir)) = (&checked, std::env::var("HARDENING_DUMP")) {
                std::fs::write(format!("{dir}/encoder-round-{round}.flac"), &flac).unwrap();
                let pcm: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                std::fs::write(format!("{dir}/encoder-round-{round}.s32"), pcm).unwrap();
            }
            checked
        }));
        match result {
            Ok(Ok(())) => {}
            // FLAC cannot describe an empty stream
            Ok(Err(_)) if frames == 0 => {}
            Ok(Err(e)) => failures.push(format!("{config}: {e}")),
            Err(_) => failures.push(format!("{config}: panic")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Encoding on several threads writes the same bytes as on one, for every
/// writer type, with and without a known total, and with a short last frame
#[test]
fn threads_write_the_same_bytes() {
    use flac_codec::byteorder::LittleEndian;
    use flac_codec::encode::{FlacByteWriter, FlacChannelWriter};
    use std::io::Write;
    use std::num::NonZero;

    let mut rng = Rng(0x7E7E_AD5E_ED01);
    for round in 0..40 {
        let bits = [8u32, 16, 24, 32][round % 4];
        let channels = 1 + rng.below(4) as u8;
        let frames = 1 + rng.below(60000);
        let samples = random_signal(&mut rng, bits, channels.into(), frames);
        let total = (rng.below(2) == 0).then_some(samples.len() as u64);
        let base = Options::default()
            .block_size([192, 1152, 4096][rng.below(3)])
            .unwrap()
            .seektable_frames(1 + rng.below(20));
        let piece = usize::from(channels) * (1 + rng.below(9000));

        let sample_writer = |threads: usize| {
            let mut flac = Cursor::new(vec![]);
            let options = base.clone().threads(NonZero::new(threads).unwrap());
            let mut w =
                FlacSampleWriter::new(&mut flac, options, 44100, bits, channels, total).unwrap();
            samples.chunks(piece).for_each(|p| w.write(p).unwrap());
            w.finalize().unwrap();
            flac.into_inner()
        };
        let byte_writer = |threads: usize| {
            let bytes: Vec<u8> = samples
                .iter()
                .flat_map(|s| s.to_le_bytes()[..bits.div_ceil(8) as usize].to_vec())
                .collect();
            let mut flac = Cursor::new(vec![]);
            let options = base.clone().threads(NonZero::new(threads).unwrap());
            let mut w = FlacByteWriter::endian(
                &mut flac,
                LittleEndian,
                options,
                44100,
                bits,
                channels,
                total.map(|t| t * u64::from(bits.div_ceil(8))),
            )
            .unwrap();
            bytes
                .chunks(piece * 3)
                .for_each(|p| w.write_all(p).unwrap());
            w.finalize().unwrap();
            flac.into_inner()
        };
        let channel_writer = |threads: usize| {
            let split: Vec<Vec<i32>> = (0..usize::from(channels))
                .map(|c| {
                    samples
                        .iter()
                        .skip(c)
                        .step_by(channels.into())
                        .copied()
                        .collect()
                })
                .collect();
            let mut flac = Cursor::new(vec![]);
            let options = base.clone().threads(NonZero::new(threads).unwrap());
            let mut w = FlacChannelWriter::new(
                &mut flac,
                options,
                44100,
                bits,
                channels,
                total.map(|t| t / u64::from(channels)),
            )
            .unwrap();
            let step = piece / usize::from(channels);
            for start in (0..frames).step_by(step) {
                let end = (start + step).min(frames);
                w.write(split.iter().map(|c| &c[start..end]).collect::<Vec<_>>())
                    .unwrap();
            }
            w.finalize().unwrap();
            flac.into_inner()
        };

        // dropping the writer instead of finalizing it writes the queued frames,
        // as it writes the frames of one thread
        let dropped = |threads: usize| {
            let mut flac = Cursor::new(vec![]);
            let options = base.clone().threads(NonZero::new(threads).unwrap());
            let mut w =
                FlacSampleWriter::new(&mut flac, options, 44100, bits, channels, total).unwrap();
            samples.chunks(piece).for_each(|p| w.write(p).unwrap());
            drop(w);
            flac.into_inner()
        };

        let one = sample_writer(1);
        assert_eq!(decode(&one).unwrap(), samples, "round {round}");
        assert_eq!(dropped(4), dropped(1), "round {round}, dropped");
        for threads in [2, 3, 8] {
            assert_eq!(
                sample_writer(threads),
                one,
                "round {round}, {threads} threads"
            );
        }
        assert_eq!(byte_writer(3), byte_writer(1), "round {round}, bytes");
        assert_eq!(
            channel_writer(3),
            channel_writer(1),
            "round {round}, channels"
        );
    }
}

/// Writing more samples than declared fails the same way on several threads:
/// the same error, and the same frames before it
#[test]
fn threads_fail_the_same_way() {
    use std::num::NonZero;

    let samples = signal(Kind::Tones, 16, 2, 50000, 3);
    let attempt = |threads: usize| {
        let mut flac = Cursor::new(vec![]);
        let options = Options::default()
            .block_size(1152)
            .unwrap()
            .threads(NonZero::new(threads).unwrap());
        let mut w = FlacSampleWriter::new(
            &mut flac,
            options,
            44100,
            16,
            2,
            Some(samples.len() as u64 / 2),
        )
        .unwrap();
        // the error may come from a later write or from finalizing
        let written = samples
            .chunks(2 * 1000)
            .map(|p| w.write(p))
            .find_map(Result::err);
        let error = match written {
            Some(err) => {
                // dropping the writer finalizes what it can
                drop(w);
                Some(err)
            }
            None => w.finalize().err(),
        }
        .map(|err| format!("{err:?}"));
        (error, flac.into_inner())
    };
    let (error, bytes) = attempt(1);
    assert_eq!(error.as_deref(), Some("ExcessiveTotalSamples"));
    for threads in [2, 5] {
        assert_eq!(
            attempt(threads),
            (error.clone(), bytes.clone()),
            "{threads} threads"
        );
    }
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
