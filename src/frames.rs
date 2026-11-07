// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Decoding frames from memory
//!
//! The decoder reads its stream ahead into a [`ReadAhead`] buffer and
//! decodes each frame from a byte slice with [`SliceBitReader`], which
//! takes up to 64 bits at a time instead of one byte per call.
//! The frame's CRC-16 is then computed over exactly the bytes the frame
//! used. A frame that runs past the buffered bytes is decoded again once
//! more are read, so the result never depends on where the buffer ends.

use bitstream_io::{BitCount, BitRead, BitReader, Endianness, Primitive};
use bitstream_io::{SignedBitCount, SignedInteger, UnsignedInteger};
use std::io;

/// Bytes read ahead from a stream but not yet decoded
#[derive(Clone, Default)]
pub(crate) struct ReadAhead {
    data: Vec<u8>,
    // the first byte of `data` not yet consumed
    start: usize,
    // whether the stream has ended
    eof: bool,
}

impl ReadAhead {
    /// How much one refill asks the stream for
    const CHUNK: usize = 256 * 1024;

    /// How often one frame may refill the buffer before it is read from
    /// the stream instead: twice covers a `BufReader` handing out the rest
    /// of its own buffer first
    pub(crate) const REFILLS: usize = 2;

    /// The most bytes a frame decodes from memory;
    /// a larger frame is read from the stream instead.
    ///
    /// The largest frame a FLAC encoder writes in practice,
    /// 65,535 verbatim samples of 8 channels with a 33-bit side channel,
    /// takes about 2.2 MB.
    pub(crate) const LIMIT: usize = 4 * 1024 * 1024;

    /// The bytes read ahead and not yet consumed
    #[inline]
    pub(crate) fn pending(&self) -> &[u8] {
        &self.data[self.start..]
    }

    /// Whether the stream has ended, so `pending` holds all that is left
    #[inline]
    pub(crate) fn at_eof(&self) -> bool {
        self.eof
    }

    /// Marks the first `bytes` pending bytes as consumed
    #[inline]
    pub(crate) fn consume(&mut self, bytes: usize) {
        debug_assert!(bytes <= self.pending().len());
        self.start += bytes.min(self.pending().len());
    }

    /// Forgets everything read ahead, as after seeking the stream
    pub(crate) fn clear(&mut self) {
        self.data.clear();
        self.start = 0;
        self.eof = false;
    }

    /// Takes the pending bytes out of the buffer, leaving it empty
    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        self.data.drain(..self.start);
        self.start = 0;
        std::mem::take(&mut self.data)
    }

    /// Puts bytes back in front of the buffer
    pub(crate) fn restore(&mut self, bytes: &[u8]) {
        self.data.drain(..self.start);
        self.start = 0;
        self.data.splice(0..0, bytes.iter().copied());
    }

    /// Reads more of the stream with a single read call,
    /// so a live stream is never waited on for more than it has
    pub(crate) fn fill<R: io::Read>(&mut self, reader: &mut R) -> io::Result<()> {
        self.data.drain(..self.start);
        self.start = 0;
        let len = self.data.len();
        self.data.resize(len + Self::CHUNK, 0);
        let read = loop {
            match reader.read(&mut self.data[len..]) {
                Ok(read) => break read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    self.data.truncate(len);
                    return Err(err);
                }
            }
        };
        self.data.truncate(len + read);
        if read == 0 {
            self.eof = true;
        }
        Ok(())
    }
}

/// A big-endian bit reader over a byte slice
///
/// It behaves like bitstream-io's `BitReader` on the same bytes:
/// the same values, the same errors, and an end of data reported as
/// `UnexpectedEof`. Reading past the end also sets [`Self::ran_out`],
/// which tells a short slice apart from a short stream.
pub(crate) struct SliceBitReader<'s> {
    data: &'s [u8],
    // the next byte of `data` to load into the cache
    pos: usize,
    // the unread bits, the next one in the most significant bit;
    // the bits below the top `bits` are zero
    cache: u64,
    bits: u32,
    ran_out: bool,
}

impl<'s> SliceBitReader<'s> {
    /// Reads from the start of `data`
    #[inline]
    pub(crate) fn new(data: &'s [u8]) -> Self {
        Self {
            data,
            pos: 0,
            cache: 0,
            bits: 0,
            ran_out: false,
        }
    }

    /// The bytes read so far, a partly read byte included
    #[inline]
    pub(crate) fn consumed(&self) -> usize {
        self.pos - (self.bits / 8) as usize
    }

    /// Whether a read wanted more than the slice holds
    #[inline]
    pub(crate) fn ran_out(&self) -> bool {
        self.ran_out
    }

    /// Loads whole bytes until the cache holds more than 56 bits
    /// or the slice is exhausted
    #[inline]
    fn refill(&mut self) {
        let room = (64 - self.bits) / 8;
        if room == 0 {
            return;
        }
        match self
            .data
            .get(self.pos..)
            .and_then(|rest| rest.first_chunk::<8>())
        {
            Some(chunk) => {
                let word = u64::from_be_bytes(*chunk);
                let word = match room {
                    8 => word,
                    room => word & !(u64::MAX >> (room * 8)),
                };
                // room > 0, so fewer than 64 bits are cached
                self.cache |= word >> self.bits;
                self.bits += room * 8;
                self.pos += room as usize;
            }
            None => {
                while self.bits <= 56 {
                    match self.data.get(self.pos) {
                        Some(byte) => {
                            self.cache |= u64::from(*byte) << (56 - self.bits);
                            self.bits += 8;
                            self.pos += 1;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    /// Reading past the end: like a stream, everything left is consumed
    #[cold]
    fn exhausted(&mut self) -> io::Error {
        self.ran_out = true;
        self.pos = self.data.len();
        self.cache = 0;
        self.bits = 0;
        io::Error::from(io::ErrorKind::UnexpectedEof)
    }

    /// Takes the next `n` bits, from 1 to 56
    #[inline]
    fn take(&mut self, n: u32) -> io::Result<u64> {
        debug_assert!((1..=56).contains(&n));
        if self.bits < n {
            self.refill();
            if self.bits < n {
                return Err(self.exhausted());
            }
        }
        let value = self.cache >> (64 - n);
        self.cache <<= n;
        self.bits -= n;
        Ok(value)
    }

    /// Takes the next `n` bits, from 0 to 64
    #[inline]
    fn take_wide(&mut self, n: u32) -> io::Result<u64> {
        match n {
            0 => Ok(0),
            1..=56 => self.take(n),
            _ => {
                let high = self.take(n - 32)?;
                let low = self.take(32)?;
                Ok((high << 32) | low)
            }
        }
    }

    /// Counts the bits before the next `stop` bit and consumes them and it
    #[inline]
    fn unary(&mut self, stop: bool) -> io::Result<u32> {
        let mut count = 0u32;
        loop {
            if self.bits == 0 {
                self.refill();
                if self.bits == 0 {
                    return Err(self.exhausted());
                }
            }
            let run = if stop {
                self.cache.leading_zeros()
            } else {
                self.cache.leading_ones()
            }
            .min(self.bits);
            if run < self.bits {
                // the stop bit is cached too
                count += run;
                self.cache = self.cache.checked_shl(run + 1).unwrap_or(0);
                self.bits -= run + 1;
                return Ok(count);
            }
            count += run;
            self.cache = 0;
            self.bits = 0;
        }
    }
}

impl SliceBitReader<'_> {
    /// The next Rice-coded value with parameter `k` (at most 30), before
    /// zigzag decoding: the same as a unary count and `k` more bits
    #[inline]
    pub(crate) fn rice(&mut self, k: u32) -> io::Result<u32> {
        if self.bits < 32 {
            self.refill();
        }
        // the usual case: the whole code is cached
        let zeros = self.cache.leading_zeros();
        if zeros < self.bits && zeros + 1 + k <= self.bits {
            let rest = (self.cache << zeros) << 1;
            let lsb = match k {
                0 => 0,
                k => (rest >> (64 - k)) as u32,
            };
            self.cache = rest.checked_shl(k).unwrap_or(0);
            self.bits -= zeros + 1 + k;
            return Ok((zeros << k) | lsb);
        }
        let msb = self.unary(true)?;
        let lsb = match k {
            0 => 0,
            k => self.take(k)? as u32,
        };
        Ok((msb << k) | lsb)
    }
}

/// An unsigned value of up to 64 bits in a type at least that wide
#[inline]
fn unsigned_from_u64<U: UnsignedInteger>(value: u64) -> U {
    let mut buf = U::buffer();
    let bytes = buf.as_mut();
    let be = value.to_be_bytes();
    match bytes.len() {
        len @ 8.. => bytes[len - 8..].copy_from_slice(&be),
        len => bytes.copy_from_slice(&be[8 - len..]),
    }
    U::from_be_bytes(buf)
}

fn excessive_bits() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "excessive bits for type read")
}

impl BitRead for SliceBitReader<'_> {
    #[inline]
    fn read_bit(&mut self) -> io::Result<bool> {
        self.take(1).map(|bit| bit == 1)
    }

    #[inline]
    fn read_unsigned_counted<const MAX: u32, U>(&mut self, bits: BitCount<MAX>) -> io::Result<U>
    where
        U: UnsignedInteger,
    {
        let bits = u32::from(bits);
        if !(MAX <= U::BITS_SIZE || bits <= U::BITS_SIZE) {
            return Err(excessive_bits());
        }
        match bits {
            ..=64 => self.take_wide(bits).map(unsigned_from_u64),
            _ => {
                let high: U = unsigned_from_u64(self.take_wide(bits - 64)?);
                let low: U = unsigned_from_u64(self.take_wide(64)?);
                Ok((high << 64) | low)
            }
        }
    }

    #[inline]
    fn read_signed_counted<const MAX: u32, S>(
        &mut self,
        bits: impl TryInto<SignedBitCount<MAX>>,
    ) -> io::Result<S>
    where
        S: SignedInteger,
    {
        let bits: SignedBitCount<MAX> = bits.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "signed reads need at least 1 bit for sign",
            )
        })?;
        let total = u32::from(bits.count());
        if !(MAX <= S::BITS_SIZE || total <= S::BITS_SIZE) {
            return Err(excessive_bits());
        }
        let is_negative = self.read_bit()?;
        let unsigned: S::Unsigned = self.read_unsigned_counted(bits.unsigned_count())?;
        Ok(if is_negative {
            unsigned.as_negative(total)
        } else {
            unsigned.as_non_negative()
        })
    }

    fn read_to<V>(&mut self) -> io::Result<V>
    where
        V: Primitive,
    {
        let mut buf = V::buffer();
        self.read_bytes(buf.as_mut())?;
        Ok(V::from_be_bytes(buf))
    }

    /// Takes the bytes as the big-endian bit stream gives them and orders
    /// them by `F`, which is what bitstream-io does when byte-aligned
    /// (frames do not use this read)
    fn read_as_to<F, V>(&mut self) -> io::Result<V>
    where
        F: Endianness,
        V: Primitive,
    {
        let mut buf = V::buffer();
        self.read_bytes(buf.as_mut())?;
        BitReader::<_, F>::new(buf.as_ref()).read_to::<V>()
    }

    fn skip(&mut self, mut bits: u32) -> io::Result<()> {
        while bits > 0 {
            let n = bits.min(56);
            self.take(n)?;
            bits -= n;
        }
        Ok(())
    }

    fn read_bytes(&mut self, buf: &mut [u8]) -> io::Result<()> {
        buf.iter_mut().try_for_each(|byte| {
            *byte = self.take(8)? as u8;
            Ok(())
        })
    }

    #[inline]
    fn read_unary<const STOP_BIT: u8>(&mut self) -> io::Result<u32> {
        const {
            assert!(matches!(STOP_BIT, 0 | 1), "stop bit must be 0 or 1");
        }
        self.unary(STOP_BIT == 1)
    }

    #[inline]
    fn byte_aligned(&self) -> bool {
        self.bits.is_multiple_of(8)
    }

    #[inline]
    fn byte_align(&mut self) {
        let partial = self.bits % 8;
        self.cache = self.cache.checked_shl(partial).unwrap_or(0);
        self.bits -= partial;
    }
}

/// Reads whole bytes, for parsers that take a byte stream (frame headers)
impl io::Read for SliceBitReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.data.len() - self.consumed();
        let n = buf.len().min(available);
        if n < buf.len() {
            self.ran_out = true;
        }
        for byte in &mut buf[..n] {
            *byte = self.take(8)? as u8;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod test {
    use super::SliceBitReader;
    use bitstream_io::{BigEndian, BitCount, BitRead, BitReader, LittleEndian};
    use std::cell::Cell;
    use std::rc::Rc;

    /// Deterministic xorshift for the comparison below
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A byte stream that counts the bytes taken from it
    struct Counted<'d> {
        data: &'d [u8],
        taken: Rc<Cell<usize>>,
    }

    impl std::io::Read for Counted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = std::io::Read::read(&mut self.data, buf)?;
            self.taken.set(self.taken.get() + n);
            Ok(n)
        }
    }

    /// The same random operations on bitstream-io's reader over a stream and on
    /// ours over the same bytes (often runs of zeros or ones, for unary codes):
    /// every value, error kind, alignment and byte position must match.
    #[test]
    fn behaves_like_bitstream_io() {
        let mut rng = Rng(0x5EED_B175);
        for round in 0..20000 {
            let len = rng.below(64) as usize;
            let data: Vec<u8> = (0..len)
                .map(|_| match rng.below(4) {
                    0 => 0x00,
                    1 => 0xFF,
                    _ => rng.next() as u8,
                })
                .collect();
            let taken = Rc::new(Cell::new(0));
            let mut theirs = BitReader::endian(
                Counted {
                    data: &data,
                    taken: taken.clone(),
                },
                BigEndian,
            );
            let mut ours = SliceBitReader::new(&data);

            for step in 0..40 {
                let op = rng.below(16);
                let n = rng.below(70) as u32;
                let context =
                    format!("round {round}, step {step}, op {op}, n {n}, data {data:02x?}");
                macro_rules! same {
                    ($r:ident => $e:expr) => {{
                        let a = {
                            let $r = &mut theirs;
                            $e
                        };
                        let b = {
                            let $r = &mut ours;
                            $e
                        };
                        match (&a, &b) {
                            (Ok(x), Ok(y)) => assert_eq!(x, y, "{context}"),
                            (Err(x), Err(y)) => assert_eq!(x.kind(), y.kind(), "{context}"),
                            _ => panic!("{context}: {a:?} against {b:?}"),
                        }
                        a.is_ok()
                    }};
                }
                let count64 = BitCount::<64>::try_from(n.min(64)).unwrap();
                let count32 = BitCount::<32>::try_from(n.min(32)).unwrap();
                let signed32 = BitCount::<32>::try_from(1 + n % 32)
                    .unwrap()
                    .signed_count()
                    .unwrap();
                let signed64 = BitCount::<64>::try_from(1 + n % 64)
                    .unwrap()
                    .signed_count()
                    .unwrap();
                let ok = match op {
                    0 => same!(r => r.read_bit()),
                    1 => same!(r => r.read_unary::<1>()),
                    2 => same!(r => r.read_unary::<0>()),
                    3 => same!(r => r.read_unsigned_counted::<64, u64>(count64)),
                    4 => same!(r => r.read_unsigned_counted::<32, u32>(count32)),
                    // more bits than the type holds
                    5 => same!(r => r.read_unsigned_counted::<64, u8>(count64)),
                    6 => same!(r => r.read_counted::<32, u16>(count32)),
                    7 => same!(r => r.read_signed_counted::<32, i32>(signed32)),
                    8 => same!(r => r.read_signed_counted::<64, i64>(signed64)),
                    9 => same!(r => r.read_signed_counted::<64, i16>(signed64)),
                    10 => same!(r => r.skip(n)),
                    11 => {
                        theirs.byte_align();
                        ours.byte_align();
                        true
                    }
                    12 => same!(r => r.read_to::<u32>()),
                    // a Rice code: a unary count and then `k` bits
                    15 => {
                        let k = n % 31;
                        let a = theirs.read_unary::<1>().and_then(|msb| {
                            theirs
                                .read_unsigned_counted::<32, u32>(BitCount::try_from(k).unwrap())
                                .map(|lsb| (msb << k) | lsb)
                        });
                        let b = ours.rice(k);
                        match (&a, &b) {
                            (Ok(x), Ok(y)) => assert_eq!(x, y, "{context}"),
                            (Err(x), Err(y)) => assert_eq!(x.kind(), y.kind(), "{context}"),
                            _ => panic!("{context}: {a:?} against {b:?}"),
                        }
                        a.is_ok()
                    }
                    // bitstream-io mixes bit orders for this read when it is not
                    // byte-aligned; frames never use it, so compare it aligned
                    13 => {
                        theirs.byte_align();
                        ours.byte_align();
                        same!(r => r.read_as_to::<LittleEndian, u16>())
                    }
                    _ => same!(r => {
                        let mut buf = [0u8; 3];
                        r.read_bytes(&mut buf).map(|()| buf)
                    }),
                };
                // after an error, only the bytes taken still matter
                assert_eq!(ours.consumed(), taken.get(), "{context}: position");
                if !ok {
                    break;
                }
                assert_eq!(
                    theirs.byte_aligned(),
                    ours.byte_aligned(),
                    "{context}: alignment"
                );
            }
        }
    }

    /// Whole bytes through `io::Read`, as frame headers are read, and the
    /// end of the slice reported as `ran_out`
    #[test]
    fn reads_bytes_and_reports_running_out() {
        use std::io::Read;

        let data = [1u8, 2, 3, 4, 5];
        let mut ours = SliceBitReader::new(&data);
        let mut buf = [0u8; 3];
        ours.read_exact(&mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(ours.consumed(), 3);
        assert!(!ours.ran_out());
        assert_eq!(
            ours.read_exact(&mut buf).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        assert!(ours.ran_out());
        assert_eq!(ours.consumed(), data.len());
    }
}
