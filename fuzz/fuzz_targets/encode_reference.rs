// Any samples, layout and options, on one to four threads: the encoder must write
// the bytes that the branch before the encoder speed work writes
// (`flac-codec-reference`), and fail where it fails.
#![no_main]

use libfuzzer_sys::arbitrary::{self, Arbitrary};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

#[derive(Arbitrary, Debug)]
struct Input {
    bits: u8,
    channels: u8,
    rate: u32,
    block_size: u16,
    max_lpc_order: Option<u8>,
    max_partition_order: u8,
    mid_side: bool,
    fast_channel_correlation: bool,
    window: (u8, f32),
    total_known: bool,
    threads: u8,
    piece: u16,
    samples: Vec<i32>,
}

/// The same options for either crate
macro_rules! options {
    ($krate:ident, $input:expr) => {{
        use $krate::encode::{Options, Window};
        let window = match $input.window.0 % 3 {
            0 => Window::Rectangle,
            1 => Window::Hann,
            _ => Window::Tukey($input.window.1),
        };
        Options::default()
            .block_size($input.block_size.max(16))
            .unwrap()
            .max_lpc_order($input.max_lpc_order.map(|o| 1 + o % 32))
            .unwrap()
            .max_partition_order(u32::from($input.max_partition_order % 16))
            .unwrap()
            .mid_side($input.mid_side)
            .fast_channel_correlation($input.fast_channel_correlation)
            .window(window)
    }};
}

/// Encodes with either crate: the stream, or how it failed
macro_rules! encode {
    ($krate:ident, $options:expr, $rate:expr, $bits:expr, $channels:expr, $total:expr, $samples:expr, $piece:expr) => {{
        let mut flac = Cursor::new(vec![]);
        let written = $krate::encode::FlacSampleWriter::new(
            &mut flac, $options, $rate, $bits, $channels, $total,
        )
        .and_then(|mut writer| {
            for p in $samples.chunks($piece) {
                writer.write(p)?;
            }
            writer.finalize()
        });
        match written {
            Ok(()) => Ok(flac.into_inner()),
            Err(err) => Err(format!("{err:?}").split('(').next().unwrap().to_string()),
        }
    }};
}

fuzz_target!(|input: Input| {
    let bits = 4 + u32::from(input.bits) % 29;
    let channels = 1 + input.channels % 8;
    let rate = 1 + input.rate % 1_048_575;
    let threads = std::num::NonZero::new(1 + usize::from(input.threads % 4)).unwrap();

    // sign-extend each sample from `bits`, so every value of the range is reachable
    let mut samples: Vec<i32> = input
        .samples
        .iter()
        .map(|s| (s << (32 - bits)) >> (32 - bits))
        .collect();
    samples.truncate(samples.len() / usize::from(channels) * usize::from(channels));
    if samples.is_empty() {
        return;
    }
    let total = input.total_known.then_some(samples.len() as u64);
    let piece = usize::from(channels) * (1 + usize::from(input.piece % 4096));

    let reference = encode!(
        flac_codec_reference,
        options!(flac_codec_reference, input),
        rate,
        bits,
        channels,
        total,
        samples,
        piece
    );
    let ours = encode!(
        flac_codec,
        options!(flac_codec, input).threads(threads),
        rate,
        bits,
        channels,
        total,
        samples,
        piece
    );
    match (&ours, &reference) {
        (Ok(a), Ok(b)) => assert!(a == b, "streams differ"),
        (Err(a), Err(b)) => assert_eq!(a, b),
        _ => panic!(
            "{:?} against {:?}",
            ours.as_ref().map(Vec::len),
            reference.as_ref().map(Vec::len)
        ),
    }
});
