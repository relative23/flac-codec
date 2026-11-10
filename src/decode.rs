// Copyright 2025 Brian Langenberger
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! For decoding FLAC files to PCM samples

use crate::Error;
use crate::audio::Frame;
use crate::exact;
use crate::md5::Md5;
use crate::metadata::{BlockList, ChannelMask, SeekTable};
use bitstream_io::{BitRead, SignedBitCount};
use std::collections::VecDeque;
use std::fs::File;
use std::io::BufReader;
use std::num::NonZero;
use std::path::Path;

pub use crate::metadata::Metadata;

trait SignedInteger:
    bitstream_io::SignedInteger + Into<i64> + std::ops::AddAssign + std::ops::Neg<Output = Self>
{
    fn from_i64(i: i64) -> Self;

    fn from_u32(u: u32) -> Self;
}

impl SignedInteger for i32 {
    #[inline(always)]
    fn from_i64(i: i64) -> i32 {
        i as i32
    }

    #[inline(always)]
    fn from_u32(u: u32) -> i32 {
        u as i32
    }
}

impl SignedInteger for i64 {
    #[inline(always)]
    fn from_i64(i: i64) -> i64 {
        i
    }

    #[inline(always)]
    fn from_u32(u: u32) -> i64 {
        u as i64
    }
}

/// A FLAC reader which outputs PCM samples as bytes
///
/// # Example
///
/// ```
/// use flac_codec::{
///     byteorder::LittleEndian,
///     encode::{FlacByteWriter, Options},
///     decode::{FlacByteReader, Metadata},
/// };
/// use std::io::{Cursor, Read, Seek, Write};
///
/// let mut flac = Cursor::new(vec![]);  // a FLAC file in memory
///
/// let mut writer = FlacByteWriter::endian(
///     &mut flac,           // our wrapped writer
///     LittleEndian,        // .wav-style byte order
///     Options::default(),  // default encoding options
///     44100,               // sample rate
///     16,                  // bits-per-sample
///     1,                   // channel count
///     Some(2000),          // total bytes
/// ).unwrap();
///
/// // write 1000 samples as 16-bit, signed, little-endian bytes (2000 bytes total)
/// let written_bytes = (0..1000).map(i16::to_le_bytes).flatten().collect::<Vec<u8>>();
/// assert!(writer.write_all(&written_bytes).is_ok());
///
/// // finalize writing file
/// assert!(writer.finalize().is_ok());
///
/// flac.rewind().unwrap();
///
/// // open reader around written FLAC file
/// let mut reader = FlacByteReader::endian(flac, LittleEndian).unwrap();
///
/// // read 2000 bytes
/// let mut read_bytes = vec![];
/// assert!(reader.read_to_end(&mut read_bytes).is_ok());
///
/// // ensure MD5 sum of signed, little-endian samples matches hash in file
/// let mut md5 = md5::Context::new();
/// md5.consume(&read_bytes);
/// assert_eq!(&md5.compute().0, reader.md5().unwrap());
///
/// // ensure input and output matches
/// assert_eq!(read_bytes, written_bytes);
/// ```
#[derive(Clone)]
pub struct FlacByteReader<R, E> {
    // the wrapped decoder
    decoder: Decoder<R>,
    // decoded byte buffer
    buf: VecDeque<u8>,
    // the endianness of the bytes in our byte buffer
    endianness: std::marker::PhantomData<E>,
    // offset start of frames, if known
    frames_start: Option<u64>,
}

impl<R: std::io::Read, E: crate::byteorder::Endianness> FlacByteReader<R, E> {
    /// Opens new FLAC reader which wraps the given reader
    ///
    /// The reader must be positioned at the start of the
    /// FLAC stream.  If the file has non-FLAC data
    /// at the beginning (such as ID3v2 tags), one
    /// should skip such data before initializing a `FlacByteReader`.
    #[inline]
    pub fn new(mut reader: R) -> Result<Self, Error> {
        let blocklist = BlockList::read(reader.by_ref())?;

        Ok(Self {
            decoder: Decoder::new(reader, blocklist),
            buf: VecDeque::default(),
            endianness: std::marker::PhantomData,
            frames_start: None,
        })
    }

    /// Opens new FLAC reader in the given endianness
    ///
    /// The reader must be positioned at the start of the
    /// FLAC stream.  If the file has non-FLAC data
    /// at the beginning (such as ID3v2 tags), one
    /// should skip such data before initializing a `FlacByteReader`.
    #[inline]
    pub fn endian(reader: R, _endian: E) -> Result<Self, Error> {
        Self::new(reader)
    }

    /// Returns FLAC metadata blocks
    #[inline]
    pub fn metadata(&self) -> &BlockList {
        self.decoder.metadata()
    }
}

impl<R: std::io::Read + std::io::Seek, E: crate::byteorder::Endianness> FlacByteReader<R, E> {
    /// Opens a new seekable FLAC reader which wraps the given reader
    ///
    /// If a stream is both readable and seekable,
    /// it's vital to use this method to open it if one
    /// also wishes to seek within the FLAC stream.
    /// Otherwise, an I/O error will result when attempting to seek.
    ///
    /// [`FlacByteReader::open`] calls this method to ensure
    /// all `File`-based streams are also seekable.
    ///
    /// The reader must be positioned at the start of the
    /// FLAC stream.  If the file has non-FLAC data
    /// at the beginning (such as ID3v2 tags), one
    /// should skip such data before initializing a `FlacByteReader`.
    ///
    /// # Example
    ///
    /// ```
    /// use flac_codec::{
    ///     byteorder::LittleEndian,
    ///     encode::{FlacByteWriter, Options},
    ///     decode::FlacByteReader,
    /// };
    /// use std::io::{Cursor, Read, Seek, SeekFrom, Write};
    ///
    /// let mut flac = Cursor::new(vec![]);  // a FLAC file in memory
    ///
    /// let mut writer = FlacByteWriter::endian(
    ///     &mut flac,           // our wrapped writer
    ///     LittleEndian,        // .wav-style byte order
    ///     Options::default(),  // default encoding options
    ///     44100,               // sample rate
    ///     16,                  // bits-per-sample
    ///     1,                   // channel count
    ///     Some(2000),          // total bytes
    /// ).unwrap();
    ///
    /// // write 1000 samples as 16-bit, signed, little-endian bytes (2000 bytes total)
    /// let written_bytes = (0..1000).map(i16::to_le_bytes).flatten().collect::<Vec<u8>>();
    /// assert!(writer.write_all(&written_bytes).is_ok());
    ///
    /// // finalize writing file
    /// assert!(writer.finalize().is_ok());
    ///
    /// flac.rewind().unwrap();
    ///
    /// // open reader around written FLAC file
    /// let mut reader: FlacByteReader<_, LittleEndian> =
    ///     FlacByteReader::new_seekable(flac).unwrap();
    ///
    /// // read 2000 bytes
    /// let mut read_bytes_1 = vec![];
    /// assert!(reader.read_to_end(&mut read_bytes_1).is_ok());
    ///
    /// // ensure input and output matches
    /// assert_eq!(read_bytes_1, written_bytes);
    ///
    /// // rewind reader to halfway through file
    /// assert!(reader.seek(SeekFrom::Start(1000)).is_ok());
    ///
    /// // read 1000 bytes
    /// let mut read_bytes_2 = vec![];
    /// assert!(reader.read_to_end(&mut read_bytes_2).is_ok());
    ///
    /// // ensure output matches back half of input
    /// assert_eq!(read_bytes_2.len(), 1000);
    /// assert!(written_bytes.ends_with(&read_bytes_2));
    /// ```
    pub fn new_seekable(mut reader: R) -> Result<Self, Error> {
        let blocklist = BlockList::read(reader.by_ref())?;
        let frames_start = reader.stream_position()?;

        Ok(Self {
            decoder: Decoder::new(reader, blocklist),
            buf: VecDeque::default(),
            endianness: std::marker::PhantomData,
            frames_start: Some(frames_start),
        })
    }
}

impl<R: std::io::Read, E: crate::byteorder::Endianness> Metadata for FlacByteReader<R, E> {
    #[inline]
    fn channel_count(&self) -> u8 {
        self.decoder.channel_count().get()
    }

    #[inline]
    fn channel_mask(&self) -> ChannelMask {
        self.decoder.channel_mask()
    }

    #[inline]
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    #[inline]
    fn bits_per_sample(&self) -> u32 {
        self.decoder.bits_per_sample()
    }

    #[inline]
    fn total_samples(&self) -> Option<u64> {
        self.decoder.total_samples().map(|s| s.get())
    }

    #[inline]
    fn md5(&self) -> Option<&[u8; 16]> {
        self.decoder.md5()
    }
}

impl<E: crate::byteorder::Endianness> FlacByteReader<BufReader<File>, E> {
    /// Opens FLAC file from the given path
    #[inline]
    pub fn open<P: AsRef<Path>>(path: P, _endianness: E) -> Result<Self, Error> {
        FlacByteReader::new_seekable(BufReader::new(File::open(path.as_ref())?))
    }
}

impl<R: std::io::Read, E: crate::byteorder::Endianness> std::io::Read for FlacByteReader<R, E> {
    /// Reads samples to the given buffer as bytes in our stream's endianness
    ///
    /// Returned samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// This is the same format used by common PCM container
    /// formats like WAVE and AIFF
    ///
    /// # Errors
    ///
    /// Returns any error that occurs when reading the stream,
    /// converted to an I/O error.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.buf.is_empty() {
            match self.decoder.read_frame()? {
                Some(frame) => {
                    self.buf.resize(frame.bytes_len(), 0);
                    frame.to_buf::<E>(self.buf.make_contiguous());
                    self.buf.read(buf)
                }
                None => Ok(0),
            }
        } else {
            self.buf.read(buf)
        }
    }
}

impl<R: std::io::Read, E: crate::byteorder::Endianness> std::io::BufRead for FlacByteReader<R, E> {
    /// Reads samples to the given buffer as bytes in our stream's endianness
    ///
    /// Returned samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// # Errors
    ///
    /// Returns any error that occurs when reading the stream,
    /// converted to an I/O error.
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.buf.is_empty() {
            match self.decoder.read_frame()? {
                Some(frame) => {
                    self.buf.resize(frame.bytes_len(), 0);
                    frame.to_buf::<E>(self.buf.make_contiguous());
                    self.buf.fill_buf()
                }
                None => Ok(&[]),
            }
        } else {
            self.buf.fill_buf()
        }
    }

    fn consume(&mut self, amt: usize) {
        self.buf.consume(amt)
    }
}

/// A FLAC reader which outputs PCM samples as signed integers
///
/// # Example
///
/// ```
/// use flac_codec::{
///     encode::{FlacSampleWriter, Options},
///     decode::FlacSampleReader,
/// };
/// use std::io::{Cursor, Seek};
///
/// let mut flac = Cursor::new(vec![]);  // a FLAC file in memory
///
/// let mut writer = FlacSampleWriter::new(
///     &mut flac,           // our wrapped writer
///     Options::default(),  // default encoding options
///     44100,               // sample rate
///     16,                  // bits-per-sample
///     1,                   // channel count
///     Some(1000),          // total samples
/// ).unwrap();
///
/// // write 1000 samples
/// let written_samples = (0..1000).collect::<Vec<i32>>();
/// assert!(writer.write(&written_samples).is_ok());
///
/// // finalize writing file
/// assert!(writer.finalize().is_ok());
///
/// flac.rewind().unwrap();
///
/// // open reader around written FLAC file
/// let mut reader = FlacSampleReader::new(flac).unwrap();
///
/// // read 1000 samples
/// let mut read_samples = vec![0; 1000];
/// assert!(matches!(reader.read(&mut read_samples), Ok(1000)));
///
/// // ensure they match
/// assert_eq!(read_samples, written_samples);
/// ```
#[derive(Clone)]
pub struct FlacSampleReader<R> {
    // the wrapped decoder
    decoder: Decoder<R>,
    // decoded sample buffer
    buf: Vec<i32>,
    // start of FLAC frames
    frames_start: Option<u64>,
}

impl<R: std::io::Read> FlacSampleReader<R> {
    /// Opens new FLAC reader which wraps the given reader
    ///
    /// The reader must be positioned at the start of the
    /// FLAC stream.  If the file has non-FLAC data
    /// at the beginning (such as ID3v2 tags), one
    /// should skip such data before initializing a `FlacByteReader`.
    #[inline]
    pub fn new(mut reader: R) -> Result<Self, Error> {
        let blocklist = BlockList::read(reader.by_ref())?;

        Ok(Self {
            decoder: Decoder::new(reader, blocklist),
            buf: Vec::default(),
            frames_start: None,
        })
    }

    /// Returns FLAC metadata blocks
    #[inline]
    pub fn metadata(&self) -> &BlockList {
        self.decoder.metadata()
    }

    /// Attempts to fill the buffer with samples and returns quantity read
    ///
    /// Returned samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// # Errors
    ///
    /// Returns error if some error occurs reading FLAC file
    #[inline]
    pub fn read(&mut self, samples: &mut [i32]) -> Result<usize, Error> {
        if self.buf.is_empty() {
            match self.decoder.read_frame()? {
                Some(frame) => {
                    self.buf.extend(frame.iter());
                }
                None => return Ok(0),
            }
        }

        let to_consume = samples.len().min(self.buf.len());
        for (i, o) in samples.iter_mut().zip(self.buf.drain(0..to_consume)) {
            *i = o;
        }
        Ok(to_consume)
    }

    /// Reads all samples from source FLAC, placing them in `buf`
    ///
    /// If successful, returns the total number of samples read
    pub fn read_to_end(&mut self, buf: &mut Vec<i32>) -> Result<usize, Error> {
        let mut amt_read = 0;
        loop {
            match self.fill_buf()? {
                [] => break Ok(amt_read),
                decoded => {
                    let decoded_len = decoded.len();
                    buf.extend_from_slice(decoded);
                    amt_read += decoded_len;
                    self.consume(decoded_len);
                }
            }
        }
    }

    /// Returns complete buffer of all read samples
    ///
    /// Analogous to [`std::io::BufRead::fill_buf`], this should
    /// be paired with [`FlacSampleReader::consume`] to
    /// consume samples in the filled buffer once used.
    ///
    /// Returned samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// # Errors
    ///
    /// Returns error if some error occurs reading FLAC file
    /// to fill buffer.
    #[inline]
    pub fn fill_buf(&mut self) -> Result<&[i32], Error> {
        if self.buf.is_empty() {
            match self.decoder.read_frame()? {
                Some(frame) => {
                    self.buf.extend(frame.iter());
                }
                None => return Ok(&[]),
            }
        }

        Ok(self.buf.as_mut_slice())
    }

    /// Informs the reader that `amt` samples have been consumed.
    ///
    /// Analagous to [`std::io::BufRead::consume`], which marks
    /// samples as having been read.
    ///
    /// May panic if attempting to consume more bytes
    /// than are available in the buffer.
    #[inline]
    pub fn consume(&mut self, amt: usize) {
        self.buf.drain(0..amt);
    }
}

impl<R: std::io::Read + std::io::Seek> FlacSampleReader<R> {
    /// Opens a new seekable FLAC reader which wraps the given reader
    ///
    /// If a stream is both readable and seekable,
    /// it's vital to use this method to open it if one
    /// also wishes to seek within the FLAC stream.
    /// Otherwise, an I/O error will result when attempting to seek.
    ///
    /// [`FlacSampleReader::open`] calls this method to ensure
    /// all `File`-based streams are also seekable.
    ///
    /// The reader must be positioned at the start of the
    /// FLAC stream.  If the file has non-FLAC data
    /// at the beginning (such as ID3v2 tags), one
    /// should skip such data before initializing a `FlacSampleReader`.
    ///
    /// # Example
    ///
    /// ```
    /// use flac_codec::{
    ///     encode::{FlacSampleWriter, Options},
    ///     decode::FlacSampleReader,
    /// };
    /// use std::io::{Cursor, Seek};
    ///
    /// let mut flac = Cursor::new(vec![]);  // a FLAC file in memory
    ///
    /// let mut writer = FlacSampleWriter::new(
    ///     &mut flac,           // our wrapped writer
    ///     Options::default(),  // default encoding options
    ///     44100,               // sample rate
    ///     16,                  // bits-per-sample
    ///     1,                   // channel count
    ///     Some(1000),          // total samples
    /// ).unwrap();
    ///
    /// // write 1000 samples
    /// let written_samples = (0..1000).collect::<Vec<i32>>();
    /// assert!(writer.write(&written_samples).is_ok());
    ///
    /// // finalize writing file
    /// assert!(writer.finalize().is_ok());
    ///
    /// flac.rewind().unwrap();
    ///
    /// // open reader around written FLAC file
    /// let mut reader = FlacSampleReader::new_seekable(flac).unwrap();
    ///
    /// // read 1000 samples
    /// let mut read_samples_1 = vec![0; 1000];
    /// assert!(matches!(reader.read(&mut read_samples_1), Ok(1000)));
    ///
    /// // ensure they match
    /// assert_eq!(read_samples_1, written_samples);
    ///
    /// // rewind reader to halfway through file
    /// assert!(reader.seek(500).is_ok());
    ///
    /// // read 500 samples
    /// let mut read_samples_2 = vec![0; 500];
    /// assert!(matches!(reader.read(&mut read_samples_2), Ok(500)));
    ///
    /// // ensure output matches back half of input
    /// assert_eq!(read_samples_2.len(), 500);
    /// assert!(written_samples.ends_with(&read_samples_2));
    /// ```
    pub fn new_seekable(mut reader: R) -> Result<Self, Error> {
        let blocklist = BlockList::read(reader.by_ref())?;
        let frames_start = reader.stream_position()?;

        Ok(Self {
            decoder: Decoder::new(reader, blocklist),
            buf: Vec::default(),
            frames_start: Some(frames_start),
        })
    }
}

impl FlacSampleReader<BufReader<File>> {
    /// Opens FLAC file from the given path
    #[inline]
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        FlacSampleReader::new_seekable(BufReader::new(File::open(path.as_ref())?))
    }
}

impl<R: std::io::Read> Metadata for FlacSampleReader<R> {
    #[inline]
    fn channel_count(&self) -> u8 {
        self.decoder.channel_count().get()
    }

    #[inline]
    fn channel_mask(&self) -> ChannelMask {
        self.decoder.channel_mask()
    }

    #[inline]
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    #[inline]
    fn bits_per_sample(&self) -> u32 {
        self.decoder.bits_per_sample()
    }

    #[inline]
    fn total_samples(&self) -> Option<u64> {
        self.decoder.total_samples().map(|s| s.get())
    }

    #[inline]
    fn md5(&self) -> Option<&[u8; 16]> {
        self.decoder.md5()
    }
}

impl<R: std::io::Read> IntoIterator for FlacSampleReader<R> {
    type IntoIter = FlacSampleIterator<R>;
    type Item = Result<i32, Error>;

    fn into_iter(self) -> FlacSampleIterator<R> {
        FlacSampleIterator {
            decoder: self.decoder,
            buf: self.buf.into(),
        }
    }
}

/// A FLAC reader which iterates over decoded samples as signed integers
///
/// # Example
///
/// ```
/// use flac_codec::{
///     encode::{FlacSampleWriter, Options},
///     decode::FlacSampleReader,
/// };
/// use std::io::{Cursor, Seek};
///
/// let mut flac = Cursor::new(vec![]);  // a FLAC file in memory
///
/// let mut writer = FlacSampleWriter::new(
///     &mut flac,           // our wrapped writer
///     Options::default(),  // default encoding options
///     44100,               // sample rate
///     16,                  // bits-per-sample
///     1,                   // channel count
///     Some(10),            // total samples
/// ).unwrap();
///
/// // write 10 samples
/// let written_samples = (0..10).collect::<Vec<i32>>();
/// assert!(writer.write(&written_samples).is_ok());
///
/// // finalize writing file
/// assert!(writer.finalize().is_ok());
///
/// flac.rewind().unwrap();
///
/// // open reader around written FLAC file
/// let reader = FlacSampleReader::new(flac).unwrap();
///
/// // convert reader to iterator
/// let mut iter = reader.into_iter();
///
/// // read all its samples
/// let read_samples = iter.collect::<Result<Vec<_>, _>>().unwrap();
///
/// // ensure they match
/// assert_eq!(read_samples, written_samples);
/// assert_eq!(read_samples, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
/// ```
#[derive(Clone)]
pub struct FlacSampleIterator<R> {
    // the wrapped decoder
    decoder: Decoder<R>,
    // decoded sample buffer
    buf: VecDeque<i32>,
}

impl<R: std::io::Read> Metadata for FlacSampleIterator<R> {
    fn channel_count(&self) -> u8 {
        self.decoder.channel_count().get()
    }

    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn bits_per_sample(&self) -> u32 {
        self.decoder.bits_per_sample()
    }

    fn total_samples(&self) -> Option<u64> {
        self.decoder.total_samples().map(|s| s.get())
    }

    fn md5(&self) -> Option<&[u8; 16]> {
        self.decoder.md5()
    }
}

impl<R: std::io::Read> Iterator for FlacSampleIterator<R> {
    type Item = Result<i32, Error>;

    #[inline]
    fn next(&mut self) -> Option<Result<i32, Error>> {
        self.buf.pop_front().map_or_else(
            || match self.decoder.read_frame() {
                Ok(Some(frame)) => {
                    self.buf.extend(frame.iter());
                    self.buf.pop_front().map(Ok)
                }
                Err(e) => Some(Err(e)),
                Ok(None) => None,
            },
            |i| Some(Ok(i)),
        )
    }
}

impl<R: std::io::Read + std::io::Seek, E: crate::byteorder::Endianness> std::io::Seek
    for FlacByteReader<R, E>
{
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::cmp::Ordering;

        let Self { decoder, buf, .. } = self;

        let streaminfo = decoder.blocks.streaminfo();

        let bytes_per_pcm_frame: u64 = (u32::from(streaminfo.bits_per_sample).div_ceil(8)
            * u32::from(streaminfo.channels.get()))
        .into();

        // the desired absolute position in the stream, in bytes
        let desired_pos: u64 = match pos {
            std::io::SeekFrom::Start(pos) => pos,
            std::io::SeekFrom::Current(pos) => {
                // current position in bytes is current position in samples
                // converted to bytes *minus* the un-consumed space in the buffer
                // since the sample position is running ahead of the byte position
                let original_pos: u64 =
                    (decoder.current_sample * bytes_per_pcm_frame) - (buf.len() as u64);

                match pos.cmp(&0) {
                    Ordering::Less => {
                        original_pos
                            .checked_sub(pos.unsigned_abs())
                            .ok_or(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "cannot seek below byte 0",
                            ))?
                    }
                    Ordering::Equal => return Ok(original_pos),
                    Ordering::Greater => {
                        original_pos
                            .checked_add(pos.unsigned_abs())
                            .ok_or(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "seek offset too large",
                            ))?
                    }
                }
            }
            std::io::SeekFrom::End(pos) => {
                // if the total samples is unknown in streaminfo,
                // we have no way to know where the file's end is
                // (this is a very unusual case)
                let max_pos: u64 = decoder.total_samples().map(|s| s.get()).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::NotSeekable, "total samples not known")
                })?;

                match pos.cmp(&0) {
                    Ordering::Less => max_pos.checked_sub(pos.unsigned_abs()).ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "cannot seek below byte 0",
                        )
                    })?,
                    Ordering::Equal => max_pos,
                    Ordering::Greater => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "cannot seek beyond end of file",
                        ));
                    }
                }
            }
        };

        // perform seek in stream to the desired sample
        // (this will usually be some position prior to the desired sample)
        let mut new_pos = decoder.seek(
            self.frames_start
                .ok_or(std::io::Error::from(std::io::ErrorKind::NotSeekable))?,
            desired_pos / bytes_per_pcm_frame,
        )? * bytes_per_pcm_frame;

        // seeking invalidates current buffer
        buf.clear();

        // skip bytes to reach desired sample
        while new_pos < desired_pos {
            use std::io::BufRead;

            let buf = self.fill_buf()?;

            if !buf.is_empty() {
                let to_skip = (usize::try_from(desired_pos - new_pos).unwrap()).min(buf.len());
                self.consume(to_skip);
                new_pos += to_skip as u64;
            } else {
                // attempting to seek beyond the end of the FLAC file
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "stream exhausted before sample reached",
                ));
            }
        }

        Ok(desired_pos)
    }
}

impl<R: std::io::Read + std::io::Seek> FlacSampleReader<R> {
    /// Seeks to the given channel-independent sample
    ///
    /// The sample is relative to the beginning of the stream
    pub fn seek(&mut self, sample: u64) -> Result<(), Error> {
        let channels: u8 = self.channel_count();

        // actual position, in channel-independent samples
        let mut pos = self.decoder.seek(
            self.frames_start
                .ok_or(std::io::Error::from(std::io::ErrorKind::NotSeekable))?,
            sample,
        )?;

        // seeking invalidates the current buffer
        self.buf.clear();

        // needed channel-independent samples
        while sample > pos {
            let buf = self.fill_buf()?;

            // size of buf in channel-independent samples
            match buf.len() / usize::from(channels) {
                0 => {
                    // read beyond end of stream
                    return Err(Error::InvalidSeek);
                }
                buf_samples => {
                    // amount of channel-independent samples to consume
                    let to_consume = buf_samples.min((sample - pos).try_into().unwrap());

                    // mark samples in buffer as consumed
                    self.consume(to_consume * usize::from(channels));

                    // advance current actual position
                    pos += to_consume as u64;
                }
            }
        }

        Ok(())
    }
}

/// A FLAC reader which outputs non-interleaved channels
///
/// # Example
/// ```
/// use flac_codec::{
///     encode::{FlacChannelWriter, Options},
///     decode::FlacChannelReader,
/// };
/// use std::io::{Cursor, Seek};
///
/// let mut flac = Cursor::new(vec![]);  // a FLAC file in memory
///
/// let mut writer = FlacChannelWriter::new(
///     &mut flac,           // our wrapped writer
///     Options::default(),  // default encoding options
///     44100,               // sample rate
///     16,                  // bits-per-sample
///     2,                   // channel count
///     Some(5),             // total channel-independent samples
/// ).unwrap();
///
/// // write our samples, divided by channel
/// let written_samples = vec![
///     vec![1, 2, 3, 4, 5],
///     vec![-1, -2, -3, -4, -5],
/// ];
/// assert!(writer.write(&written_samples).is_ok());
///
/// // finalize writing file
/// assert!(writer.finalize().is_ok());
///
/// flac.rewind().unwrap();
///
/// // open reader around written FLAC file
/// let mut reader = FlacChannelReader::new(flac).unwrap();
///
/// // read a buffer's worth of samples
/// let read_samples = reader.fill_buf().unwrap();
///
/// // ensure the channels match
/// assert_eq!(read_samples.len(), written_samples.len());
/// assert_eq!(read_samples[0], written_samples[0]);
/// assert_eq!(read_samples[1], written_samples[1]);
/// ```
#[derive(Clone)]
pub struct FlacChannelReader<R> {
    // the wrapped decoder
    decoder: Decoder<R>,
    // amount of consumed frames across all channels
    consumed: usize,
    // start of FLAC frames
    frames_start: Option<u64>,
}

impl<R: std::io::Read> FlacChannelReader<R> {
    /// Opens new FLAC reader which wraps the given reader
    ///
    /// The reader must be positioned at the start of the
    /// FLAC stream.  If the file has non-FLAC data
    /// at the beginning (such as ID3v2 tags), one
    /// should skip such data before initializing a `FlacByteReader`.
    #[inline]
    pub fn new(mut reader: R) -> Result<Self, Error> {
        let blocklist = BlockList::read(reader.by_ref())?;

        Ok(Self {
            decoder: Decoder::new(reader, blocklist),
            consumed: 0,
            frames_start: None,
        })
    }

    /// Returns FLAC metadata blocks
    #[inline]
    pub fn metadata(&self) -> &BlockList {
        self.decoder.metadata()
    }

    /// Returns complete buffer of all read samples
    ///
    /// Analogous to [`std::io::BufRead::fill_buf`], this should
    /// be paired with [`FlacSampleReader::consume`] to
    /// consume samples in the filled buffer once used.
    ///
    /// Returned samples are returned as a `Vec` of channel slices,
    /// like: [[left₀ , left₁ , left₂ , …], [right₀ , right₁ , right₂ , …]]
    ///
    /// The returned buffer will always contain at least one channel.
    /// All channel slices will always be the same length.
    ///
    /// # Errors
    ///
    /// Returns error if some error occurs reading FLAC file
    /// to fill buffer.
    #[inline]
    pub fn fill_buf(&mut self) -> Result<Vec<&[i32]>, Error> {
        if self.consumed < self.decoder.buf.pcm_frames() {
            Ok(self
                .decoder
                .buf
                .channels()
                .map(|c| &c[self.consumed..])
                .collect())
        } else {
            self.consumed = 0;
            let channels = usize::from(self.decoder.channel_count().get());
            match self.decoder.read_frame()? {
                Some(frame) => Ok(frame.channels().collect()),
                None => Ok(vec![&[]; channels]),
            }
        }
    }

    /// Informs the reader that `amt` channel-indepedent samples
    /// have been consumed.
    ///
    /// Analagous to [`std::io::BufRead::consume`], which marks
    /// samples as having been read.
    ///
    /// May panic if attempting to consume more bytes
    /// than are available in the buffer.
    #[inline]
    pub fn consume(&mut self, amt: usize) {
        self.consumed += amt;
    }
}

impl<R: std::io::Read + std::io::Seek> FlacChannelReader<R> {
    /// Opens a new seekable FLAC reader which wraps the given reader
    ///
    /// If a stream is both readable and seekable,
    /// it's vital to use this method to open it if one
    /// also wishes to seek within the FLAC stream.
    /// Otherwise, an I/O error will result when attempting to seek.
    ///
    /// [`FlacChannelReader::open`] calls this method to ensure
    /// all `File`-based streams are also seekable.
    ///
    /// The reader must be positioned at the start of the
    /// FLAC stream.  If the file has non-FLAC data
    /// at the beginning (such as ID3v2 tags), one
    /// should skip such data before initializing a `FlacChannelReader`.
    pub fn new_seekable(mut reader: R) -> Result<Self, Error> {
        let blocklist = BlockList::read(reader.by_ref())?;
        let frames_start = reader.stream_position()?;

        Ok(Self {
            decoder: Decoder::new(reader, blocklist),
            consumed: 0,
            frames_start: Some(frames_start),
        })
    }
}

impl FlacChannelReader<BufReader<File>> {
    /// Opens FLAC file from the given path
    #[inline]
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        FlacChannelReader::new_seekable(BufReader::new(File::open(path.as_ref())?))
    }
}

impl<R: std::io::Read + std::io::Seek> FlacChannelReader<R> {
    /// Seeks to the given channel-independent sample
    ///
    /// The sample is relative to the beginning of the stream
    pub fn seek(&mut self, sample: u64) -> Result<(), Error> {
        // actual position, in channel-independent samples
        let mut pos = self.decoder.seek(
            self.frames_start
                .ok_or(std::io::Error::from(std::io::ErrorKind::NotSeekable))?,
            sample,
        )?;

        // seeking invalidates the current samples consumed
        self.consumed = 0;

        // needed channel-independent samples
        while sample > pos {
            let buf = self.fill_buf()?;

            // size of buf in channel-independent samples
            match buf[0].len() {
                0 => {
                    // read beyond end of stream
                    return Err(Error::InvalidSeek);
                }
                buf_samples => {
                    // amount of channel-independent samples to consume
                    let to_consume = buf_samples.min((sample - pos).try_into().unwrap());

                    // mark samples in buffer as consumed
                    self.consume(to_consume);

                    // advance current actual position
                    pos += to_consume as u64;
                }
            }
        }

        Ok(())
    }
}

impl<R: std::io::Read> Metadata for FlacChannelReader<R> {
    #[inline]
    fn channel_count(&self) -> u8 {
        self.decoder.channel_count().get()
    }

    #[inline]
    fn channel_mask(&self) -> ChannelMask {
        self.decoder.channel_mask()
    }

    #[inline]
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    #[inline]
    fn bits_per_sample(&self) -> u32 {
        self.decoder.bits_per_sample()
    }

    #[inline]
    fn total_samples(&self) -> Option<u64> {
        self.decoder.total_samples().map(|s| s.get())
    }

    #[inline]
    fn md5(&self) -> Option<&[u8; 16]> {
        self.decoder.md5()
    }
}

/// A FLAC reader which operates on streamed input
///
/// Streamed FLAC files are simply a collection of raw
/// FLAC frames with no accompanying metadata,
/// like might be delivered over a multicast stream.
///
/// Because this reader needs to scan the stream for
/// valid frame sync codes before playback,
/// it requires [`std::io::BufRead`] instead of [`std::io::Read`].
///
/// # Example
///
/// ```
/// use flac_codec::{
///     decode::{FlacStreamReader, FrameBuf},
///     encode::{FlacStreamWriter, Options},
/// };
/// use std::io::{Cursor, Seek};
/// use std::num::NonZero;
/// use bitstream_io::SignedBitCount;
///
/// let mut flac = Cursor::new(vec![]);
///
/// let samples = (0..100).collect::<Vec<i32>>();
///
/// let mut w = FlacStreamWriter::new(&mut flac, Options::default());
///
/// // write a single FLAC frame with some samples
/// w.write(
///     44100,  // sample rate
///     1,      // channels
///     16,     // bits-per-sample
///     &samples,
/// ).unwrap();
///
/// flac.rewind().unwrap();
///
/// let mut r = FlacStreamReader::new(&mut flac);
///
/// // read a single FLAC frame with some samples
/// assert_eq!(
///     r.read().unwrap(),
///     FrameBuf {
///         samples: &samples,
///         sample_rate: 44100,
///         channels: 1,
///         bits_per_sample: 16,
///     },
/// );
/// ```
pub struct FlacStreamReader<R> {
    // the wrapped reader
    reader: R,
    // raw decoded frame samples
    buf: Frame,
    // interlaced frame samples
    samples: Vec<i32>,
}

impl<R: std::io::BufRead> FlacStreamReader<R> {
    /// Opens new FLAC stream reader which wraps the given reader
    #[inline]
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Frame::default(),
            samples: Vec::default(),
        }
    }

    /// Returns the next decoded FLAC frame and its parameters
    ///
    /// # Errors
    ///
    /// Returns an I/O error from the stream or if any
    /// other error occurs when reading the file.
    pub fn read(&mut self) -> Result<FrameBuf<'_>, Error> {
        use crate::crc::{Checksum, Crc16, CrcReader};
        use crate::stream::FrameHeader;
        use bitstream_io::{BigEndian, BitReader};
        use std::io::Read;

        // Finding the next frame header in a BufRead is
        // tougher than it seems because fill_buf might
        // slice a frame sync code in half, which needs
        // to be accounted for.

        let (header, mut crc16_reader) = loop {
            // scan for the first byte of the frame sync
            self.reader.skip_until(0b11111111)?;

            // either gotten the first half of the frame sync,
            // or have reached EOF

            // check that the next byte is the other half of a frame sync
            match self.reader.fill_buf() {
                Ok([]) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "eof looking for frame sync",
                    )
                    .into());
                }
                Ok([byte, ..]) if byte >> 1 == 0b1111100 => {
                    // got a whole frame sync
                    // so try to parse a whole frame header
                    let mut crc_reader: CrcReader<_, Crc16> = CrcReader::new(
                        std::slice::from_ref(&0b11111111).chain(self.reader.by_ref()),
                    );

                    if let Ok(header) = FrameHeader::read_subset(&mut crc_reader) {
                        break (header, crc_reader);
                    }
                }
                Ok(_) => continue,
                // didn't get the other half of frame sync,
                // so continue without consuming anything
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        };

        read_subframes(
            BitReader::endian(crc16_reader.by_ref(), BigEndian),
            &header,
            &mut self.buf,
        )?;

        if crc16_reader.into_checksum().valid() {
            self.samples.clear();
            self.samples.extend(self.buf.iter());

            Ok(FrameBuf {
                samples: self.samples.as_slice(),
                sample_rate: header.sample_rate.into(),
                channels: header.channel_assignment.count(),
                bits_per_sample: header.bits_per_sample.into(),
            })
        } else {
            Err(Error::Crc16Mismatch)
        }
    }
}

/// A buffer of samples read from a [`FlacStreamReader`]
///
/// In a conventional FLAC reader, the stream's metadata
/// is known in advance from the required STREAMINFO metadata block
/// and is an error for it to change mid-file.
///
/// In a streamed reader, that metadata isn't known in advance
/// and can change from frame to frame.  This buffer contains
/// all the metadata fields in the frame for decoding/playback.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FrameBuf<'s> {
    /// Decoded samples
    ///
    /// Samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    pub samples: &'s [i32],

    /// The sample rate, in Hz
    pub sample_rate: u32,

    /// Channel count, from 1 to 8
    pub channels: u8,

    /// Bits-per-sample, from 4 to 32
    pub bits_per_sample: u32,
}

/// The results of FLAC file verification
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum Verified {
    /// FLAC file has MD5 hash and decoded contents match that hash
    MD5Match,
    /// FLAC file has MD5 hash, but decoded contents do not match
    MD5Mismatch,
    /// FLAC file has no MD5 hash, but decodes successfully
    NoMD5,
}

/// Verifies FLAC file for correctness
pub fn verify<P: AsRef<Path>>(p: P) -> Result<Verified, Error> {
    File::open(p.as_ref())
        .map_err(Error::Io)
        .and_then(|r| verify_reader(BufReader::new(r)))
}

/// Verifies FLAC stream for correctness
///
/// The stream must be set to the start of the FLAC data
pub fn verify_reader<R: std::io::Read>(r: R) -> Result<Verified, Error> {
    use crate::byteorder::LittleEndian;

    let mut r = FlacByteReader::endian(r, LittleEndian)?;
    match r.md5().cloned() {
        Some(flac_md5) => {
            let mut output_md5 = Md5::default();
            std::io::copy(&mut r, &mut output_md5)?;
            Ok(if flac_md5 == output_md5.finalize() {
                Verified::MD5Match
            } else {
                Verified::MD5Mismatch
            })
        }
        None => std::io::copy(&mut r, &mut std::io::sink())
            .map(|_| Verified::NoMD5)
            .map_err(Error::Io),
    }
}

/// A FLAC decoder
#[derive(Clone)]
struct Decoder<R> {
    reader: R,
    // all metadata blocks
    blocks: BlockList,
    // // the size of everything before the first frame, in bytes
    // frames_start: u64,
    // the current sample, in channel-independent samples
    current_sample: u64,
    // raw decoded frame samples
    buf: Frame,
    // bytes read ahead of the frames decoded so far
    input: crate::frames::ReadAhead,
}

impl<R: std::io::Read> Decoder<R> {
    /// Builds a new FLAC decoder from the given stream
    ///
    /// This assumes the stream is positioned at the start
    /// of the file.
    ///
    /// # Errors
    ///
    /// Returns an error of the initial FLAC metadata
    /// is invalid or an I/O error occurs reading
    /// the initial metadata.
    fn new(reader: R, blocks: BlockList) -> Self {
        Self {
            reader,
            blocks,
            current_sample: 0,
            buf: Frame::default(),
            input: crate::frames::ReadAhead::default(),
        }
    }

    /// Returns channel count
    ///
    /// From 1 to 8
    fn channel_count(&self) -> NonZero<u8> {
        self.blocks.streaminfo().channels
    }

    fn channel_mask(&self) -> ChannelMask {
        self.blocks.channel_mask()
    }

    /// Returns sample rate, in Hz
    fn sample_rate(&self) -> u32 {
        self.blocks.streaminfo().sample_rate
    }

    /// Returns decoder's bits-per-sample
    ///
    /// From 1 to 32
    fn bits_per_sample(&self) -> u32 {
        self.blocks.streaminfo().bits_per_sample.into()
    }

    /// Returns total number of channel-independent samples, if known
    fn total_samples(&self) -> Option<NonZero<u64>> {
        self.blocks.streaminfo().total_samples
    }

    /// Returns MD5 of entire stream, if known
    fn md5(&self) -> Option<&[u8; 16]> {
        self.blocks.streaminfo().md5.as_ref()
    }

    /// Returns FLAC metadata
    fn metadata(&self) -> &BlockList {
        &self.blocks
    }

    /// Returns decoded frame, if any.
    ///
    /// # Errors
    ///
    /// Returns any decoding error from the stream.
    fn read_frame(&mut self) -> Result<Option<&Frame>, Error> {
        use crate::crc::{Checksum, Crc16};
        use crate::frames::{ReadAhead, SliceBitReader};

        // refills for this frame; after that, the rest of it is read from the
        // stream, so a source that hands out tiny pieces costs no repeated work
        let mut refills = 0;
        loop {
            let pending = self.input.pending();
            let mut reader = SliceBitReader::new(pending);
            let decoded =
                frame_header(&self.blocks, self.current_sample, &mut reader).and_then(|header| {
                    match header {
                        Some(header) => read_subframes(&mut reader, &header, &mut self.buf)
                            .map(|()| Some(u16::from(header.block_size))),
                        None => Ok(None),
                    }
                });

            if reader.ran_out() && !self.input.at_eof() {
                // the frame goes on past what has been read ahead
                if refills == ReadAhead::REFILLS || pending.len() >= ReadAhead::LIMIT {
                    return self.read_frame_from_stream();
                }
                self.input.fill(&mut self.reader)?;
                refills += 1;
                continue;
            }

            let consumed = reader.consumed();
            let crc16 = Crc16::default().update_slice(&pending[..consumed]);
            // the bytes a failed frame read are consumed too, as from a stream
            self.input.consume(consumed);

            return match decoded? {
                Some(block_size) if crc16.valid() => {
                    self.current_sample += u64::from(block_size);
                    Ok(Some(&self.buf))
                }
                Some(_) => Err(Error::Crc16Mismatch),
                None => Ok(None),
            };
        }
    }

    /// Decodes a frame straight from the stream, behind the bytes already
    /// read ahead: one too large for memory, or one from a source that
    /// hands out small pieces
    #[cold]
    fn read_frame_from_stream(&mut self) -> Result<Option<&Frame>, Error> {
        use crate::crc::{Checksum, Crc16, CrcReader};
        use bitstream_io::{BigEndian, BitReader};
        use std::io::Read;

        let pending = self.input.take_pending();
        let mut rest = pending.as_slice();
        let decoded = {
            let mut crc16_reader: CrcReader<_, Crc16> =
                CrcReader::new((&mut rest).chain(self.reader.by_ref()));
            frame_header(&self.blocks, self.current_sample, &mut crc16_reader).and_then(|header| {
                match header {
                    Some(header) => read_subframes(
                        BitReader::endian(crc16_reader.by_ref(), BigEndian),
                        &header,
                        &mut self.buf,
                    )
                    .map(|()| Some((u16::from(header.block_size), crc16_reader.into_checksum()))),
                    None => Ok(None),
                }
            })
        };
        // whatever the frame did not read stays ahead of the stream
        self.input.restore(rest);

        match decoded? {
            Some((block_size, crc16)) if crc16.valid() => {
                self.current_sample += u64::from(block_size);
                Ok(Some(&self.buf))
            }
            Some(_) => Err(Error::Crc16Mismatch),
            None => Ok(None),
        }
    }
}

/// Reads the next frame's header, or `None` at the end of the stream
fn frame_header<R: std::io::Read>(
    blocks: &BlockList,
    current_sample: u64,
    reader: &mut R,
) -> Result<Option<crate::stream::FrameHeader>, Error> {
    use crate::stream::FrameHeader;

    match blocks
        .streaminfo()
        .total_samples
        .map(|total| total.get().checked_sub(current_sample))
    {
        Some(Some(0)) => Ok(None),
        Some(Some(remaining)) => {
            FrameHeader::read(reader, blocks.streaminfo()).and_then(|header| {
                let block_size = u16::from(header.block_size);
                if u64::from(block_size) > remaining {
                    // a block may not run past the total in STREAMINFO
                    Err(Error::TooManySamples)
                } else if u64::from(block_size) == remaining || block_size > 14 {
                    // only the last block in a stream may contain <= 14 samples
                    Ok(Some(header))
                } else {
                    Err(Error::ShortBlock)
                }
            })
        }
        Some(None) => Err(Error::TooManySamples),
        // if total number of remaining samples isn't known,
        // treat an EOF error as the end of stream
        // (this is an uncommon case)
        None => match FrameHeader::read(reader, blocks.streaminfo()) {
            Ok(header) => Ok(Some(header)),
            Err(Error::Io(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(err) => Err(err),
        },
    }
}

impl<R: std::io::Seek> Decoder<R> {
    /// Attempts to seek to desired sample number
    ///
    /// Sample number is indicated in channel-independent samples.
    ///
    /// Upon success, returns the actual sample number
    /// the stream is positioned to, in channel-independent samples,
    /// which may be less than the desired sample.
    ///
    /// # Errors
    ///
    /// Passes along an I/O error that occurs when seeking
    /// within the file.
    fn seek(&mut self, frames_start: u64, sample: u64) -> Result<u64, Error> {
        use crate::metadata::SeekPoint;
        use std::io::SeekFrom;

        match self.blocks.get() {
            Some(SeekTable { points: seektable }) => {
                match seektable.iter().rfind(|point| {
                    point
                        .sample_offset()
                        .map(|offset| offset <= sample)
                        .unwrap_or(false)
                }) {
                    Some(SeekPoint::Defined {
                        sample_offset,
                        byte_offset,
                        ..
                    }) => {
                        assert!(*sample_offset <= sample);
                        self.reader
                            .seek(SeekFrom::Start(frames_start + byte_offset))?;
                        self.input.clear();
                        self.current_sample = *sample_offset;
                        Ok(*sample_offset)
                    }
                    _ => {
                        // empty seektable so rewind to start of stream
                        self.reader.seek(SeekFrom::Start(frames_start))?;
                        self.input.clear();
                        self.current_sample = 0;
                        Ok(0)
                    }
                }
            }
            None => {
                // no seektable
                // all we can do is rewind data to start of stream
                self.reader.seek(SeekFrom::Start(frames_start))?;
                self.input.clear();
                self.current_sample = 0;
                Ok(0)
            }
        }
    }
}

fn read_subframes<R: ReadRice>(
    mut reader: R,
    header: &crate::stream::FrameHeader,
    buf: &mut Frame,
) -> Result<(), Error> {
    use crate::stream::ChannelAssignment;

    match header.channel_assignment {
        ChannelAssignment::Independent(total_channels) => {
            buf.resized_channels(
                header.bits_per_sample.into(),
                total_channels.into(),
                u16::from(header.block_size).into(),
            )
            .try_for_each(|channel| {
                read_subframe(&mut reader, header.bits_per_sample.into(), channel)
            })?;
        }
        ChannelAssignment::LeftSide => {
            let (left, side) = buf.resized_stereo(
                header.bits_per_sample.into(),
                u16::from(header.block_size).into(),
            );

            read_subframe(&mut reader, header.bits_per_sample.into(), left)?;

            match header.bits_per_sample.checked_add(1) {
                Some(side_bps) => {
                    read_subframe(&mut reader, side_bps, side)?;

                    left_side(left, side);
                }
                None => {
                    // the very rare case of 32-bps streams
                    // accompanied by side channels
                    let mut side_i64 = vec![0; side.len()];

                    read_subframe::<33, R, i64>(
                        &mut reader,
                        SignedBitCount::from(header.bits_per_sample)
                            .checked_add(1)
                            .expect("excessive bps for substream"),
                        &mut side_i64,
                    )?;

                    left_side_wide(left, &side_i64, side);
                }
            }
        }
        ChannelAssignment::SideRight => {
            let (side, right) = buf.resized_stereo(
                header.bits_per_sample.into(),
                u16::from(header.block_size).into(),
            );

            match header.bits_per_sample.checked_add(1) {
                Some(side_bps) => {
                    read_subframe(&mut reader, side_bps, side)?;
                    read_subframe(&mut reader, header.bits_per_sample.into(), right)?;

                    side_right(side, right);
                }
                None => {
                    // the very rare case of 32-bps streams
                    // accompanied by side channels
                    let mut side_i64 = vec![0; side.len()];

                    read_subframe::<33, R, i64>(
                        &mut reader,
                        SignedBitCount::from(header.bits_per_sample)
                            .checked_add(1)
                            .expect("excessive bps for substream"),
                        &mut side_i64,
                    )?;
                    read_subframe(&mut reader, header.bits_per_sample.into(), right)?;

                    side_right_wide(&side_i64, right, side);
                }
            }
        }
        ChannelAssignment::MidSide => {
            let (mid, side) = buf.resized_stereo(
                header.bits_per_sample.into(),
                u16::from(header.block_size).into(),
            );

            read_subframe(&mut reader, header.bits_per_sample.into(), mid)?;

            match header.bits_per_sample.checked_add(1) {
                Some(side_bps) => {
                    read_subframe(&mut reader, side_bps, side)?;

                    mid_side(mid, side);
                }
                None => {
                    // the very rare case of 32-bps streams
                    // accompanied by side channels
                    let mut side_i64 = vec![0; side.len()];

                    read_subframe::<33, R, i64>(
                        &mut reader,
                        SignedBitCount::from(header.bits_per_sample)
                            .checked_add(1)
                            .expect("excessive bps for substream"),
                        &mut side_i64,
                    )?;

                    mid_side_wide(mid, &side_i64, side);
                }
            }
        }
    }

    reader.byte_align();
    reader.skip(16)?; // CRC-16 checksum

    Ok(())
}

// Stereo decorrelation. A damaged stream can push the sums past the range
// of the sample type; wrapping arithmetic makes debug builds wrap around as
// release builds always have, instead of panicking (the frame CRC rejects
// such a frame afterwards).

/// Left and side to left and right: `side` becomes the right channel.
#[inline]
fn left_side(left: &[i32], side: &mut [i32]) {
    left.iter().zip(side.iter_mut()).for_each(|(left, side)| {
        *side = left.wrapping_sub(*side);
    });
}

/// Like [`left_side`], with the 33-bit side channel of a 32-bit stream.
#[inline]
fn left_side_wide(left: &[i32], side_i64: &[i64], side: &mut [i32]) {
    left.iter()
        .zip(side_i64)
        .zip(side.iter_mut())
        .for_each(|((left, side_i64), side)| {
            *side = i64::from(*left).wrapping_sub(*side_i64) as i32;
        });
}

/// Side and right to left and right: `side` becomes the left channel.
#[inline]
fn side_right(side: &mut [i32], right: &[i32]) {
    side.iter_mut().zip(right.iter()).for_each(|(side, right)| {
        *side = side.wrapping_add(*right);
    });
}

/// Like [`side_right`], with the 33-bit side channel of a 32-bit stream.
#[inline]
fn side_right_wide(side_i64: &[i64], right: &[i32], side: &mut [i32]) {
    side.iter_mut()
        .zip(side_i64)
        .zip(right.iter())
        .for_each(|((side, side_i64), right)| {
            *side = side_i64.wrapping_add(i64::from(*right)) as i32;
        });
}

/// Mid and side to left and right.
#[inline]
fn mid_side(mid: &mut [i32], side: &mut [i32]) {
    mid.iter_mut().zip(side.iter_mut()).for_each(|(mid, side)| {
        // the low bit of side is the bit mid lost (|side| % 2, also for i32::MIN)
        let sum = mid.wrapping_mul(2) | (*side & 1);
        *mid = sum.wrapping_add(*side) >> 1;
        *side = sum.wrapping_sub(*side) >> 1;
    });
}

/// Like [`mid_side`], with the 33-bit side channel of a 32-bit stream.
#[inline]
fn mid_side_wide(mid: &mut [i32], side_i64: &[i64], side: &mut [i32]) {
    mid.iter_mut()
        .zip(side.iter_mut())
        .zip(side_i64)
        .for_each(|((mid, side), side_i64)| {
            let sum = i64::from(*mid).wrapping_mul(2) | (side_i64 & 1);
            *mid = (sum.wrapping_add(*side_i64) >> 1) as i32;
            *side = (sum.wrapping_sub(*side_i64) >> 1) as i32;
        });
}

#[test]
fn decorrelation_of_valid_samples() {
    // left 100 and right -30 are side 130 and mid 35
    let mut right = [130];
    left_side(&[100], &mut right);
    assert_eq!(right, [-30]);

    let mut left = [130];
    side_right(&mut left, &[-30]);
    assert_eq!(left, [100]);

    let (mut mid, mut side) = ([35], [130]);
    mid_side(&mut mid, &mut side);
    assert_eq!((mid, side), ([100], [-30]));

    // the same through the 33-bit side channel
    let mut out = [0];
    left_side_wide(&[100], &[130], &mut out);
    assert_eq!(out, [-30]);
    side_right_wide(&[130], &[-30], &mut out);
    assert_eq!(out, [100]);
    let (mut mid, mut side) = ([35], [0]);
    mid_side_wide(&mut mid, &[130], &mut side);
    assert_eq!((mid, side), ([100], [-30]));
}

#[test]
fn decorrelation_overflow_wraps() {
    // damaged samples at the edges of the range wrap around, as in release builds
    let mut side = [i32::MAX, i32::MIN];
    left_side(&[i32::MIN, i32::MAX], &mut side);
    assert_eq!(side, [1, -1]);

    let mut side = [i32::MAX, i32::MIN];
    side_right(&mut side, &[1, -1]);
    assert_eq!(side, [i32::MIN, i32::MAX]);

    let (mut mid, mut side) = ([i32::MAX, i32::MIN], [i32::MIN, i32::MAX]);
    mid_side(&mut mid, &mut side);
    assert_eq!(mid, [1073741823, -1073741824]);
    assert_eq!(side, [1073741823, -1073741823]);

    let mut out = [0, 0];
    left_side_wide(&[i32::MIN, 0], &[i64::MAX, i64::MIN], &mut out);
    assert_eq!(out, [-2147483647, 0]);
    side_right_wide(&[i64::MAX, i64::MIN], &[1, -1], &mut out);
    assert_eq!(out, [0, -1]);
    let (mut mid, mut side) = ([i32::MAX, i32::MIN], [0, 0]);
    mid_side_wide(&mut mid, &[i64::MAX, i64::MIN], &mut side);
    assert_eq!(mid, [i32::MAX, i32::MIN]);
    assert_eq!(side, [i32::MIN, i32::MIN]);
}

fn read_subframe<const MAX: u32, R: ReadRice, I: SignedInteger>(
    reader: &mut R,
    bits_per_sample: SignedBitCount<MAX>,
    channel: &mut [I],
) -> Result<(), Error> {
    use crate::stream::{SubframeHeader, SubframeHeaderType};

    let header = reader.parse::<SubframeHeader>()?;

    let effective_bps = bits_per_sample
        .checked_sub::<MAX>(header.wasted_bps)
        .ok_or(Error::ExcessiveWastedBits)?;

    match header.type_ {
        SubframeHeaderType::Constant => {
            channel.fill(reader.read_signed_counted(effective_bps)?);
        }
        SubframeHeaderType::Verbatim => {
            channel.iter_mut().try_for_each(|i| {
                *i = reader.read_signed_counted(effective_bps)?;
                Ok::<(), Error>(())
            })?;
        }
        SubframeHeaderType::Fixed { order } => {
            read_fixed_subframe(
                reader,
                effective_bps,
                SubframeHeaderType::FIXED_COEFFS[order as usize],
                channel,
            )?;
        }
        SubframeHeaderType::Lpc { order } => {
            read_lpc_subframe(reader, effective_bps, order, channel)?;
        }
    }

    if header.wasted_bps > 0 {
        channel.iter_mut().for_each(|i| *i <<= header.wasted_bps);
    }

    Ok(())
}

fn read_fixed_subframe<const MAX: u32, R: ReadRice, I: SignedInteger>(
    reader: &mut R,
    bits_per_sample: SignedBitCount<MAX>,
    coefficients: &[i64],
    channel: &mut [I],
) -> Result<(), Error> {
    let (warm_up, residuals) = channel
        .split_at_mut_checked(coefficients.len())
        .ok_or(Error::InvalidFixedOrder)?;

    warm_up.iter_mut().try_for_each(|s| {
        *s = reader.read_signed_counted(bits_per_sample)?;
        Ok::<_, std::io::Error>(())
    })?;

    read_residuals(reader, coefficients.len(), residuals)?;
    predict(coefficients, 0, channel);
    Ok(())
}

fn read_lpc_subframe<const MAX: u32, R: ReadRice, I: SignedInteger>(
    reader: &mut R,
    bits_per_sample: SignedBitCount<MAX>,
    predictor_order: NonZero<u8>,
    channel: &mut [I],
) -> Result<(), Error> {
    let mut coefficients: [i64; 32] = [0; 32];

    let (warm_up, residuals) = channel
        .split_at_mut_checked(predictor_order.get().into())
        .ok_or(Error::InvalidLpcOrder)?;

    warm_up.iter_mut().try_for_each(|s| {
        *s = reader.read_signed_counted(bits_per_sample)?;
        Ok::<_, std::io::Error>(())
    })?;

    let qlp_precision: SignedBitCount<15> = reader
        .read_count::<0b1111>()?
        .checked_add(1)
        .and_then(|c| c.signed_count())
        .ok_or(Error::InvalidQlpPrecision)?;

    let qlp_shift: u32 = reader
        .read::<5, i32>()?
        .try_into()
        .map_err(|_| Error::NegativeLpcShift)?;

    let coefficients = &mut coefficients[0..predictor_order.get().into()];

    coefficients.iter_mut().try_for_each(|c| {
        *c = reader.read_signed_counted(qlp_precision)?;
        Ok::<_, std::io::Error>(())
    })?;

    read_residuals(reader, coefficients.len(), residuals)?;
    predict(coefficients, qlp_shift, channel);
    Ok(())
}

fn predict<I: SignedInteger>(coefficients: &[i64], qlp_shift: u32, channel: &mut [I]) {
    // fixed-size loops for the usual orders (FIXED 1 to 4, LPC up to 12),
    // which the compiler unrolls; the same arithmetic as the general loop
    macro_rules! orders {
        ($($n:literal)*) => {
            match coefficients.len() {
                $($n => {
                    if let Ok(coefficients) = <&[i64; $n]>::try_from(coefficients) {
                        return predict_order(coefficients, qlp_shift, channel);
                    }
                })*
                _ => {}
            }
        };
    }
    orders!(1 2 3 4 5 6 7 8 9 10 11 12);

    if predict_long(coefficients, qlp_shift, channel) {
        return;
    }

    for split in coefficients.len()..channel.len() {
        let (predicted, residuals) = channel.split_at_mut(split);

        // wrapping arithmetic in 64 bits, truncated to the sample type:
        // a damaged stream wraps around in debug builds as it does in
        // release builds instead of panicking (its frame CRC rejects
        // the result afterwards)
        // (the oldest sample's product first and the newest one's last, which
        // lets most of the sum be ready before the newest sample is)
        residuals[0] = I::from_i64(
            residuals[0].into().wrapping_add(
                predicted[split - coefficients.len()..]
                    .iter()
                    .zip(coefficients.iter().rev())
                    .fold(0i64, |sum, (x, y)| {
                        sum.wrapping_add((*x).into().wrapping_mul(*y))
                    })
                    >> qlp_shift,
            ),
        );
    }
}

/// `predict` for exactly `N` coefficients
///
/// Each sample depends on the one just computed, so the order of the sum
/// decides how fast the loop runs: the older samples' products are added
/// first, and the newest sample's product last, from a local value instead of
/// from `channel`. The processor then has everything but one product and
/// one addition ready when the newest sample arrives. (Wrapping addition
/// gives the same sum in any order.) The four most recent samples stay in
/// local values, so that no read of `channel` touches a sample that was only
/// just written.
#[inline]
fn predict_order<const N: usize, I: SignedInteger>(
    coefficients: &[i64; N],
    qlp_shift: u32,
    channel: &mut [I],
) {
    const RECENT: usize = 4;

    if channel.len() < N {
        return;
    }
    // the coefficients of the recent samples, and of the older ones
    let (near, far) = coefficients.split_at(N.min(RECENT));
    // the samples before the one to predict, most recent first
    let mut recent = [0i64; RECENT];
    for (r, sample) in recent.iter_mut().zip(channel[..N].iter().rev()) {
        *r = (*sample).into();
    }

    for split in N..channel.len() {
        let (predicted, residuals) = channel.split_at_mut(split);
        // wrapping in 64 bits, truncated to the sample type, as in `predict`:
        // the oldest samples first
        let mut sum = 0i64;
        if N > RECENT {
            sum = predicted[split - N..split - RECENT]
                .iter()
                .zip(far.iter().rev())
                .fold(sum, |sum, (x, y)| {
                    sum.wrapping_add((*x).into().wrapping_mul(*y))
                });
        }
        // then the recent ones, the newest last
        for (x, y) in recent.iter().zip(near).rev() {
            sum = sum.wrapping_add(x.wrapping_mul(*y));
        }
        residuals[0] = I::from_i64(residuals[0].into().wrapping_add(sum >> qlp_shift));
        recent = [residuals[0].into(), recent[0], recent[1], recent[2]];
    }
}

/// `predict` for more coefficients than the fixed-size loops take, in
/// samples of at most 32 bits; `false`, with nothing done, where that does
/// not apply
///
/// A predictor of up to 32 coefficients spends nearly all its time on the
/// sum of 32 products for each sample. The products of all but the most
/// recent samples are summed in `f64`, which is exact here (see
/// [`crate::exact`]) and takes two products at a time where 64-bit integers
/// take one. They are read from a copy of the samples kept as `f64`.
///
/// The most recent samples are added as integers from local values, the
/// newest last, as in `predict_order`. There are eight of them so that the
/// `f64` copies are read well after they were stored: reading two values in
/// one go that were stored one by one has to wait until both stores are
/// through.
fn predict_long<I: SignedInteger>(coefficients: &[i64], qlp_shift: u32, channel: &mut [I]) -> bool {
    // the samples before the predicted one that are added as integers
    const RECENT: usize = 8;
    // the samples before those, which are summed in `f64`
    const OLDER: usize = 24;
    // how many of their products are summed side by side
    const LANES: usize = 8;
    // how far back a prediction reaches: the largest order
    const BACK: usize = RECENT + OLDER;
    // the samples kept as `f64`: the BACK before a run of samples, and the run
    const WINDOW: usize = 256;
    const _: () = assert!(OLDER.is_multiple_of(LANES) && BACK < WINDOW);

    let order = coefficients.len();
    if I::BITS_SIZE > 32
        || order <= RECENT
        || order > BACK
        || channel.len() < order
        || !exact::sums_are_exact(coefficients.iter().map(|c| c.unsigned_abs()))
    {
        return false;
    }
    let (near, far) = coefficients.split_at(RECENT);
    // the older samples' coefficients, the oldest sample's first, with zeros
    // for the samples beyond this predictor's order
    let mut older = [0.0; OLDER];
    for (older, coefficient) in older.iter_mut().rev().zip(far) {
        *older = *coefficient as f64;
    }
    // how many of the older samples this predictor reaches, in whole groups
    let reach = far.len().next_multiple_of(LANES);

    let (warm_up, rest) = channel.split_at_mut(order);
    // `history[i + BACK]` is the place of a run's sample `i`; what lies
    // before the warm-up samples is only ever multiplied by zero
    let mut history = [0.0; WINDOW];
    for (kept, sample) in history[..BACK].iter_mut().rev().zip(warm_up.iter().rev()) {
        *kept = (*sample).into() as f64;
    }
    // the samples before the one to predict, most recent first
    let mut recent = [0i64; RECENT];
    for (recent, sample) in recent.iter_mut().zip(warm_up.iter().rev()) {
        *recent = (*sample).into();
    }

    for run in rest.chunks_mut(WINDOW - BACK) {
        for (i, sample) in run.iter_mut().enumerate() {
            let mut lanes = [0.0; LANES];
            for (coefficients, samples) in older[OLDER - reach..]
                .as_chunks::<LANES>()
                .0
                .iter()
                .zip(history[i + OLDER - reach..i + OLDER].as_chunks::<LANES>().0)
            {
                for ((lane, coefficient), sample) in lanes.iter_mut().zip(coefficients).zip(samples)
                {
                    *lane += coefficient * sample;
                }
            }
            // (in pairs, so that no addition waits for more than two others)
            let [a, b, c, d, e, f, g, h] = lanes;
            let mut sum = exact::integer(((a + e) + (c + g)) + ((b + f) + (d + h)));
            for (x, y) in recent.iter().zip(near).rev() {
                sum = sum.wrapping_add(x.wrapping_mul(*y));
            }
            // wrapping and truncated to the sample type, as in `predict`
            *sample = I::from_i64((*sample).into().wrapping_add(sum >> qlp_shift));

            let value: i64 = (*sample).into();
            history[i + BACK] = value as f64;
            recent = std::array::from_fn(|k| match k {
                0 => value,
                _ => recent[k - 1],
            });
        }
        // the last BACK samples go before the next run
        history.copy_within(run.len()..run.len() + BACK, 0);
    }
    true
}

#[test]
fn long_prediction_matches_the_general_loop() {
    // the loop at the end of `predict`
    fn general(coefficients: &[i64], shift: u32, channel: &mut [i32]) {
        for split in coefficients.len()..channel.len() {
            let (predicted, residuals) = channel.split_at_mut(split);
            let sum = predicted
                .iter()
                .rev()
                .zip(coefficients)
                .fold(0i64, |sum, (x, y)| {
                    sum.wrapping_add(i64::from(*x).wrapping_mul(*y))
                });
            residuals[0] = (i64::from(residuals[0]).wrapping_add(sum >> shift)) as i32;
        }
    }

    let mut seed = 0xD6E8_FEB8_6659_FD93u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..1500 {
        // every order the function takes, with coefficients of up to 15 bits,
        // the most a stream can hold, at the edges of their range too
        let order = 9 + (next() % 24) as usize;
        let precision = 2 + (next() % 14) as u32;
        let coefficients: Vec<i64> = (0..order)
            .map(|_| match next() % 8 {
                0 => (1 << (precision - 1)) - 1,
                1 => -(1 << (precision - 1)),
                _ => i64::from((next() as i32) >> (32 - precision)),
            })
            .collect();
        let shift = (next() % 32) as u32;
        // from the warm-up samples alone to several runs of the `f64` copy,
        // with the lengths around a run's end
        let len = order
            + match round % 5 {
                0 => (next() % 3) as usize,
                1 => 222 + (next() % 5) as usize,
                2 => 446 + (next() % 5) as usize,
                _ => (next() % 1200) as usize,
            };
        // samples and residuals of 8 to 32 bits: at 32 bits the samples
        // wrap around, as a damaged stream's do
        let bits = 8 + (next() % 25) as u32;
        let samples: Vec<i32> = (0..len)
            .map(|_| match next() % 16 {
                0 => i32::MAX >> (32 - bits),
                1 => i32::MIN >> (32 - bits),
                _ => (next() as i32) >> (32 - bits),
            })
            .collect();

        let mut expected = samples.clone();
        general(&coefficients, shift, &mut expected);
        let mut long = samples.clone();
        assert!(
            predict_long(&coefficients, shift, &mut long),
            "round {round}"
        );
        assert_eq!(
            long, expected,
            "round {round}: order {order}, {len} samples"
        );
        // and `predict` as a whole gives the same
        let mut whole = samples;
        predict(&coefficients, shift, &mut whole);
        assert_eq!(whole, expected, "round {round}: order {order}");
    }

    // left to the other loops: few coefficients, more than 32, fewer samples
    // than coefficients, coefficients beyond the exact bound and wide samples
    let mut samples = [1i32; 40];
    assert!(!predict_long(&[1; 8], 0, &mut samples));
    assert!(predict_long(&[1; 9], 0, &mut samples));
    assert!(predict_long(&[1; 32], 0, &mut samples));
    assert!(!predict_long(&[1; 33], 0, &mut samples));
    assert!(!predict_long(&[1; 32], 0, &mut samples[..31]));
    assert!(predict_long(&[(1 << 20) / 16 - 1; 16], 0, &mut samples));
    assert!(!predict_long(&[(1 << 20) / 16; 16], 0, &mut samples));
    assert!(!predict_long(&[1; 16], 0, &mut [1i64; 40]));
}

#[test]
fn prediction_by_order_matches_the_general_loop() {
    // every specialised order against the general loop, with values that wrap
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for order in 1..=14 {
        for _ in 0..50 {
            let coefficients: Vec<i64> = (0..order).map(|_| next() as i16 as i64).collect();
            let shift = (next() % 16) as u32;
            let samples: Vec<i32> = (0..64).map(|_| next() as i32).collect();
            let mut general = samples.clone();
            for split in coefficients.len()..general.len() {
                let (predicted, residuals) = general.split_at_mut(split);
                let sum = predicted
                    .iter()
                    .rev()
                    .zip(&coefficients)
                    .fold(0i64, |sum, (x, y)| {
                        sum.wrapping_add(i64::from(*x).wrapping_mul(*y))
                    });
                residuals[0] = (i64::from(residuals[0]).wrapping_add(sum >> shift)) as i32;
            }
            let mut fast = samples.clone();
            predict(&coefficients, shift, &mut fast);
            assert_eq!(fast, general, "order {order}");
        }
    }
}

#[test]
fn prediction_overflow_wraps() {
    // a damaged residual can push a sample past i32's range
    let mut buffer = [i32::MAX, 1, i32::MIN, -1];
    predict(&[1], 0, &mut buffer);
    assert_eq!(
        buffer,
        [
            i32::MAX,
            i32::MAX.wrapping_add(1),
            i32::MAX.wrapping_add(1).wrapping_add(i32::MIN),
            i32::MAX
                .wrapping_add(1)
                .wrapping_add(i32::MIN)
                .wrapping_sub(1),
        ]
    );

    // in the 64-bit side channel of 32-bit streams, damaged samples
    // can grow until the products overflow too
    let mut wide = [i64::MAX, 1, 1];
    predict(&[2], 0, &mut wide);
    let second = 1i64.wrapping_add(i64::MAX.wrapping_mul(2));
    assert_eq!(
        wide,
        [i64::MAX, second, 1i64.wrapping_add(second.wrapping_mul(2))]
    );
}

#[test]
fn verify_prediction() {
    let mut coefficients = [-75, 166, 121, -269, -75, -399, 1042];
    let mut buffer = [
        -796, -547, -285, -32, 199, 443, 670, -2, -23, 14, 6, 3, -4, 12, -2, 10,
    ];
    coefficients.reverse();
    predict(&coefficients, 9, &mut buffer);
    assert_eq!(
        &buffer,
        &[
            -796, -547, -285, -32, 199, 443, 670, 875, 1046, 1208, 1343, 1454, 1541, 1616, 1663,
            1701
        ]
    );

    let mut coefficients = [119, -255, 555, -836, 879, -1199, 1757];
    let mut buffer = [-21363, -21951, -22649, -24364, -27297, -26870, -30017, 3157];
    coefficients.reverse();
    predict(&coefficients, 10, &mut buffer);
    assert_eq!(
        &buffer,
        &[
            -21363, -21951, -22649, -24364, -27297, -26870, -30017, -29718
        ]
    );

    let mut coefficients = [
        709, -2589, 4600, -4612, 1350, 4220, -9743, 12671, -12129, 8586, -3775, -645, 3904, -5543,
        4373, 182, -6873, 13265, -15417, 11550,
    ];
    let mut buffer = [
        213238, 210830, 234493, 209515, 235139, 201836, 208151, 186277, 157720, 148176, 115037,
        104836, 60794, 54523, 412, 17943, -6025, -3713, 8373, 11764, 30094,
    ];
    coefficients.reverse();
    predict(&coefficients, 12, &mut buffer);
    assert_eq!(
        &buffer,
        &[
            213238, 210830, 234493, 209515, 235139, 201836, 208151, 186277, 157720, 148176, 115037,
            104836, 60794, 54523, 412, 17943, -6025, -3713, 8373, 11764, 33931,
        ]
    );
}

/// Bit readers that decode Rice codes
///
/// The provided method reads the code as two values; the decoder's own
/// slice reader takes it in one go.
trait ReadRice: BitRead {
    /// The next Rice-coded value with parameter `rice`, before zigzag decoding
    #[inline]
    fn read_rice<const RICE_MAX: u32>(
        &mut self,
        rice: bitstream_io::BitCount<RICE_MAX>,
    ) -> std::io::Result<u32> {
        let msb = self.read_unary::<1>()?;
        let lsb = self.read_counted::<RICE_MAX, u32>(rice)?;
        Ok((msb << u32::from(rice)) | lsb)
    }
}

impl<R: std::io::Read, E: bitstream_io::Endianness> ReadRice for bitstream_io::BitReader<R, E> {}

impl ReadRice for crate::frames::SliceBitReader<'_> {
    #[inline]
    fn read_rice<const RICE_MAX: u32>(
        &mut self,
        rice: bitstream_io::BitCount<RICE_MAX>,
    ) -> std::io::Result<u32> {
        self.rice(u32::from(rice))
    }
}

impl<T: ReadRice + ?Sized> ReadRice for &mut T {
    #[inline]
    fn read_rice<const RICE_MAX: u32>(
        &mut self,
        rice: bitstream_io::BitCount<RICE_MAX>,
    ) -> std::io::Result<u32> {
        (**self).read_rice(rice)
    }
}

fn read_residuals<R: ReadRice, I: SignedInteger>(
    reader: &mut R,
    predictor_order: usize,
    residuals: &mut [I],
) -> Result<(), Error> {
    fn read_block<const RICE_MAX: u32, R: ReadRice, I: SignedInteger>(
        reader: &mut R,
        predictor_order: usize,
        residuals: &mut [I],
    ) -> Result<(), Error> {
        use crate::stream::ResidualPartitionHeader;

        let block_size = predictor_order + residuals.len();
        let partition_order = reader.read::<4, u32>()?;
        let partition_count = 1 << partition_order;

        // more partitions than samples would make the partition size zero
        if partition_count > block_size {
            return Err(Error::InvalidPartitionOrder);
        }

        let partitions = residuals.rchunks_mut(block_size / partition_count).rev();

        if partitions.len() != partition_count {
            return Err(Error::InvalidPartitionOrder);
        }
        for partition in partitions {
            match reader.parse()? {
                ResidualPartitionHeader::Standard { rice } => {
                    partition.iter_mut().try_for_each(|s| {
                        let unsigned = reader.read_rice::<RICE_MAX>(rice)?;
                        *s = if (unsigned & 1) == 1 {
                            -(I::from_u32(unsigned >> 1)) - I::ONE
                        } else {
                            I::from_u32(unsigned >> 1)
                        };
                        Ok::<(), std::io::Error>(())
                    })?;
                }
                ResidualPartitionHeader::Escaped { escape_size } => {
                    partition.iter_mut().try_for_each(|s| {
                        *s = reader.read_signed_counted(escape_size)?;
                        Ok::<(), std::io::Error>(())
                    })?;
                }
                ResidualPartitionHeader::Constant => {
                    partition.fill(I::ZERO);
                }
            }
        }

        Ok(())
    }

    match reader.read::<2, u8>()? {
        0 => read_block::<0b1111, R, I>(reader, predictor_order, residuals),
        1 => read_block::<0b11111, R, I>(reader, predictor_order, residuals),
        _ => Err(Error::InvalidCodingMethod),
    }
}

#[test]
fn partition_order_larger_than_block() {
    use bitstream_io::{BigEndian, BitReader};

    // Rice coding method 0, partition order 15 (32768 partitions) for a block of 16 samples
    let data = [0b0011_1100, 0, 0, 0];
    let mut residuals = [0i32; 16];
    assert!(matches!(
        read_residuals(
            &mut BitReader::endian(data.as_slice(), BigEndian),
            0,
            &mut residuals
        ),
        Err(Error::InvalidPartitionOrder)
    ));
}
