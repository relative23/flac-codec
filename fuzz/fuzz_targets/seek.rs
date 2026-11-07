// A stream and a position: seeking and reading on must end, without a panic.
#![no_main]

use flac_codec::decode::FlacSampleReader;
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|input: (u64, &[u8])| {
    let (position, data) = input;
    if let Ok(mut reader) = FlacSampleReader::new(Cursor::new(data)) {
        if reader.seek(position).is_ok() {
            let mut buf = vec![0; 4096];
            while let Ok(1..) = reader.read(&mut buf) {}
        }
    }
});
