// Any input: decoding samples and checking the MD5 must end, without a panic.
#![no_main]

use flac_codec::decode::{FlacSampleReader, verify_reader};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    if let Ok(mut reader) = FlacSampleReader::new(Cursor::new(data)) {
        let mut buf = vec![0; 1 << 16];
        while let Ok(1..) = reader.read(&mut buf) {}
    }
    let _ = verify_reader(data);
});
