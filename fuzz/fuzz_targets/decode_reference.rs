// Any input, read in pieces of any size and optionally after a seek: the samples
// and the way decoding ends must match the branch before the speed work on
// decoding (`flac-codec-reference`, reading the same bytes in one piece).
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read};

/// A stream that hands out at most `chunk` bytes per read
struct Chunked<'d> {
    data: &'d [u8],
    chunk: usize,
}

impl Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.chunk).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

/// Up to 16 million samples, then how decoding ended
fn ours(result: Result<usize, flac_codec::Error>) -> String {
    match result {
        Ok(_) => "end".to_string(),
        Err(flac_codec::Error::Io(err)) => format!("Io({:?})", err.kind()),
        Err(err) => format!("{err:?}"),
    }
}

fn reference(result: Result<usize, flac_codec_reference::Error>) -> String {
    match result {
        Ok(_) => "end".to_string(),
        Err(flac_codec_reference::Error::Io(err)) => format!("Io({:?})", err.kind()),
        Err(err) => format!("{err:?}"),
    }
}

/// Reads samples in pieces of 4096 until the end or an error
macro_rules! read_all {
    ($reader:expr, $samples:expr) => {{
        let mut buf = vec![0; 4096];
        loop {
            match $reader.read(&mut buf) {
                Ok(0) => break Ok(0),
                Ok(n) if $samples.len() < 16 << 20 => $samples.extend_from_slice(&buf[..n]),
                Ok(n) => break Ok(n),
                Err(err) => break Err(err),
            }
        }
    }};
}

fuzz_target!(|input: (u8, u64, &[u8])| {
    let (mode, position, data) = input;
    let mut expected = vec![];
    let mut decoded = vec![];

    if mode & 1 == 0 {
        // read from the start, ours in pieces of 1 to 128 bytes or 64 KiB
        let chunk = match mode >> 1 {
            0 => 1 << 16,
            n => usize::from(n),
        };
        let theirs = match flac_codec_reference::decode::FlacSampleReader::new(Cursor::new(data)) {
            Ok(mut r) => reference(read_all!(r, expected)),
            Err(err) => reference(Err(err)),
        };
        let mine = match flac_codec::decode::FlacSampleReader::new(Chunked { data, chunk }) {
            Ok(mut r) => ours(read_all!(r, decoded)),
            Err(err) => ours(Err(err)),
        };
        assert_eq!(mine, theirs);
    } else {
        // seek, then read on
        let theirs = match flac_codec_reference::decode::FlacSampleReader::new(Cursor::new(data)) {
            Ok(mut r) => match r.seek(position) {
                Ok(()) => reference(read_all!(r, expected)),
                Err(err) => reference(Err(err)),
            },
            Err(err) => reference(Err(err)),
        };
        let mine = match flac_codec::decode::FlacSampleReader::new(Cursor::new(data)) {
            Ok(mut r) => match r.seek(position) {
                Ok(()) => ours(read_all!(r, decoded)),
                Err(err) => ours(Err(err)),
            },
            Err(err) => ours(Err(err)),
        };
        assert_eq!(mine, theirs);
    }
    assert_eq!(decoded, expected);
});
