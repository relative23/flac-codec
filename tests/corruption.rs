// Copyright 2025 Brian Langenberger
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[test]
fn test_file_corruption() {
    use flac_codec::byteorder::LittleEndian;
    use flac_codec::decode::FlacByteReader;
    use std::io::{copy, sink};

    // ensure test file is okay
    let flac = include_bytes!("data/sine.flac");

    assert!(
        copy(
            &mut FlacByteReader::endian(&flac[..], LittleEndian).unwrap(),
            &mut sink()
        )
        .is_ok()
    );

    // try swapping some random bits outside the metadata block area
    // (flipping a bit somewhere in a PADDING block might not be
    // noticed, for instance)
    let valid_range = 136..flac.len();

    for _ in 0..100 {
        let mut flac: Vec<u8> = Vec::from(flac);
        flac[fastrand::usize(valid_range.clone())] ^= 1 << fastrand::u32(0..8);

        assert!(
            copy(
                &mut FlacByteReader::endian(&flac[..], LittleEndian).unwrap(),
                &mut sink()
            )
            .is_err()
        );
    }
}

#[test]
fn test_samples_beyond_streaminfo_total() {
    use flac_codec::Error;
    use flac_codec::decode::FlacSampleReader;
    use flac_codec::encode::{FlacSampleWriter, Options};
    use std::io::Cursor;

    // two 4096-sample blocks of stereo audio
    let samples: Vec<i32> = (0..8192 * 2).map(|i| (i % 1000) - 500).collect();
    let mut flac = Cursor::new(Vec::new());
    let mut writer = FlacSampleWriter::new(
        &mut flac,
        Options::default(),
        44100,
        16,
        2,
        Some(samples.len() as u64),
    )
    .unwrap();
    writer.write(&samples).unwrap();
    writer.finalize().unwrap();
    let mut flac = flac.into_inner();

    // claim 5000 samples in STREAMINFO, so the second block runs past the total
    // (the total is the low 36 bits of the 64 bits at STREAMINFO offset 10)
    let at = 4 + 4 + 10;
    let packed = u64::from_be_bytes(flac[at..at + 8].try_into().unwrap());
    let packed = (packed & !0xF_FFFF_FFFF) | 5000;
    flac[at..at + 8].copy_from_slice(&packed.to_be_bytes());

    let mut reader = FlacSampleReader::new(Cursor::new(flac)).unwrap();
    assert!(matches!(
        reader.read_to_end(&mut Vec::new()),
        Err(Error::TooManySamples)
    ));
}
