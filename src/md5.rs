// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The MD5 sum (RFC 1321) of a stream's samples, which STREAMINFO stores
//!
//! FLAC uses MD5 to notice damage. It does not resist deliberate changes,
//! and this module is meant for nothing else.
//!
//! Hashing is a large part of encoding and of verifying, and MD5 cannot be
//! computed side by side: each of a block's 64 steps needs the result of
//! the step before. What decides its speed is therefore how many operations
//! lie between one step's result and the next. The steps below are written
//! so that only what has to wait for the previous result does so; see
//! [`compress`].

/// The bytes of a block
const BLOCK: usize = 64;

/// The constant of each step: `floor(2^32 * abs(sin(i)))` for `i` from 1 to 64,
/// four to a line as the RFC prints them
#[rustfmt::skip]
static CONSTANTS: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee,
    0xf57c_0faf, 0x4787_c62a, 0xa830_4613, 0xfd46_9501,
    0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be,
    0x6b90_1122, 0xfd98_7193, 0xa679_438e, 0x49b4_0821,
    0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa,
    0xd62f_105d, 0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8,
    0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a,
    0xfffa_3942, 0x8771_f681, 0x6d9d_6122, 0xfde5_380c,
    0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70,
    0x289b_7ec6, 0xeaa1_27fa, 0xd4ef_3085, 0x0488_1d05,
    0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665,
    0xf429_2244, 0x432a_ff97, 0xab94_23a7, 0xfc93_a039,
    0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1,
    0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb, 0xeb86_d391,
];

/// How far each step rotates its sum to the left: one row for each round,
/// used in turn
const ROTATIONS: [[u32; 4]; 4] = [
    [7, 12, 17, 22],
    [5, 9, 14, 20],
    [4, 11, 16, 23],
    [6, 10, 15, 21],
];

/// An MD5 sum in progress
#[derive(Clone)]
pub(crate) struct Md5 {
    // the words A, B, C and D
    state: [u32; 4],
    // the bytes after the last whole block
    pending: [u8; BLOCK],
    // how many bytes were given so far; `pending` holds the last `len % BLOCK`
    len: u64,
}

impl Default for Md5 {
    #[inline]
    fn default() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            pending: [0; BLOCK],
            len: 0,
        }
    }
}

impl Md5 {
    /// Adds `bytes` to what is summed
    pub(crate) fn update(&mut self, mut bytes: &[u8]) {
        let pending = (self.len % BLOCK as u64) as usize;
        // (the sum's length field is the count of bits modulo 2^64)
        self.len = self.len.wrapping_add(bytes.len() as u64);

        if pending > 0 {
            // complete the block that was begun
            let (head, rest) = bytes.split_at(bytes.len().min(BLOCK - pending));
            self.pending[pending..pending + head.len()].copy_from_slice(head);
            if pending + head.len() < BLOCK {
                return;
            }
            compress(&mut self.state, &self.pending);
            bytes = rest;
        }

        let (blocks, rest) = bytes.as_chunks::<BLOCK>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        self.pending[..rest.len()].copy_from_slice(rest);
    }

    /// The sum of all the bytes given
    pub(crate) fn finalize(mut self) -> [u8; 16] {
        // a 1 bit, then zeros up to 8 bytes before the end of a block,
        // then the number of bits summed
        const LENGTH: usize = 8;
        let bits = self.len.wrapping_mul(8);
        let pending = (self.len % BLOCK as u64) as usize;
        let padding = match pending < BLOCK - LENGTH {
            true => BLOCK - LENGTH - pending,
            false => 2 * BLOCK - LENGTH - pending,
        };
        let mut pad = [0; BLOCK];
        pad[0] = 0x80;
        self.update(&pad[..padding]);
        self.update(&bits.to_le_bytes());
        debug_assert!(self.len.is_multiple_of(BLOCK as u64));

        let mut sum = [0; 16];
        for (bytes, word) in sum.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
            *bytes = word.to_le_bytes();
        }
        sum
    }
}

/// So that `std::io::copy` can sum what a reader gives
impl std::io::Write for Md5 {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.update(bytes);
        Ok(bytes.len())
    }

    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Mixes one block into `state`
///
/// A step replaces the oldest of the four words: the word, a message word,
/// a constant and a mix of the other three words are added up, rotated and
/// added to the newest word, `b`. The mix needs `b`, the result of the
/// previous step, so the next step can only begin there. Each step
/// therefore adds up the old word, the message word and the constant first
/// (`early`), and keeps to a form of the mix in which `b` passes through as
/// few operations as possible:
///
/// - round 1: `d ^ (b & (c ^ d))` for "`c` where `b` is set, else `d`",
///   two operations after `b`;
/// - round 2: "`b` where `d` is set, else `c`" as two parts that share no
///   bit and can be added one after the other, so that only `b & d`, one
///   operation, comes after `b`;
/// - round 3: `b ^ (c ^ d)`, one operation after `b`;
/// - round 4: `c ^ (b | !d)`, two operations after `b`.
fn compress(state: &mut [u32; 4], block: &[u8; BLOCK]) {
    // A constant the compiler knows is added last, after the mix, which
    // makes every step one addition longer. Read through a reference it
    // cannot see through, the constants are added first, as written.
    let constants = std::hint::black_box(&CONSTANTS);

    let mut words = [0; 16];
    for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }

    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..16 {
        let early = a.wrapping_add(words[i]).wrapping_add(constants[i]);
        let sum = early.wrapping_add(d ^ (b & (c ^ d)));
        (a, b, c, d) = (
            d,
            b.wrapping_add(sum.rotate_left(ROTATIONS[0][i % 4])),
            b,
            c,
        );
    }
    for i in 0..16 {
        let early = a
            .wrapping_add(words[(5 * i + 1) % 16])
            .wrapping_add(constants[16 + i])
            .wrapping_add(c & !d);
        let sum = early.wrapping_add(b & d);
        (a, b, c, d) = (
            d,
            b.wrapping_add(sum.rotate_left(ROTATIONS[1][i % 4])),
            b,
            c,
        );
    }
    for i in 0..16 {
        let early = a
            .wrapping_add(words[(3 * i + 5) % 16])
            .wrapping_add(constants[32 + i]);
        let sum = early.wrapping_add(b ^ (c ^ d));
        (a, b, c, d) = (
            d,
            b.wrapping_add(sum.rotate_left(ROTATIONS[2][i % 4])),
            b,
            c,
        );
    }
    for i in 0..16 {
        let early = a
            .wrapping_add(words[(7 * i) % 16])
            .wrapping_add(constants[48 + i]);
        let sum = early.wrapping_add(c ^ (b | !d));
        (a, b, c, d) = (
            d,
            b.wrapping_add(sum.rotate_left(ROTATIONS[3][i % 4])),
            b,
            c,
        );
    }

    for (word, mixed) in state.iter_mut().zip([a, b, c, d]) {
        *word = word.wrapping_add(mixed);
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK, Md5};

    fn sum(bytes: &[u8]) -> [u8; 16] {
        let mut md5 = Md5::default();
        md5.update(bytes);
        md5.finalize()
    }

    fn hex(sum: [u8; 16]) -> String {
        sum.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_test_suite_of_rfc_1321() {
        for (text, expected) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "1234567890123456789012345678901234567890\
                 1234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            assert_eq!(hex(sum(text.as_bytes())), expected, "{text:?}");
        }
    }

    #[test]
    fn every_length_around_the_block_and_padding_limits() {
        // lengths from nothing to past three blocks: every position of the
        // padding, which moves to a block of its own from 56 bytes on
        let bytes: Vec<u8> = (0..3 * BLOCK + 9).map(|i| (i * 131 + 7) as u8).collect();
        for len in 0..=bytes.len() {
            assert_eq!(
                sum(&bytes[..len]),
                ::md5::compute(&bytes[..len]).0,
                "{len} bytes"
            );
        }
    }

    #[test]
    fn any_split_of_the_input_gives_the_same_sum() {
        let mut seed = 0xC2B2_AE3D_27D4_EB4Fu64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..2000 {
            let len = match round % 4 {
                0 => (next() % 200) as usize,
                1 => 3072 * (1 + (next() % 4) as usize),
                _ => (next() % 20_000) as usize,
            };
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();

            // pieces from empty to several blocks, the block length included
            let mut md5 = Md5::default();
            let mut rest = bytes.as_slice();
            while !rest.is_empty() {
                let piece = match next() % 5 {
                    0 => 0,
                    1 => BLOCK,
                    2 => 1 + (next() % 3) as usize,
                    _ => (next() % 300) as usize,
                };
                let (head, tail) = rest.split_at(piece.min(rest.len()));
                md5.update(head);
                rest = tail;
            }
            // a sum in progress can be copied, and both copies go on alike
            let copy = md5.clone();

            let expected = ::md5::compute(&bytes).0;
            assert_eq!(md5.finalize(), expected, "round {round}, {len} bytes");
            assert_eq!(copy.finalize(), expected, "round {round}, {len} bytes");
        }
    }

    #[test]
    fn copying_from_a_reader_sums_its_bytes() {
        let bytes: Vec<u8> = (0..100_000u32).map(|i| ((i * 31) >> 3) as u8).collect();
        let mut md5 = Md5::default();
        std::io::copy(&mut bytes.as_slice(), &mut md5).unwrap();
        assert_eq!(md5.finalize(), ::md5::compute(&bytes).0);
    }
}
