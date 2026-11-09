// Any samples, layout and options the API accepts: encoding must succeed and decode to
// the same samples.
#![no_main]

use flac_codec::decode::FlacSampleReader;
use flac_codec::encode::{FlacSampleWriter, Options, Window};
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

fuzz_target!(|input: Input| {
    let bits = 4 + u32::from(input.bits) % 29;
    let channels = 1 + input.channels % 8;
    let rate = 1 + input.rate % 1_048_575;
    let window = match input.window.0 % 3 {
        0 => Window::Rectangle,
        1 => Window::Hann,
        _ => Window::Tukey(input.window.1),
    };
    let options = Options::default()
        .block_size(input.block_size.max(16))
        .unwrap()
        .max_lpc_order(input.max_lpc_order.map(|o| 1 + o % 32))
        .unwrap()
        .max_partition_order(u32::from(input.max_partition_order % 16))
        .unwrap()
        .mid_side(input.mid_side)
        .fast_channel_correlation(input.fast_channel_correlation)
        .window(window)
        .threads(std::num::NonZero::new(1 + usize::from(input.threads % 4)).unwrap());

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

    let mut flac = Cursor::new(vec![]);
    let mut writer = FlacSampleWriter::new(&mut flac, options, rate, bits, channels, total)
        .expect("valid parameters");
    let piece = usize::from(channels) * (1 + usize::from(input.piece % 4096));
    for p in samples.chunks(piece) {
        writer.write(p).expect("write");
    }
    writer.finalize().expect("finalize");

    let mut decoded = vec![];
    FlacSampleReader::new(Cursor::new(flac.into_inner()))
        .expect("written stream opens")
        .read_to_end(&mut decoded)
        .expect("written stream decodes");
    assert_eq!(decoded, samples);
});
