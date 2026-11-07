// Any input: every metadata block that reads must write back and read again unchanged.
#![no_main]

use flac_codec::metadata::{Block, read_blocks, write_blocks};
use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let Ok(blocks) = read_blocks(Cursor::new(data)).collect::<Result<Vec<Block>, _>>() else {
        return;
    };
    let mut written = vec![];
    if write_blocks(&mut written, blocks.iter()).is_ok() {
        let again = read_blocks(Cursor::new(&written))
            .collect::<Result<Vec<Block>, _>>()
            .expect("written blocks read back");
        assert_eq!(again, blocks);
    }
});
