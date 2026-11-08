// Copyright 2025 Brian Langenberger
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! For encoding PCM samples to FLAC files
//!
//! ## Multithreading
//!
//! Encoders will operate using multithreading if the optional `rayon` feature is enabled,
//! typically boosting performance by processing channels in parallel.
//! But because subframes must eventually be written serially, and their size cannot generally
//! be known in advance, processing two channels across two threads will not
//! encode twice as fast.

use crate::audio::Frame;
use crate::metadata::{
    Application, BlockList, BlockSize, Cuesheet, Picture, PortableMetadataBlock, SeekPoint,
    Streaminfo, VorbisComment, write_blocks,
};
use crate::stream::{
    ChannelAssignment, FrameNumber, Independent, ResidualPartitionHeader, SampleRate,
};
use crate::{Counter, Error};
use arrayvec::ArrayVec;
use bitstream_io::{BitCount, BitWrite, BitsWritten, SignedBitCount};
use std::fs::File;
use std::io::BufWriter;
use std::num::NonZero;
use std::path::Path;

const MAX_CHANNELS: usize = 8;
// maximum number of LPC coefficients
const MAX_LPC_COEFFS: usize = 32;

// Invent a vec!-like macro that the official crate lacks
macro_rules! arrayvec {
    ( $( $x:expr ),* ) => {
        {
            let mut v = ArrayVec::default();
            $( v.push($x); )*
            v
        }
    }
}

#[derive(Default)]
enum Finalized {
    NotFinalized,
    #[default]
    Finalized,
}

/// A FLAC writer which accepts samples as bytes
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
pub struct FlacByteWriter<W: std::io::Write + std::io::Seek, E: crate::byteorder::Endianness> {
    // the wrapped encoder
    encoder: Encoder<W>,
    // bytes that make up a partial FLAC frame
    buf: Vec<u8>,
    // a whole set of samples for a FLAC frame
    frame: Frame,
    // size of a single sample in bytes
    bytes_per_sample: usize,
    // size of single set of channel-independent samples in bytes
    pcm_frame_size: usize,
    // size of whole FLAC frame's samples in bytes
    frame_byte_size: usize,
    // whether the encoder has finalized the file
    finalized: Finalized,
    // the input bytes' endianness
    endianness: std::marker::PhantomData<E>,
}

impl<W: std::io::Write + std::io::Seek, E: crate::byteorder::Endianness> FlacByteWriter<W, E> {
    /// Creates new FLAC writer with the given parameters
    ///
    /// The writer should be positioned at the start of the file.
    ///
    /// `sample_rate` must be between 0 (for non-audio streams) and 2²⁰.
    ///
    /// `bits_per_sample` must be between 1 and 32.
    ///
    /// `channels` must be between 1 and 8.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of channel-independent samples written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    /// Returns error if any of the encoding parameters are invalid.
    pub fn new(
        writer: W,
        options: Options,
        sample_rate: u32,
        bits_per_sample: u32,
        channels: u8,
        total_bytes: Option<u64>,
    ) -> Result<Self, Error> {
        let bits_per_sample: SignedBitCount<32> = bits_per_sample
            .try_into()
            .map_err(|_| Error::InvalidBitsPerSample)?;

        let bytes_per_sample = u32::from(bits_per_sample).div_ceil(8) as usize;

        let pcm_frame_size = bytes_per_sample * channels as usize;

        Ok(Self {
            buf: Vec::default(),
            frame: Frame::empty(channels.into(), bits_per_sample.into()),
            bytes_per_sample,
            pcm_frame_size,
            frame_byte_size: pcm_frame_size * options.block_size as usize,
            encoder: Encoder::new(
                writer,
                options,
                sample_rate,
                bits_per_sample,
                channels,
                total_bytes
                    .map(|bytes| {
                        exact_div(bytes, channels.into())
                            .and_then(|s| exact_div(s, bytes_per_sample as u64))
                            .ok_or(Error::SamplesNotDivisibleByChannels)
                            .and_then(|b| NonZero::new(b).ok_or(Error::InvalidTotalBytes))
                    })
                    .transpose()?,
            )?,
            finalized: Finalized::NotFinalized,
            endianness: std::marker::PhantomData,
        })
    }

    /// Creates new FLAC writer with CDDA parameters
    ///
    /// The writer should be positioned at the start of the file.
    ///
    /// Sample rate is 44100 Hz, bits-per-sample is 16,
    /// channels is 2.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of channel-independent samples written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    /// Returns error if any of the encoding parameters are invalid.
    pub fn new_cdda(writer: W, options: Options, total_bytes: Option<u64>) -> Result<Self, Error> {
        Self::new(writer, options, 44100, 16, 2, total_bytes)
    }

    /// Creates new FLAC writer in the given endianness with the given parameters
    ///
    /// The writer should be positioned at the start of the file.
    ///
    /// `sample_rate` must be between 0 (for non-audio streams) and 2²⁰.
    ///
    /// `bits_per_sample` must be between 1 and 32.
    ///
    /// `channels` must be between 1 and 8.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of bytes written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    #[inline]
    pub fn endian(
        writer: W,
        _endianness: E,
        options: Options,
        sample_rate: u32,
        bits_per_sample: u32,
        channels: u8,
        total_bytes: Option<u64>,
    ) -> Result<Self, Error> {
        Self::new(
            writer,
            options,
            sample_rate,
            bits_per_sample,
            channels,
            total_bytes,
        )
    }

    fn finalize_inner(&mut self) -> Result<(), Error> {
        if matches!(std::mem::take(&mut self.finalized), Finalized::NotFinalized) {
            // encode as many bytes as possible into final frame, if necessary
            if !self.buf.is_empty() {
                use crate::byteorder::LittleEndian;

                // truncate buffer to whole PCM frames
                let buf = self.buf.as_mut_slice();
                let buf_len = buf.len();
                let buf = &mut buf[..(buf_len - buf_len % self.pcm_frame_size)];

                // convert buffer to little-endian bytes
                E::bytes_to_le(buf, self.bytes_per_sample);

                // update MD5 sum with little-endian bytes
                self.encoder.md5.consume(&buf);

                self.encoder
                    .encode(self.frame.fill_from_buf::<LittleEndian>(buf))?;
            }

            self.encoder.finalize_inner()
        } else {
            Ok(())
        }
    }

    /// Attempt to finalize stream
    ///
    /// It is necessary to finalize the FLAC encoder
    /// so that it will write any partially unwritten samples
    /// to the stream and update the [`crate::metadata::Streaminfo`] and [`crate::metadata::SeekTable`] blocks
    /// with their final values.
    ///
    /// Dropping the encoder will attempt to finalize the stream
    /// automatically, but will ignore any errors that may occur.
    pub fn finalize(mut self) -> Result<(), Error> {
        self.finalize_inner()?;
        Ok(())
    }
}

impl<E: crate::byteorder::Endianness> FlacByteWriter<BufWriter<File>, E> {
    /// Creates new FLAC file at the given path
    ///
    /// `sample_rate` must be between 0 (for non-audio streams) and 2²⁰.
    ///
    /// `bits_per_sample` must be between 1 and 32.
    ///
    /// `channels` must be between 1 and 8.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of bytes written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    #[inline]
    pub fn create<P: AsRef<Path>>(
        path: P,
        options: Options,
        sample_rate: u32,
        bits_per_sample: u32,
        channels: u8,
        total_bytes: Option<u64>,
    ) -> Result<Self, Error> {
        FlacByteWriter::new(
            BufWriter::new(options.create(path)?),
            options,
            sample_rate,
            bits_per_sample,
            channels,
            total_bytes,
        )
    }

    /// Creates new FLAC file with CDDA parameters at the given path
    ///
    /// Sample rate is 44100 Hz, bits-per-sample is 16,
    /// channels is 2.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of bytes written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    pub fn create_cdda<P: AsRef<Path>>(
        path: P,
        options: Options,
        total_bytes: Option<u64>,
    ) -> Result<Self, Error> {
        Self::create(path, options, 44100, 16, 2, total_bytes)
    }
}

impl<W: std::io::Write + std::io::Seek, E: crate::byteorder::Endianness> std::io::Write
    for FlacByteWriter<W, E>
{
    /// Writes a set of sample bytes to the FLAC file
    ///
    /// Samples are signed and encoded in the stream's given byte order.
    ///
    /// Samples are then interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// This is the same format used by common PCM container
    /// formats like WAVE and AIFF.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use crate::byteorder::LittleEndian;

        // dump whole set of bytes into our internal buffer
        self.buf.extend(buf);

        // encode as many FLAC frames as possible (which may be 0)
        let mut encoded_frames = 0;
        for buf in self
            .buf
            .as_mut_slice()
            .chunks_exact_mut(self.frame_byte_size)
        {
            // convert buffer to little-endian bytes
            E::bytes_to_le(buf, self.bytes_per_sample);

            // update MD5 sum with little-endian bytes
            self.encoder.md5.consume(&buf);

            // encode fresh FLAC frame
            self.encoder
                .encode(self.frame.fill_from_buf::<LittleEndian>(buf))?;

            encoded_frames += 1;
        }

        self.buf.drain(0..self.frame_byte_size * encoded_frames);

        // indicate whole buffer's been consumed
        Ok(buf.len())
    }

    #[inline]
    fn flush(&mut self) -> std::io::Result<()> {
        // we don't want to flush a partial frame to disk,
        // but we can at least flush our internal writer
        self.encoder.writer.flush()
    }
}

impl<W: std::io::Write + std::io::Seek, E: crate::byteorder::Endianness> Drop
    for FlacByteWriter<W, E>
{
    fn drop(&mut self) {
        let _ = self.finalize_inner();
    }
}

/// A FLAC writer which accepts samples as signed integers
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
pub struct FlacSampleWriter<W: std::io::Write + std::io::Seek> {
    // the wrapped encoder
    encoder: Encoder<W>,
    // samples that make up a partial FLAC frame
    // in channel-interleaved order
    // (must de-interleave later in case someone writes
    // only partial set of channels in a single write call)
    sample_buf: Vec<i32>,
    // a whole set of samples for a FLAC frame
    frame: Frame,
    // size of a single frame in samples
    frame_sample_size: usize,
    // size of a single PCM frame in samples
    pcm_frame_size: usize,
    // size of a single sample in bytes
    bytes_per_sample: usize,
    // whether the encoder has finalized the file
    finalized: Finalized,
}

impl<W: std::io::Write + std::io::Seek> FlacSampleWriter<W> {
    /// Creates new FLAC writer with the given parameters
    ///
    /// `sample_rate` must be between 0 (for non-audio streams) and 2²⁰.
    ///
    /// `bits_per_sample` must be between 1 and 32.
    ///
    /// `channels` must be between 1 and 8.
    ///
    /// Note that if `total_samples` is indicated,
    /// the number of samples written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    /// Returns error if any of the encoding parameters are invalid.
    pub fn new(
        writer: W,
        options: Options,
        sample_rate: u32,
        bits_per_sample: u32,
        channels: u8,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        let bits_per_sample: SignedBitCount<32> = bits_per_sample
            .try_into()
            .map_err(|_| Error::InvalidBitsPerSample)?;

        let bytes_per_sample = u32::from(bits_per_sample).div_ceil(8) as usize;

        let pcm_frame_size = usize::from(channels);

        Ok(Self {
            sample_buf: Vec::default(),
            frame: Frame::empty(channels.into(), bits_per_sample.into()),
            bytes_per_sample,
            pcm_frame_size,
            frame_sample_size: pcm_frame_size * options.block_size as usize,
            encoder: Encoder::new(
                writer,
                options,
                sample_rate,
                bits_per_sample,
                channels,
                total_samples
                    .map(|samples| {
                        exact_div(samples, channels.into())
                            .ok_or(Error::SamplesNotDivisibleByChannels)
                            .and_then(|s| NonZero::new(s).ok_or(Error::InvalidTotalSamples))
                    })
                    .transpose()?,
            )?,
            finalized: Finalized::NotFinalized,
        })
    }

    /// Creates new FLAC writer with CDDA parameters
    ///
    /// Sample rate is 44100 Hz, bits-per-sample is 16,
    /// channels is 2.
    ///
    /// Note that if `total_samples` is indicated,
    /// the number of samples written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    /// Returns error if any of the encoding parameters are invalid.
    pub fn new_cdda(
        writer: W,
        options: Options,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        Self::new(writer, options, 44100, 16, 2, total_samples)
    }

    /// Given a set of samples, writes them to the FLAC file
    ///
    /// Samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// This may output 0 or more actual FLAC frames,
    /// depending on the quantity of samples and the amount
    /// previously written.
    pub fn write(&mut self, samples: &[i32]) -> Result<(), Error> {
        // dump whole set of samples into our internal buffer
        self.sample_buf.extend(samples);

        // encode as many FLAC frames as possible (which may be 0)
        let mut encoded_frames = 0;
        for buf in self
            .sample_buf
            .as_mut_slice()
            .chunks_exact_mut(self.frame_sample_size)
        {
            // update running MD5 sum calculation
            // since samples are already interleaved in channel order
            update_md5(
                &mut self.encoder.md5,
                buf.iter().copied(),
                self.bytes_per_sample,
            );

            // encode fresh FLAC frame
            self.encoder.encode(self.frame.fill_from_samples(buf))?;

            encoded_frames += 1;
        }
        self.sample_buf
            .drain(0..self.frame_sample_size * encoded_frames);

        Ok(())
    }

    fn finalize_inner(&mut self) -> Result<(), Error> {
        if matches!(std::mem::take(&mut self.finalized), Finalized::NotFinalized) {
            // encode as many samples possible into final frame, if necessary
            if !self.sample_buf.is_empty() {
                // truncate buffer to whole PCM frames
                let buf = self.sample_buf.as_mut_slice();
                let buf_len = buf.len();
                let buf = &mut buf[..(buf_len - buf_len % self.pcm_frame_size)];

                // update running MD5 sum calculation
                // since samples are already interleaved in channel order
                update_md5(
                    &mut self.encoder.md5,
                    buf.iter().copied(),
                    self.bytes_per_sample,
                );

                // encode final FLAC frame
                self.encoder.encode(self.frame.fill_from_samples(buf))?;
            }

            self.encoder.finalize_inner()
        } else {
            Ok(())
        }
    }

    /// Attempt to finalize stream
    ///
    /// It is necessary to finalize the FLAC encoder
    /// so that it will write any partially unwritten samples
    /// to the stream and update the [`crate::metadata::Streaminfo`] and [`crate::metadata::SeekTable`] blocks
    /// with their final values.
    ///
    /// Dropping the encoder will attempt to finalize the stream
    /// automatically, but will ignore any errors that may occur.
    pub fn finalize(mut self) -> Result<(), Error> {
        self.finalize_inner()?;
        Ok(())
    }
}

impl FlacSampleWriter<BufWriter<File>> {
    /// Creates new FLAC file at the given path
    ///
    /// `sample_rate` must be between 0 (for non-audio streams) and 2²⁰.
    ///
    /// `bits_per_sample` must be between 1 and 32.
    ///
    /// `channels` must be between 1 and 8.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of bytes written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    #[inline]
    pub fn create<P: AsRef<Path>>(
        path: P,
        options: Options,
        sample_rate: u32,
        bits_per_sample: u32,
        channels: u8,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        FlacSampleWriter::new(
            BufWriter::new(options.create(path)?),
            options,
            sample_rate,
            bits_per_sample,
            channels,
            total_samples,
        )
    }

    /// Creates new FLAC file at the given path with CDDA parameters
    ///
    /// Sample rate is 44100 Hz, bits-per-sample is 16,
    /// channels is 2.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of bytes written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    #[inline]
    pub fn create_cdda<P: AsRef<Path>>(
        path: P,
        options: Options,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        Self::create(path, options, 44100, 16, 2, total_samples)
    }
}

/// A FLAC writer which accepts samples as channels of signed integers
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
pub struct FlacChannelWriter<W: std::io::Write + std::io::Seek> {
    // the wrapped encoder
    encoder: Encoder<W>,
    // channels that make up a partial FLAC frame
    channel_bufs: Vec<Vec<i32>>,
    // a whole set of samples for a FLAC frame
    frame: Frame,
    // size of a single frame in samples
    frame_sample_size: usize,
    // size of a single sample in bytes
    bytes_per_sample: usize,
    // whether the encoder has finalized the file
    finalized: Finalized,
}

impl<W: std::io::Write + std::io::Seek> FlacChannelWriter<W> {
    /// Creates new FLAC writer with the given parameters
    ///
    /// `sample_rate` must be between 0 (for non-audio streams) and 2²⁰.
    ///
    /// `bits_per_sample` must be between 1 and 32.
    ///
    /// `channels` must be between 1 and 8.
    ///
    /// Note that if `total_samples` is indicated,
    /// the number of samples written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    /// Returns error if any of the encoding parameters are invalid.
    pub fn new(
        writer: W,
        options: Options,
        sample_rate: u32,
        bits_per_sample: u32,
        channels: u8,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        let bits_per_sample: SignedBitCount<32> = bits_per_sample
            .try_into()
            .map_err(|_| Error::InvalidBitsPerSample)?;

        let bytes_per_sample = u32::from(bits_per_sample).div_ceil(8) as usize;

        Ok(Self {
            channel_bufs: vec![Vec::default(); channels.into()],
            frame: Frame::empty(channels.into(), bits_per_sample.into()),
            bytes_per_sample,
            frame_sample_size: options.block_size as usize,
            encoder: Encoder::new(
                writer,
                options,
                sample_rate,
                bits_per_sample,
                channels,
                total_samples.and_then(NonZero::new),
            )?,
            finalized: Finalized::NotFinalized,
        })
    }

    /// Creates new FLAC writer with CDDA parameters
    ///
    /// Sample rate is 44100 Hz, bits-per-sample is 16,
    /// channels is 2.
    ///
    /// Note that if `total_samples` is indicated,
    /// the number of samples written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    /// Returns error if any of the encoding parameters are invalid.
    pub fn new_cdda(
        writer: W,
        options: Options,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        Self::new(writer, options, 44100, 16, 2, total_samples)
    }

    /// Given a set of channels containing samples, writes them to the FLAC file
    ///
    /// Channels should be a slice-able set of sample slices, like:
    /// [[left₀ , left₁ , left₂ , …] , [right₀ , right₁ , right₂ , …]]
    ///
    /// The number of channels must be identical to the
    /// channel count indicated when intializing the encoder.
    ///
    /// The number of samples in each channel must also be identical.
    pub fn write<C, S>(&mut self, channels: C) -> Result<(), Error>
    where
        C: AsRef<[S]>,
        S: AsRef<[i32]>,
    {
        use crate::audio::MultiZip;

        // sanity-check our channel inputs
        let channels = channels.as_ref();

        match channels {
            whole @ [first, rest @ ..]
                if whole.len() == usize::from(self.encoder.channel_count().get()) =>
            {
                if rest
                    .iter()
                    .any(|c| c.as_ref().len() != first.as_ref().len())
                {
                    return Err(Error::ChannelLengthMismatch);
                }
            }
            _ => {
                return Err(Error::ChannelCountMismatch);
            }
        }

        // dump whole set of samples into our internal channel buffers
        for (buf, channel) in self.channel_bufs.iter_mut().zip(channels) {
            buf.extend(channel.as_ref());
        }

        // encode as many FLAC frames as possible (which may be 0)
        let mut encoded_frames = 0;
        for bufs in self
            .channel_bufs
            .iter_mut()
            .map(|v| v.as_mut_slice().chunks_exact_mut(self.frame_sample_size))
            .collect::<MultiZip<_>>()
        {
            // update running MD5 sum calculation
            update_md5(
                &mut self.encoder.md5,
                bufs.iter()
                    .map(|c| c.iter().copied())
                    .collect::<MultiZip<_>>()
                    .flatten(),
                self.bytes_per_sample,
            );

            // encode fresh FLAC frame
            self.encoder.encode(self.frame.fill_from_channels(&bufs))?;

            encoded_frames += 1;
        }

        // drain encoded samples from buffers
        for channel in self.channel_bufs.iter_mut() {
            channel.drain(0..self.frame_sample_size * encoded_frames);
        }

        Ok(())
    }

    fn finalize_inner(&mut self) -> Result<(), Error> {
        use crate::audio::MultiZip;

        if matches!(std::mem::take(&mut self.finalized), Finalized::NotFinalized) {
            // encode as many samples possible into final frame, if necessary
            if !self.channel_bufs[0].is_empty() {
                // update running MD5 sum calculation
                update_md5(
                    &mut self.encoder.md5,
                    self.channel_bufs
                        .iter()
                        .map(|c| c.iter().copied())
                        .collect::<MultiZip<_>>()
                        .flatten(),
                    self.bytes_per_sample,
                );

                // encode final FLAC frame
                self.encoder.encode(
                    self.frame.fill_from_channels(
                        self.channel_bufs
                            .iter_mut()
                            .map(|v| v.as_mut_slice())
                            .collect::<ArrayVec<_, MAX_CHANNELS>>()
                            .as_slice(),
                    ),
                )?;
            }

            self.encoder.finalize_inner()
        } else {
            Ok(())
        }
    }

    /// Attempt to finalize stream
    ///
    /// It is necessary to finalize the FLAC encoder
    /// so that it will write any partially unwritten samples
    /// to the stream and update the [`crate::metadata::Streaminfo`] and [`crate::metadata::SeekTable`] blocks
    /// with their final values.
    ///
    /// Dropping the encoder will attempt to finalize the stream
    /// automatically, but will ignore any errors that may occur.
    pub fn finalize(mut self) -> Result<(), Error> {
        self.finalize_inner()?;
        Ok(())
    }
}

impl FlacChannelWriter<BufWriter<File>> {
    /// Creates new FLAC file at the given path
    ///
    /// `sample_rate` must be between 0 (for non-audio streams) and 2²⁰.
    ///
    /// `bits_per_sample` must be between 1 and 32.
    ///
    /// `channels` must be between 1 and 8.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of bytes written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    #[inline]
    pub fn create<P: AsRef<Path>>(
        path: P,
        options: Options,
        sample_rate: u32,
        bits_per_sample: u32,
        channels: u8,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        FlacChannelWriter::new(
            BufWriter::new(options.create(path)?),
            options,
            sample_rate,
            bits_per_sample,
            channels,
            total_samples,
        )
    }

    /// Creates new FLAC file at the given path with CDDA parameters
    ///
    /// Sample rate is 44100 Hz, bits-per-sample is 16,
    /// channels is 2.
    ///
    /// Note that if `total_bytes` is indicated,
    /// the number of bytes written *must*
    /// be equal to that amount or an error will occur when writing
    /// or finalizing the stream.
    ///
    /// # Errors
    ///
    /// Returns I/O error if unable to write initial
    /// metadata blocks.
    #[inline]
    pub fn create_cdda<P: AsRef<Path>>(
        path: P,
        options: Options,
        total_samples: Option<u64>,
    ) -> Result<Self, Error> {
        Self::create(path, options, 44100, 16, 2, total_samples)
    }
}

/// A FLAC writer which operates on streamed output
///
/// Because this encodes FLAC frames without any metadata
/// blocks or finalizing, it does not need to be seekable.
///
/// # Example
///
/// ```
/// use flac_codec::{
///     decode::{FlacStreamReader, FrameBuf},
///     encode::{FlacStreamWriter, Options},
/// };
/// use std::io::{Cursor, Seek};
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
pub struct FlacStreamWriter<W> {
    // the writer we're outputting to
    writer: W,
    // various encoding optins
    options: EncoderOptions,
    // various encoder caches
    caches: EncodingCaches,
    // a whole set of samples for a FLAC frame
    frame: Frame,
    // the current frame number
    frame_number: FrameNumber,
}

impl<W: std::io::Write> FlacStreamWriter<W> {
    /// Creates new stream writer
    pub fn new(writer: W, options: Options) -> Self {
        Self {
            writer,
            options: EncoderOptions {
                max_partition_order: options.max_partition_order,
                mid_side: options.mid_side,
                seektable_interval: options.seektable_interval,
                max_lpc_order: options.max_lpc_order,
                window: options.window,
                exhaustive_channel_correlation: options.exhaustive_channel_correlation,
                use_rice2: false,
            },
            caches: EncodingCaches::default(),
            frame: Frame::default(),
            frame_number: FrameNumber::default(),
        }
    }

    /// Writes a whole FLAC frame to our output stream
    ///
    /// Samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// This writes a whole FLAC frame to the output stream on each call.
    ///
    /// # Errors
    ///
    /// Returns an error of any of the parameters are invalid
    /// or if an I/O error occurs when writing to the stream.
    pub fn write(
        &mut self,
        sample_rate: u32,
        channels: u8,
        bits_per_sample: u32,
        samples: &[i32],
    ) -> Result<(), Error> {
        use crate::stream::{BitsPerSample, FrameHeader, SampleRate};

        let bits_per_sample: SignedBitCount<32> = bits_per_sample
            .try_into()
            .map_err(|_| Error::NonSubsetBitsPerSample)?;

        // samples must divide evenly into channels
        if !samples.len().is_multiple_of(usize::from(channels)) {
            return Err(Error::SamplesNotDivisibleByChannels);
        } else if !(1..=8).contains(&channels) {
            return Err(Error::ExcessiveChannels);
        }

        self.options.use_rice2 = u32::from(bits_per_sample) > 16;

        self.frame
            .resize(bits_per_sample.into(), channels.into(), 0);
        self.frame.fill_from_samples(samples);

        // block size must be valid
        let block_size: crate::stream::BlockSize<u16> = crate::stream::BlockSize::try_from(
            u16::try_from(self.frame.pcm_frames()).map_err(|_| Error::InvalidBlockSize)?,
        )
        .map_err(|_| Error::InvalidBlockSize)?;

        // sample rate must be valid for subset streams
        let sample_rate: SampleRate<u32> = sample_rate.try_into().and_then(|rate| match rate {
            SampleRate::Streaminfo(_) => Err(Error::NonSubsetSampleRate),
            rate => Ok(rate),
        })?;

        // bits-per-sample must be valid for subset streams
        let header_bits_per_sample = match BitsPerSample::from(bits_per_sample) {
            BitsPerSample::Streaminfo(_) => return Err(Error::NonSubsetBitsPerSample),
            bps => bps,
        };

        let channels = self
            .frame
            .channels()
            .collect::<ArrayVec<&[i32], MAX_CHANNELS>>();
        let EncodingCaches {
            channels: caches,
            correlated,
            frame: buf,
        } = &mut self.caches;
        let (channel_assignment, plans) = plan_frame(
            &self.options,
            caches,
            correlated,
            &channels,
            bits_per_sample,
        )?;
        write_frame_bytes(
            buf,
            |header| {
                FrameHeader {
                    blocking_strategy: false,
                    frame_number: self.frame_number,
                    block_size,
                    sample_rate,
                    bits_per_sample: header_bits_per_sample,
                    channel_assignment,
                }
                .write_subset(header)
            },
            &plans,
        )?;
        self.writer.write_all(buf)?;

        if self.frame_number.try_increment().is_err() {
            self.frame_number = FrameNumber::default();
        }

        Ok(())
    }

    /// Writes a whole FLAC frame to our output stream with CDDA parameters
    ///
    /// Samples are interleaved by channel, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// This writes a whole FLAC frame to the output stream on each call.
    ///
    /// # Errors
    ///
    /// Returns an error of any of the parameters are invalid
    /// or if an I/O error occurs when writing to the stream.
    pub fn write_cdda(&mut self, samples: &[i32]) -> Result<(), Error> {
        self.write(44100, 2, 16, samples)
    }
}

fn update_md5(md5: &mut md5::Context, samples: impl Iterator<Item = i32>, bytes_per_sample: usize) {
    use crate::byteorder::{Endianness, LittleEndian};

    match bytes_per_sample {
        1 => {
            for s in samples {
                md5.consume(LittleEndian::i8_to_bytes(s as i8));
            }
        }
        2 => {
            for s in samples {
                md5.consume(LittleEndian::i16_to_bytes(s as i16));
            }
        }
        3 => {
            for s in samples {
                md5.consume(LittleEndian::i24_to_bytes(s));
            }
        }
        4 => {
            for s in samples {
                md5.consume(LittleEndian::i32_to_bytes(s));
            }
        }
        _ => panic!("unsupported number of bytes per sample"),
    }
}

/// The interval of seek points to generate
#[derive(Copy, Clone, Debug)]
pub enum SeekTableInterval {
    ///Generate seekpoint every nth seconds
    Seconds(NonZero<u8>),
    /// Generate seekpoint every nth frames
    Frames(NonZero<usize>),
}

impl Default for SeekTableInterval {
    fn default() -> Self {
        Self::Seconds(NonZero::new(10).unwrap())
    }
}

impl SeekTableInterval {
    // decimates full set of seekpoints based on the requested
    // seektable style, or returns None if no seektable is requested
    fn filter<'s>(
        self,
        sample_rate: u32,
        seekpoints: impl IntoIterator<Item = EncoderSeekPoint> + 's,
    ) -> Box<dyn Iterator<Item = EncoderSeekPoint> + 's> {
        match self {
            Self::Seconds(seconds) => {
                let nth_sample = u64::from(u32::from(seconds.get()) * sample_rate);
                let mut offset = 0;
                Box::new(seekpoints.into_iter().filter(move |point| {
                    if point.range().contains(&offset) {
                        offset += nth_sample;
                        true
                    } else {
                        false
                    }
                }))
            }
            Self::Frames(frames) => Box::new(seekpoints.into_iter().step_by(frames.get())),
        }
    }
}

/// FLAC encoding options
#[derive(Clone, Debug)]
pub struct Options {
    // whether to clobber existing file
    clobber: bool,
    block_size: u16,
    max_partition_order: u32,
    mid_side: bool,
    metadata: BlockList,
    seektable_interval: Option<SeekTableInterval>,
    max_lpc_order: Option<NonZero<u8>>,
    window: Window,
    exhaustive_channel_correlation: bool,
}

impl Default for Options {
    fn default() -> Self {
        // a dummy placeholder value
        // since we can't know the stream parameters yet
        let mut metadata = BlockList::new(Streaminfo {
            minimum_block_size: 0,
            maximum_block_size: 0,
            minimum_frame_size: None,
            maximum_frame_size: None,
            sample_rate: 0,
            channels: NonZero::new(1).unwrap(),
            bits_per_sample: SignedBitCount::new::<4>(),
            total_samples: None,
            md5: None,
        });

        metadata.insert(crate::metadata::Padding {
            size: 4096u16.into(),
        });

        Self {
            clobber: false,
            block_size: 4096,
            mid_side: true,
            max_partition_order: 5,
            metadata,
            seektable_interval: Some(SeekTableInterval::default()),
            max_lpc_order: NonZero::new(8),
            window: Window::default(),
            exhaustive_channel_correlation: true,
        }
    }
}

impl Options {
    /// Sets new block size
    ///
    /// Block size must be ≥ 16
    ///
    /// For subset streams, this must be ≤ 4608
    /// if the sample rate is ≤ 48 kHz -
    /// or ≤ 16384 for higher sample rates.
    pub fn block_size(self, block_size: u16) -> Result<Self, OptionsError> {
        match block_size {
            0..16 => Err(OptionsError::InvalidBlockSize),
            16.. => Ok(Self { block_size, ..self }),
        }
    }

    /// Sets new maximum LPC order
    ///
    /// The valid range is: 0 < `max_lpc_order` ≤ 32
    ///
    /// A value of `None` means that no LPC subframes will be encoded.
    pub fn max_lpc_order(self, max_lpc_order: Option<u8>) -> Result<Self, OptionsError> {
        Ok(Self {
            max_lpc_order: max_lpc_order
                .map(|o| {
                    o.try_into()
                        .ok()
                        .filter(|o| *o <= NonZero::new(32).unwrap())
                        .ok_or(OptionsError::InvalidLpcOrder)
                })
                .transpose()?,
            ..self
        })
    }

    /// Sets maximum residual partion order.
    ///
    /// The valid range is: 0 ≤ `max_partition_order` ≤ 15
    pub fn max_partition_order(self, max_partition_order: u32) -> Result<Self, OptionsError> {
        match max_partition_order {
            0..=15 => Ok(Self {
                max_partition_order,
                ..self
            }),
            16.. => Err(OptionsError::InvalidMaxPartitions),
        }
    }

    /// Whether to use mid-side encoding
    ///
    /// The default is `true`.
    pub fn mid_side(self, mid_side: bool) -> Self {
        Self { mid_side, ..self }
    }

    /// The windowing function to use for input samples
    pub fn window(self, window: Window) -> Self {
        Self { window, ..self }
    }

    /// Whether to calculate the best channel correlation quickly
    ///
    /// The default is `false`
    pub fn fast_channel_correlation(self, fast: bool) -> Self {
        Self {
            exhaustive_channel_correlation: !fast,
            ..self
        }
    }

    /// Updates size of padding block
    ///
    /// `size` must be < 2²⁴
    ///
    /// If `size` is set to 0, removes the block entirely.
    ///
    /// The default is to add a 4096 byte padding block.
    pub fn padding(mut self, size: u32) -> Result<Self, OptionsError> {
        use crate::metadata::Padding;

        match size
            .try_into()
            .map_err(|_| OptionsError::ExcessivePadding)?
        {
            BlockSize::ZERO => self.metadata.remove::<Padding>(),
            size => self.metadata.update::<Padding>(|p| {
                p.size = size;
            }),
        }
        Ok(self)
    }

    /// Remove any padding blocks from metadata
    ///
    /// This makes the file smaller, but will likely require
    /// rewriting it if any metadata needs to be modified later.
    pub fn no_padding(mut self) -> Self {
        self.metadata.remove::<crate::metadata::Padding>();
        self
    }

    /// Adds new tag to comment metadata block
    ///
    /// Creates new [`crate::metadata::VorbisComment`] block if not already present.
    pub fn tag<S>(mut self, field: &str, value: S) -> Self
    where
        S: std::fmt::Display,
    {
        self.metadata
            .update::<VorbisComment>(|vc| vc.insert(field, value));
        self
    }

    /// Replaces entire [`crate::metadata::VorbisComment`] metadata block
    ///
    /// This may be more convenient when adding many fields at once.
    pub fn comment(mut self, comment: VorbisComment) -> Self {
        self.metadata.insert(comment);
        self
    }

    /// Add new [`crate::metadata::Picture`] block to metadata
    ///
    /// Files may contain multiple [`crate::metadata::Picture`] blocks,
    /// and this adds a new block each time it is used.
    pub fn picture(mut self, picture: Picture) -> Self {
        self.metadata.insert(picture);
        self
    }

    /// Add new [`crate::metadata::Cuesheet`] block to metadata
    ///
    /// Files may (theoretically) contain multiple [`crate::metadata::Cuesheet`] blocks,
    /// and this adds a new block each time it is used.
    ///
    /// In practice, CD images almost always use only a single
    /// cue sheet.
    pub fn cuesheet(mut self, cuesheet: Cuesheet) -> Self {
        self.metadata.insert(cuesheet);
        self
    }

    /// Add new [`crate::metadata::Application`] block to metadata
    ///
    /// Files may contain multiple [`crate::metadata::Application`] blocks,
    /// and this adds a new block each time it is used.
    pub fn application(mut self, application: Application) -> Self {
        self.metadata.insert(application);
        self
    }

    /// Generate [`crate::metadata::SeekTable`] with the given number of seconds between seek points
    ///
    /// The default is to generate a SEEKTABLE with 10 seconds between seek points.
    ///
    /// If `seconds` is 0, removes the SEEKTABLE block.
    ///
    /// The interval between seek points may be larger than requested
    /// if the encoder's block size is larger than the seekpoint interval.
    pub fn seektable_seconds(mut self, seconds: u8) -> Self {
        // note that we can't drop a placeholder seektable
        // into the metadata blocks until we know
        // the sample rate and total samples of our stream
        self.seektable_interval = NonZero::new(seconds).map(SeekTableInterval::Seconds);
        self
    }

    /// Generate [`crate::metadata::SeekTable`] with the given number of FLAC frames between seek points
    ///
    /// If `frames` is 0, removes the SEEKTABLE block
    pub fn seektable_frames(mut self, frames: usize) -> Self {
        self.seektable_interval = NonZero::new(frames).map(SeekTableInterval::Frames);
        self
    }

    /// Do not generate a seektable in our encoded file
    pub fn no_seektable(self) -> Self {
        Self {
            seektable_interval: None,
            ..self
        }
    }

    /// Add new block to metadata
    ///
    /// If the block may only occur once in a file,
    /// any previous block of that same type is removed.
    pub fn add_block<B>(&mut self, block: B) -> &mut Self
    where
        B: PortableMetadataBlock,
    {
        self.metadata.insert(block);
        self
    }

    /// Add new blocks to metadata
    ///
    /// If the block may only occur once in a file,
    /// any current block of that type is replaced by
    /// the final block in the iterator - if any.
    /// Otherwise, all blocks in the iterator are used.
    pub fn add_blocks<B>(&mut self, iter: impl IntoIterator<Item = B>) -> &mut Self
    where
        B: PortableMetadataBlock,
    {
        for block in iter {
            self.metadata.insert(block);
        }
        self
    }

    /// Overwrites existing file if it already exists
    ///
    /// This only applies to encoding functions which
    /// create files from paths.
    ///
    /// The default is to not overwrite files
    /// if they already exist.
    pub fn overwrite(mut self) -> Self {
        self.clobber = true;
        self
    }

    /// Returns the fastest encoding options
    ///
    /// These are tuned to encode as quickly as possible.
    pub fn fast() -> Self {
        Self {
            block_size: 1152,
            mid_side: false,
            max_partition_order: 3,
            max_lpc_order: None,
            exhaustive_channel_correlation: false,
            ..Self::default()
        }
    }

    /// Returns the fastest encoding options
    ///
    /// These are tuned to encode files as small as possible.
    pub fn best() -> Self {
        Self {
            block_size: 4096,
            mid_side: true,
            max_partition_order: 6,
            max_lpc_order: NonZero::new(12),
            ..Self::default()
        }
    }

    /// Creates files according to whether clobber is set or not
    fn create<P: AsRef<Path>>(&self, path: P) -> std::io::Result<File> {
        if self.clobber {
            File::create(path)
        } else {
            use std::fs::OpenOptions;

            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path.as_ref())
        }
    }
}

/// An error when specifying encoding options
#[derive(Debug)]
pub enum OptionsError {
    /// Selected block size is too small
    InvalidBlockSize,
    /// Maximum LPC order is too large
    InvalidLpcOrder,
    /// Maximum residual partitions is too large
    InvalidMaxPartitions,
    /// Selected padding size is too large
    ExcessivePadding,
}

impl std::error::Error for OptionsError {}

impl std::fmt::Display for OptionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::InvalidBlockSize => "block size must be >= 16".fmt(f),
            Self::InvalidLpcOrder => "maximum LPC order must be <= 32".fmt(f),
            Self::InvalidMaxPartitions => "max partition order must be <= 15".fmt(f),
            Self::ExcessivePadding => "padding size is too large for block".fmt(f),
        }
    }
}

/// A cut-down version of Options without the metadata blocks
struct EncoderOptions {
    max_partition_order: u32,
    mid_side: bool,
    seektable_interval: Option<SeekTableInterval>,
    max_lpc_order: Option<NonZero<u8>>,
    window: Window,
    exhaustive_channel_correlation: bool,
    use_rice2: bool,
}

/// The method to use for windowing the input signal
#[derive(Copy, Clone, Debug)]
pub enum Window {
    /// Basic rectangular window
    Rectangle,
    /// Hann window
    Hann,
    /// Tukey window
    Tukey(f32),
}

// TODO - add more windowing options

impl Window {
    fn generate(&self, window: &mut [f64]) {
        use std::f64::consts::PI;

        match self {
            Self::Rectangle => window.fill(1.0),
            Self::Hann => {
                // verified output against reference implementation
                // See: FLAC__window_hann()

                let np =
                    f64::from(u16::try_from(window.len()).expect("window size too large")) - 1.0;

                window.iter_mut().zip(0u16..).for_each(|(w, n)| {
                    *w = 0.5 - 0.5 * (2.0 * PI * f64::from(n) / np).cos();
                });
            }
            Self::Tukey(p) => match p {
                // verified output against reference implementation
                // See: FLAC__window_tukey()
                ..=0.0 => {
                    window.fill(1.0);
                }
                1.0.. => {
                    Self::Hann.generate(window);
                }
                0.0..1.0 => {
                    match ((f64::from(*p) / 2.0 * window.len() as f64) as usize).checked_sub(1) {
                        Some(np) => match window.get_disjoint_mut([
                            0..np,
                            np..window.len() - np,
                            window.len() - np..window.len(),
                        ]) {
                            Ok([first, mid, last]) => {
                                // u16 is maximum block size
                                let np = u16::try_from(np).expect("window size too large");

                                for ((x, y), n) in
                                    first.iter_mut().zip(last.iter_mut().rev()).zip(0u16..)
                                {
                                    *x = 0.5 - 0.5 * (PI * f64::from(n) / f64::from(np)).cos();
                                    *y = *x;
                                }
                                mid.fill(1.0);
                            }
                            Err(_) => {
                                window.fill(1.0);
                            }
                        },
                        None => {
                            window.fill(1.0);
                        }
                    }
                }
                _ => {
                    Self::Tukey(0.5).generate(window);
                }
            },
        }
    }

    fn apply<'w>(
        &self,
        window: &mut Vec<f64>,
        cache: &'w mut Vec<f64>,
        samples: &[i32],
    ) -> &'w [f64] {
        if window.len() != samples.len() {
            // need to re-generate window to fit samples
            window.resize(samples.len(), 0.0);
            self.generate(window);
        }

        // window signal into cache and return cached slice
        cache.clear();
        cache.extend(samples.iter().zip(window).map(|(s, w)| f64::from(*s) * *w));
        cache.as_slice()
    }
}

impl Default for Window {
    fn default() -> Self {
        Self::Tukey(0.5)
    }
}

#[derive(Default)]
struct EncodingCaches {
    channels: Vec<ChannelCache>,
    correlated: CorrelationCache,
    // the frame being written
    frame: Vec<u8>,
}

#[derive(Default)]
struct CorrelationCache {
    // the average channel samples
    average_samples: Vec<i32>,
    // the difference channel samples
    difference_samples: Vec<i32>,

    left_cache: ChannelCache,
    right_cache: ChannelCache,
    average_cache: ChannelCache,
    difference_cache: ChannelCache,
}

#[derive(Default)]
struct ChannelCache {
    fixed: FixedCache,
    lpc: LpcCache,
    wasted: Vec<i32>,
}

/// The parts of a channel cache a subframe plan borrows
struct Planning<'c> {
    fixed: &'c mut FixedCache,
    lpc: &'c mut LpcCache,
    wasted: &'c mut Vec<i32>,
}

impl ChannelCache {
    /// What planning borrows
    fn planning(&mut self) -> Planning<'_> {
        let ChannelCache { fixed, lpc, wasted } = self;
        Planning { fixed, lpc, wasted }
    }
}

#[derive(Default)]
struct FixedCache {
    // FIXED subframe buffers, one per order 1-4
    fixed_buffers: [Vec<i32>; 4],
}

#[derive(Default)]
struct LpcCache {
    window: Vec<f64>,
    windowed: Vec<f64>,
    residuals: Vec<i32>,
}

/// A FLAC encoder
struct Encoder<W: std::io::Write + std::io::Seek> {
    // the writer we're outputting to
    writer: Counter<W>,
    // the stream's starting offset in the writer, in bytes
    start: u64,
    // various encoding options
    options: EncoderOptions,
    // various encoder caches
    caches: EncodingCaches,
    // our metadata blocks
    blocks: BlockList,
    // our stream's sample rate
    sample_rate: SampleRate<u32>,
    // the current frame number
    frame_number: FrameNumber,
    // the number of channel-independent samples written
    samples_written: u64,
    // all seekpoints
    seekpoints: Vec<EncoderSeekPoint>,
    // our running MD5 calculation
    md5: md5::Context,
    // whether the encoder has finalized the file
    finalized: Finalized,
}

impl<W: std::io::Write + std::io::Seek> Encoder<W> {
    const MAX_SAMPLES: u64 = 68_719_476_736;

    fn new(
        mut writer: W,
        options: Options,
        sample_rate: u32,
        bits_per_sample: SignedBitCount<32>,
        channels: u8,
        total_samples: Option<NonZero<u64>>,
    ) -> Result<Self, Error> {
        use crate::metadata::OptionalBlockType;

        let mut blocks = options.metadata;

        *blocks.streaminfo_mut() = Streaminfo {
            minimum_block_size: options.block_size,
            maximum_block_size: options.block_size,
            minimum_frame_size: None,
            maximum_frame_size: None,
            sample_rate: (0..1048576)
                .contains(&sample_rate)
                .then_some(sample_rate)
                .ok_or(Error::InvalidSampleRate)?,
            bits_per_sample,
            channels: (1..=8)
                .contains(&channels)
                .then_some(channels)
                .and_then(NonZero::new)
                .ok_or(Error::ExcessiveChannels)?,
            total_samples: match total_samples {
                None => None,
                total_samples @ Some(samples) => match samples.get() {
                    0..Self::MAX_SAMPLES => total_samples,
                    _ => return Err(Error::ExcessiveTotalSamples),
                },
            },
            md5: None,
        };

        // insert a dummy SeekTable to be populated later
        if let Some(total_samples) = total_samples
            && let Some(placeholders) = options.seektable_interval.map(|s| {
                s.filter(
                    sample_rate,
                    EncoderSeekPoint::placeholders(total_samples.get(), options.block_size),
                )
            })
        {
            use crate::metadata::SeekTable;

            blocks.insert(SeekTable {
                // placeholder points should always be contiguous
                points: placeholders
                    .take(SeekTable::MAX_POINTS)
                    .map(|p| p.into())
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap(),
            });
        }

        let start = writer.stream_position()?;

        // sort blocks to put more relevant items at the front
        blocks.sort_by(|block| match block {
            OptionalBlockType::VorbisComment => 0,
            OptionalBlockType::SeekTable => 1,
            OptionalBlockType::Picture => 2,
            OptionalBlockType::Application => 3,
            OptionalBlockType::Cuesheet => 4,
            OptionalBlockType::Padding => 5,
        });

        write_blocks(writer.by_ref(), blocks.blocks())?;

        Ok(Self {
            start,
            writer: Counter::new(writer),
            options: EncoderOptions {
                max_partition_order: options.max_partition_order,
                mid_side: options.mid_side,
                seektable_interval: options.seektable_interval,
                max_lpc_order: options.max_lpc_order,
                window: options.window,
                exhaustive_channel_correlation: options.exhaustive_channel_correlation,
                use_rice2: u32::from(bits_per_sample) > 16,
            },
            caches: EncodingCaches::default(),
            sample_rate: blocks
                .streaminfo()
                .sample_rate
                .try_into()
                .expect("invalid sample rate"),
            blocks,
            frame_number: FrameNumber::default(),
            samples_written: 0,
            seekpoints: Vec::new(),
            md5: md5::Context::new(),
            finalized: Finalized::NotFinalized,
        })
    }

    /// The encoder's channel count
    fn channel_count(&self) -> NonZero<u8> {
        self.blocks.streaminfo().channels
    }

    /// Encodes an audio frame of PCM samples
    ///
    /// Depending on the encoder's chosen block size,
    /// this may encode zero or more FLAC frames to disk.
    ///
    /// # Errors
    ///
    /// Returns an I/O error from the underlying stream,
    /// or if the frame's parameters are not a match
    /// for the encoder's.
    fn encode(&mut self, frame: &Frame) -> Result<(), Error> {
        // drop in a new seekpoint
        self.seekpoints.push(EncoderSeekPoint {
            sample_offset: self.samples_written,
            byte_offset: Some(self.writer.count),
            frame_samples: frame.pcm_frames() as u16,
        });

        // update running total of samples written
        self.samples_written += frame.pcm_frames() as u64;
        if let Some(total_samples) = self.blocks.streaminfo().total_samples
            && self.samples_written > total_samples.get()
        {
            return Err(Error::ExcessiveTotalSamples);
        }

        encode_frame(
            &self.options,
            &mut self.caches,
            &mut self.writer,
            self.blocks.streaminfo_mut(),
            &mut self.frame_number,
            self.sample_rate,
            frame.channels().collect(),
        )
    }

    fn finalize_inner(&mut self) -> Result<(), Error> {
        if matches!(std::mem::take(&mut self.finalized), Finalized::NotFinalized) {
            use crate::metadata::SeekTable;

            // update SEEKTABLE metadata block with final values
            if let Some(encoded_points) = self
                .options
                .seektable_interval
                .map(|s| s.filter(self.sample_rate.into(), self.seekpoints.iter().cloned()))
            {
                match self.blocks.get_pair_mut() {
                    (Some(SeekTable { points }), _) => {
                        // placeholder SEEKTABLE already in place,
                        // so no need to adjust PADDING to fit

                        // ensure points count is the same

                        let points_len = points.len();
                        points.clear();
                        points
                            .try_extend(
                                encoded_points
                                    .into_iter()
                                    .map(|p| p.into())
                                    .chain(std::iter::repeat(SeekPoint::Placeholder))
                                    .take(points_len),
                            )
                            .unwrap();
                    }
                    (None, Some(crate::metadata::Padding { size: padding_size })) => {
                        // no SEEKTABLE, but there is a PADDING block,
                        // so try to shrink PADDING to fit SEEKTABLE

                        use crate::metadata::MetadataBlock;

                        let seektable = SeekTable {
                            points: encoded_points
                                .map(|p| p.into())
                                .collect::<Vec<_>>()
                                .try_into()
                                .unwrap(),
                        };
                        if let Some(new_padding_size) = seektable
                            .total_size()
                            .and_then(|seektable_size| padding_size.checked_sub(seektable_size))
                        {
                            *padding_size = new_padding_size;
                            self.blocks.insert(seektable);
                        }
                    }
                    (None, None) => { /* no seektable or padding, so nothing to do */ }
                }
            }

            // verify or update final sample count
            match &mut self.blocks.streaminfo_mut().total_samples {
                Some(expected) => {
                    // ensure final sample count matches
                    if expected.get() != self.samples_written {
                        return Err(Error::SampleCountMismatch);
                    }
                }
                expected @ None => {
                    // update final sample count if possible
                    if self.samples_written < Self::MAX_SAMPLES {
                        *expected =
                            Some(NonZero::new(self.samples_written).ok_or(Error::NoSamples)?);
                    } else {
                        // TODO - should I just leave this blank
                        // if too many samples are written?
                        return Err(Error::ExcessiveTotalSamples);
                    }
                }
            }

            // update STREAMINFO MD5 sum
            self.blocks.streaminfo_mut().md5 = Some(self.md5.clone().finalize().0);

            // rewrite metadata blocks, relative to the beginning
            // of the stream
            let writer = self.writer.stream();
            writer.seek(std::io::SeekFrom::Start(self.start))?;
            write_blocks(writer.by_ref(), self.blocks.blocks())
        } else {
            Ok(())
        }
    }
}

impl<W: std::io::Write + std::io::Seek> Drop for Encoder<W> {
    fn drop(&mut self) {
        let _ = self.finalize_inner();
    }
}

// Unlike regular SeekPoints, which can have placeholder variants,
// these are always defined to be something.  A byte offset
// of None indicates a dummy encoder point
#[derive(Debug, Clone)]
struct EncoderSeekPoint {
    sample_offset: u64,
    byte_offset: Option<u64>,
    frame_samples: u16,
}

impl EncoderSeekPoint {
    // generates set of placeholder points
    fn placeholders(total_samples: u64, block_size: u16) -> impl Iterator<Item = EncoderSeekPoint> {
        (0..total_samples)
            .step_by(usize::from(block_size))
            .map(move |sample_offset| EncoderSeekPoint {
                sample_offset,
                byte_offset: None,
                frame_samples: u16::try_from(total_samples - sample_offset)
                    .map(|s| s.min(block_size))
                    .unwrap_or(block_size),
            })
    }

    // returns sample range of point
    fn range(&self) -> std::ops::Range<u64> {
        self.sample_offset..(self.sample_offset + u64::from(self.frame_samples))
    }
}

impl From<EncoderSeekPoint> for SeekPoint {
    fn from(p: EncoderSeekPoint) -> Self {
        match p.byte_offset {
            Some(byte_offset) => Self::Defined {
                sample_offset: p.sample_offset,
                byte_offset,
                frame_samples: p.frame_samples,
            },
            None => Self::Placeholder,
        }
    }
}

/// Given a FLAC stream, generates new seek table
///
/// Though encoders should add seek tables by default,
/// sometimes one isn't present.  This function takes
/// an existing FLAC file stream and generates a new
/// seek table suitable for adding to the file's metadata
/// via the [`crate::metadata::update`] function.
///
/// The stream should be rewound to the beginning of the file.
///
/// # Errors
///
/// Returns any error from the underlying stream.
///
/// # Example
/// ```
/// use flac_codec::{
///     encode::{FlacSampleWriter, Options, SeekTableInterval, generate_seektable},
///     metadata::{SeekTable, read_block},
/// };
/// use std::io::{Cursor, Seek};
///
/// let mut flac = Cursor::new(vec![]);  // a FLAC file in memory
///
/// // add a seekpoint every second
/// let options = Options::default().seektable_seconds(1);
///
/// let mut writer = FlacSampleWriter::new(
///     &mut flac,         // our wrapped writer
///     options,           // our seektable options
///     44100,             // sample rate
///     16,                // bits-per-sample
///     1,                 // channel count
///     Some(60 * 44100),  // one minute's worth of samples
/// ).unwrap();
///
/// // write one minute's worth of samples
/// writer.write(vec![0; 60 * 44100].as_slice()).unwrap();
///
/// // finalize writing file
/// assert!(writer.finalize().is_ok());
///
/// flac.rewind().unwrap();
///
/// // get existing seektable
/// let original_seektable = match read_block::<_, SeekTable>(&mut flac) {
///     Ok(Some(seektable)) => seektable,
///     _ => panic!("seektable not found"),
/// };
///
/// flac.rewind().unwrap();
///
/// // generate new seektable, also with seekpoints every second
/// let new_seektable = generate_seektable(
///     flac,
///     SeekTableInterval::Seconds(1.try_into().unwrap())
/// ).unwrap();
///
/// // ensure both seektables are identical
/// assert_eq!(original_seektable, new_seektable);
/// ```
pub fn generate_seektable<R: std::io::Read>(
    r: R,
    interval: SeekTableInterval,
) -> Result<crate::metadata::SeekTable, Error> {
    use crate::{
        metadata::{Metadata, SeekTable},
        stream::FrameIterator,
    };

    let iter = FrameIterator::new(r)?;
    let metadata_len = iter.metadata_len();
    let sample_rate = iter.sample_rate();
    let mut sample_offset = 0;

    iter.map(|r| {
        r.map(|(frame, offset)| EncoderSeekPoint {
            sample_offset,
            byte_offset: Some(offset - metadata_len),
            frame_samples: frame.header.block_size.into(),
        })
        .inspect(|p| {
            sample_offset += u64::from(p.frame_samples);
        })
    })
    .collect::<Result<Vec<_>, _>>()
    .map(|seekpoints| SeekTable {
        points: interval
            .filter(sample_rate, seekpoints)
            .take(SeekTable::MAX_POINTS)
            .map(|p| p.into())
            .collect::<Vec<_>>()
            .try_into()
            .unwrap(),
    })
}

/// Plans the subframes of a frame: the channel assignment, then one plan
/// per subframe in the order they are written
fn plan_frame<'c>(
    options: &EncoderOptions,
    caches: &'c mut Vec<ChannelCache>,
    correlated: &'c mut CorrelationCache,
    frame: &[&'c [i32]],
    bits_per_sample: SignedBitCount<32>,
) -> Result<(ChannelAssignment, ArrayVec<SubframePlan<'c>, MAX_CHANNELS>), Error> {
    match frame {
        [channel] => {
            caches.resize_with(1, ChannelCache::default);
            let plan = plan_subframe(
                options,
                caches[0].planning(),
                CorrelatedChannel::independent(bits_per_sample, channel),
            )?;
            Ok((
                ChannelAssignment::Independent(Independent::Mono),
                [plan].into_iter().collect(),
            ))
        }
        [left, right] if options.exhaustive_channel_correlation => {
            let Correlated {
                channel_assignment,
                channels,
            } = correlate_channels_exhaustive(options, correlated, [left, right], bits_per_sample)?;
            Ok((channel_assignment, channels.into_iter().collect()))
        }
        [left, right] => {
            let Correlated {
                channel_assignment,
                channels: [channel_0, channel_1],
            } = correlate_channels(options, correlated, [left, right], bits_per_sample);

            caches.resize_with(2, ChannelCache::default);
            let [cache_0, cache_1] = caches.get_disjoint_mut([0, 1]).unwrap();
            let (plan_0, plan_1) = try_join(
                || plan_subframe(options, cache_0.planning(), channel_0),
                || plan_subframe(options, cache_1.planning(), channel_1),
            )?;
            Ok((channel_assignment, [plan_0, plan_1].into_iter().collect()))
        }
        channels => {
            // non-stereo frames are always encoded independently
            caches.resize_with(channels.len(), ChannelCache::default);
            let plans = vec_map(
                caches.iter_mut().zip(channels).collect(),
                |(cache, channel)| {
                    plan_subframe(
                        options,
                        cache.planning(),
                        CorrelatedChannel::independent(bits_per_sample, channel),
                    )
                },
            )
            .into_iter()
            .collect::<Result<ArrayVec<_, MAX_CHANNELS>, Error>>()?;
            Ok((
                ChannelAssignment::Independent(
                    channels.len().try_into().expect("invalid channel count"),
                ),
                plans,
            ))
        }
    }
}

/// Writes a whole frame into `buf`, replacing what it held: the header
/// (written by `header`), the planned subframes, and the CRC-16
fn write_frame_bytes(
    buf: &mut Vec<u8>,
    header: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
    plans: &[SubframePlan<'_>],
) -> Result<(), Error> {
    use crate::crc::Crc16;
    use crate::frames::FrameWriter;

    buf.clear();
    header(buf)?;
    let mut writer = FrameWriter::new(buf);
    for plan in plans {
        let before = writer.bit_len();
        plan.write(&mut writer)?;
        debug_assert_eq!(
            writer.bit_len() - before,
            u64::from(plan.bits),
            "planned and written sizes differ"
        );
    }
    writer.byte_align()?;
    let buf = writer.finish();
    let crc16: u16 = Crc16::default().update_slice(buf).into();
    buf.extend_from_slice(&crc16.to_be_bytes());
    Ok(())
}

fn encode_frame<W>(
    options: &EncoderOptions,
    cache: &mut EncodingCaches,
    mut writer: W,
    streaminfo: &mut Streaminfo,
    frame_number: &mut FrameNumber,
    sample_rate: SampleRate<u32>,
    frame: ArrayVec<&[i32], MAX_CHANNELS>,
) -> Result<(), Error>
where
    W: std::io::Write,
{
    use crate::stream::FrameHeader;

    debug_assert!(!frame.is_empty());

    let EncodingCaches {
        channels: caches,
        correlated,
        frame: buf,
    } = cache;
    let (channel_assignment, plans) = plan_frame(
        options,
        caches,
        correlated,
        &frame,
        streaminfo.bits_per_sample,
    )?;
    write_frame_bytes(
        buf,
        |header| {
            FrameHeader {
                blocking_strategy: false,
                frame_number: *frame_number,
                block_size: (frame[0].len() as u16)
                    .try_into()
                    .expect("frame cannot be empty"),
                sample_rate,
                bits_per_sample: streaminfo.bits_per_sample.into(),
                channel_assignment,
            }
            .write(header, streaminfo)
        },
        &plans,
    )?;
    writer.write_all(buf)?;

    frame_number.try_increment()?;

    // update minimum and maximum frame size values
    if let s @ Some(size) = u32::try_from(buf.len())
        .ok()
        .filter(|size| *size < Streaminfo::MAX_FRAME_SIZE)
        .and_then(NonZero::new)
    {
        match &mut streaminfo.minimum_frame_size {
            Some(min_size) => {
                *min_size = size.min(*min_size);
            }
            min_size @ None => {
                *min_size = s;
            }
        }

        match &mut streaminfo.maximum_frame_size {
            Some(max_size) => {
                *max_size = size.max(*max_size);
            }
            max_size @ None => {
                *max_size = s;
            }
        }
    }

    Ok(())
}

struct Correlated<C> {
    channel_assignment: ChannelAssignment,
    channels: [C; 2],
}

struct CorrelatedChannel<'c> {
    samples: &'c [i32],
    bits_per_sample: SignedBitCount<32>,
    // whether all samples are known to be 0
    all_0: bool,
}

impl<'c> CorrelatedChannel<'c> {
    fn independent(bits_per_sample: SignedBitCount<32>, samples: &'c [i32]) -> Self {
        Self {
            all_0: samples.iter().all(|s| *s == 0),
            bits_per_sample,
            samples,
        }
    }
}

fn correlate_channels<'c>(
    options: &EncoderOptions,
    CorrelationCache {
        average_samples,
        difference_samples,
        ..
    }: &'c mut CorrelationCache,
    [left, right]: [&'c [i32]; 2],
    bits_per_sample: SignedBitCount<32>,
) -> Correlated<CorrelatedChannel<'c>> {
    match bits_per_sample.checked_add::<32>(1) {
        Some(difference_bits_per_sample) if options.mid_side => {
            let mut left_abs_sum = 0;
            let mut right_abs_sum = 0;
            let mut mid_abs_sum = 0;
            let mut side_abs_sum = 0;

            join(
                || {
                    average_samples.clear();
                    average_samples.extend(
                        left.iter()
                            .inspect(|s| left_abs_sum += u64::from(s.unsigned_abs()))
                            .zip(
                                right
                                    .iter()
                                    .inspect(|s| right_abs_sum += u64::from(s.unsigned_abs())),
                            )
                            .map(|(l, r)| (l + r) >> 1)
                            .inspect(|s| mid_abs_sum += u64::from(s.unsigned_abs())),
                    );
                },
                || {
                    difference_samples.clear();
                    difference_samples.extend(
                        left.iter()
                            .zip(right)
                            .map(|(l, r)| l - r)
                            .inspect(|s| side_abs_sum += u64::from(s.unsigned_abs())),
                    );
                },
            );

            match [
                (
                    ChannelAssignment::Independent(Independent::Stereo),
                    left_abs_sum + right_abs_sum,
                ),
                (ChannelAssignment::LeftSide, left_abs_sum + side_abs_sum),
                (ChannelAssignment::SideRight, side_abs_sum + right_abs_sum),
                (ChannelAssignment::MidSide, mid_abs_sum + side_abs_sum),
            ]
            .into_iter()
            .min_by_key(|(_, total)| *total)
            .unwrap()
            .0
            {
                channel_assignment @ ChannelAssignment::LeftSide => Correlated {
                    channel_assignment,
                    channels: [
                        CorrelatedChannel {
                            samples: left,
                            bits_per_sample,
                            all_0: left_abs_sum == 0,
                        },
                        CorrelatedChannel {
                            samples: difference_samples,
                            bits_per_sample: difference_bits_per_sample,
                            all_0: side_abs_sum == 0,
                        },
                    ],
                },
                channel_assignment @ ChannelAssignment::SideRight => Correlated {
                    channel_assignment,
                    channels: [
                        CorrelatedChannel {
                            samples: difference_samples,
                            bits_per_sample: difference_bits_per_sample,
                            all_0: side_abs_sum == 0,
                        },
                        CorrelatedChannel {
                            samples: right,
                            bits_per_sample,
                            all_0: right_abs_sum == 0,
                        },
                    ],
                },
                channel_assignment @ ChannelAssignment::MidSide => Correlated {
                    channel_assignment,
                    channels: [
                        CorrelatedChannel {
                            samples: average_samples,
                            bits_per_sample,
                            all_0: mid_abs_sum == 0,
                        },
                        CorrelatedChannel {
                            samples: difference_samples,
                            bits_per_sample: difference_bits_per_sample,
                            all_0: side_abs_sum == 0,
                        },
                    ],
                },
                channel_assignment @ ChannelAssignment::Independent(_) => Correlated {
                    channel_assignment,
                    channels: [
                        CorrelatedChannel {
                            samples: left,
                            bits_per_sample,
                            all_0: left_abs_sum == 0,
                        },
                        CorrelatedChannel {
                            samples: right,
                            bits_per_sample,
                            all_0: right_abs_sum == 0,
                        },
                    ],
                },
            }
        }
        Some(difference_bits_per_sample) => {
            let mut left_abs_sum = 0;
            let mut right_abs_sum = 0;
            let mut side_abs_sum = 0;

            difference_samples.clear();
            difference_samples.extend(
                left.iter()
                    .inspect(|s| left_abs_sum += u64::from(s.unsigned_abs()))
                    .zip(
                        right
                            .iter()
                            .inspect(|s| right_abs_sum += u64::from(s.unsigned_abs())),
                    )
                    .map(|(l, r)| l - r)
                    .inspect(|s| side_abs_sum += u64::from(s.unsigned_abs())),
            );

            match [
                (ChannelAssignment::LeftSide, left_abs_sum + side_abs_sum),
                (ChannelAssignment::SideRight, side_abs_sum + right_abs_sum),
                (
                    ChannelAssignment::Independent(Independent::Stereo),
                    left_abs_sum + right_abs_sum,
                ),
            ]
            .into_iter()
            .min_by_key(|(_, total)| *total)
            .unwrap()
            .0
            {
                channel_assignment @ ChannelAssignment::LeftSide => Correlated {
                    channel_assignment,
                    channels: [
                        CorrelatedChannel {
                            samples: left,
                            bits_per_sample,
                            all_0: left_abs_sum == 0,
                        },
                        CorrelatedChannel {
                            samples: difference_samples,
                            bits_per_sample: difference_bits_per_sample,
                            all_0: side_abs_sum == 0,
                        },
                    ],
                },
                channel_assignment @ ChannelAssignment::SideRight => Correlated {
                    channel_assignment,
                    channels: [
                        CorrelatedChannel {
                            samples: difference_samples,
                            bits_per_sample: difference_bits_per_sample,
                            all_0: side_abs_sum == 0,
                        },
                        CorrelatedChannel {
                            samples: right,
                            bits_per_sample,
                            all_0: right_abs_sum == 0,
                        },
                    ],
                },
                ChannelAssignment::MidSide => unreachable!(),
                channel_assignment @ ChannelAssignment::Independent(_) => Correlated {
                    channel_assignment,
                    channels: [
                        CorrelatedChannel {
                            samples: left,
                            bits_per_sample,
                            all_0: left_abs_sum == 0,
                        },
                        CorrelatedChannel {
                            samples: right,
                            bits_per_sample,
                            all_0: right_abs_sum == 0,
                        },
                    ],
                },
            }
        }
        None => {
            // 32 bps stream, so forego difference channel
            // and encode them both indepedently

            Correlated {
                channel_assignment: ChannelAssignment::Independent(Independent::Stereo),
                channels: [
                    CorrelatedChannel::independent(bits_per_sample, left),
                    CorrelatedChannel::independent(bits_per_sample, right),
                ],
            }
        }
    }
}

fn correlate_channels_exhaustive<'c>(
    options: &EncoderOptions,
    CorrelationCache {
        average_samples,
        difference_samples,
        left_cache,
        right_cache,
        average_cache,
        difference_cache,
        ..
    }: &'c mut CorrelationCache,
    [left, right]: [&'c [i32]; 2],
    bits_per_sample: SignedBitCount<32>,
) -> Result<Correlated<SubframePlan<'c>>, Error> {
    // every candidate channel is planned; only the two chosen are written later
    let left_planning = left_cache.planning();
    let right_planning = right_cache.planning();
    let (left_plan, right_plan) = try_join(
        || {
            plan_subframe(
                options,
                left_planning,
                CorrelatedChannel {
                    samples: left,
                    bits_per_sample,
                    all_0: false,
                },
            )
        },
        || {
            plan_subframe(
                options,
                right_planning,
                CorrelatedChannel {
                    samples: right,
                    bits_per_sample,
                    all_0: false,
                },
            )
        },
    )?;

    let difference = move |difference_samples: &'c mut Vec<i32>,
                           planning: Planning<'c>,
                           bits_per_sample: SignedBitCount<32>| {
        difference_samples.clear();
        difference_samples.extend(left.iter().zip(right).map(|(l, r)| l - r));
        let difference_samples: &'c [i32] = difference_samples;
        plan_subframe(
            options,
            planning,
            CorrelatedChannel {
                samples: difference_samples,
                bits_per_sample,
                all_0: false,
            },
        )
    };

    match bits_per_sample.checked_add::<32>(1) {
        Some(difference_bits_per_sample) if options.mid_side => {
            let average_planning = average_cache.planning();
            let difference_planning = difference_cache.planning();
            let (average_plan, difference_plan) = try_join(
                move || {
                    average_samples.clear();
                    average_samples
                        .extend(left.iter().zip(right.iter()).map(|(l, r)| (l + r) >> 1));
                    let average_samples: &'c [i32] = average_samples;
                    plan_subframe(
                        options,
                        average_planning,
                        CorrelatedChannel {
                            samples: average_samples,
                            bits_per_sample,
                            all_0: false,
                        },
                    )
                },
                move || {
                    difference(
                        difference_samples,
                        difference_planning,
                        difference_bits_per_sample,
                    )
                },
            )?;

            match [
                (
                    ChannelAssignment::Independent(Independent::Stereo),
                    left_plan.bits + right_plan.bits,
                ),
                (
                    ChannelAssignment::LeftSide,
                    left_plan.bits + difference_plan.bits,
                ),
                (
                    ChannelAssignment::SideRight,
                    difference_plan.bits + right_plan.bits,
                ),
                (
                    ChannelAssignment::MidSide,
                    average_plan.bits + difference_plan.bits,
                ),
            ]
            .into_iter()
            .min_by_key(|(_, total)| *total)
            .unwrap()
            .0
            {
                channel_assignment @ ChannelAssignment::LeftSide => Ok(Correlated {
                    channel_assignment,
                    channels: [left_plan, difference_plan],
                }),
                channel_assignment @ ChannelAssignment::SideRight => Ok(Correlated {
                    channel_assignment,
                    channels: [difference_plan, right_plan],
                }),
                channel_assignment @ ChannelAssignment::MidSide => Ok(Correlated {
                    channel_assignment,
                    channels: [average_plan, difference_plan],
                }),
                channel_assignment @ ChannelAssignment::Independent(_) => Ok(Correlated {
                    channel_assignment,
                    channels: [left_plan, right_plan],
                }),
            }
        }
        Some(difference_bits_per_sample) => {
            let difference_planning = difference_cache.planning();
            let difference_plan = difference(
                difference_samples,
                difference_planning,
                difference_bits_per_sample,
            )?;

            match [
                (
                    ChannelAssignment::Independent(Independent::Stereo),
                    left_plan.bits + right_plan.bits,
                ),
                (
                    ChannelAssignment::LeftSide,
                    left_plan.bits + difference_plan.bits,
                ),
                (
                    ChannelAssignment::SideRight,
                    difference_plan.bits + right_plan.bits,
                ),
            ]
            .into_iter()
            .min_by_key(|(_, total)| *total)
            .unwrap()
            .0
            {
                channel_assignment @ ChannelAssignment::LeftSide => Ok(Correlated {
                    channel_assignment,
                    channels: [left_plan, difference_plan],
                }),
                channel_assignment @ ChannelAssignment::SideRight => Ok(Correlated {
                    channel_assignment,
                    channels: [difference_plan, right_plan],
                }),
                ChannelAssignment::MidSide => unreachable!(),
                channel_assignment @ ChannelAssignment::Independent(_) => Ok(Correlated {
                    channel_assignment,
                    channels: [left_plan, right_plan],
                }),
            }
        }
        None => {
            // 32 bps stream, so forego difference channel
            // and encode them both indepedently

            Ok(Correlated {
                channel_assignment: ChannelAssignment::Independent(Independent::Stereo),
                channels: [left_plan, right_plan],
            })
        }
    }
}

/// A channel's subframe, chosen but not yet written, and its exact size
struct SubframePlan<'c> {
    kind: SubframeKind<'c>,
    bits: u32,
}

enum SubframeKind<'c> {
    Constant {
        sample: i32,
        bits_per_sample: SignedBitCount<32>,
    },
    Verbatim {
        samples: &'c [i32],
        bits_per_sample: SignedBitCount<32>,
        wasted_bps: u32,
    },
    Fixed {
        fixed: (u8, &'c [i32], &'c [i32]),
        residuals: CodingMethod<'c>,
        bits_per_sample: SignedBitCount<32>,
        wasted_bps: u32,
    },
    Lpc {
        lpc: LpcSubframeParameters<'c, 'c>,
        residuals: CodingMethod<'c>,
        bits_per_sample: SignedBitCount<32>,
        wasted_bps: u32,
    },
}

impl SubframePlan<'_> {
    fn write<W: WriteRice>(&self, writer: &mut W) -> Result<(), Error> {
        match &self.kind {
            SubframeKind::Constant {
                sample,
                bits_per_sample,
            } => encode_constant_subframe(writer, *sample, *bits_per_sample, 0),
            SubframeKind::Verbatim {
                samples,
                bits_per_sample,
                wasted_bps,
            } => encode_verbatim_subframe(writer, samples, *bits_per_sample, *wasted_bps),
            SubframeKind::Fixed {
                fixed,
                residuals,
                bits_per_sample,
                wasted_bps,
            } => {
                write_fixed_head(writer, *fixed, *bits_per_sample, *wasted_bps)?;
                residuals.write(writer)
            }
            SubframeKind::Lpc {
                lpc,
                residuals,
                bits_per_sample,
                wasted_bps,
            } => {
                write_lpc_head(writer, lpc, *bits_per_sample, *wasted_bps)?;
                residuals.write(writer)
            }
        }
    }
}

/// The bits `write` writes, failing as a 32-bit bit counter would
fn count_bits(
    write: impl FnOnce(&mut BitsWritten<u32>) -> Result<(), Error>,
) -> Result<u32, Error> {
    let mut bits = BitsWritten::<u32>::new();
    write(&mut bits)?;
    Ok(bits.written())
}

/// A subframe head's bits and its residuals' bits, failing as a 32-bit bit
/// counter would
fn with_residual_bits(head: u32, residuals: &CodingMethod<'_>) -> Result<u32, Error> {
    residuals
        .bits()
        .and_then(|bits| u32::try_from(u64::from(head) + bits).ok())
        .ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "excessive value for bits written",
            ))
        })
}

/// Chooses a channel's subframe: CONSTANT, VERBATIM, FIXED or LPC,
/// whichever is smallest, without writing it
fn plan_subframe<'c>(
    options: &EncoderOptions,
    Planning {
        fixed: fixed_cache,
        lpc: lpc_cache,
        wasted,
    }: Planning<'c>,
    CorrelatedChannel {
        samples: channel,
        bits_per_sample,
        all_0,
    }: CorrelatedChannel<'c>,
) -> Result<SubframePlan<'c>, Error> {
    const WASTED_MAX: NonZero<u32> = NonZero::new(32).unwrap();

    debug_assert!(!channel.is_empty());

    let constant = |sample: i32| -> Result<SubframePlan<'c>, Error> {
        Ok(SubframePlan {
            bits: count_bits(|w| encode_constant_subframe(w, sample, bits_per_sample, 0))?,
            kind: SubframeKind::Constant {
                sample,
                bits_per_sample,
            },
        })
    };
    let verbatim = |samples: &'c [i32],
                    bits_per_sample: SignedBitCount<32>,
                    wasted_bps: u32|
     -> Result<SubframePlan<'c>, Error> {
        Ok(SubframePlan {
            bits: count_bits(|w| {
                encode_verbatim_subframe(w, samples, bits_per_sample, wasted_bps)
            })?,
            kind: SubframeKind::Verbatim {
                samples,
                bits_per_sample,
                wasted_bps,
            },
        })
    };

    if all_0 {
        // all samples are 0
        return constant(channel[0]);
    }

    // determine any wasted bits
    let (channel, bits_per_sample, wasted_bps): (&'c [i32], _, _) =
        match channel.iter().try_fold(WASTED_MAX, |acc, sample| {
            NonZero::new(sample.trailing_zeros()).map(|sample| sample.min(acc))
        }) {
            None => (channel, bits_per_sample, 0),
            Some(WASTED_MAX) => return constant(channel[0]),
            Some(wasted_bps) => {
                let wasted_bps = wasted_bps.get();
                wasted.clear();
                wasted.extend(channel.iter().map(|sample| sample >> wasted_bps));
                (
                    wasted.as_slice(),
                    bits_per_sample.checked_sub(wasted_bps).unwrap(),
                    wasted_bps,
                )
            }
        };

    let fixed = move |fixed_cache: &'c mut FixedCache| -> Result<SubframePlan<'c>, Error> {
        let fixed = fixed_parameters(fixed_cache, channel);
        let residuals = plan_residuals(options, fixed.0.into(), fixed.2);
        let head = count_bits(|w| write_fixed_head(w, fixed, bits_per_sample, wasted_bps))?;
        Ok(SubframePlan {
            bits: with_residual_bits(head, &residuals)?,
            kind: SubframeKind::Fixed {
                fixed,
                residuals,
                bits_per_sample,
                wasted_bps,
            },
        })
    };

    let best = match options.max_lpc_order {
        Some(max_lpc_order) => {
            let lpc = move |lpc_cache: &'c mut LpcCache| -> Result<SubframePlan<'c>, Error> {
                let lpc = LpcSubframeParameters::best(
                    options,
                    bits_per_sample,
                    max_lpc_order,
                    lpc_cache,
                    channel,
                )?;
                let residuals =
                    plan_residuals(options, lpc.parameters.order.get().into(), lpc.residuals);
                let head = count_bits(|w| write_lpc_head(w, &lpc, bits_per_sample, wasted_bps))?;
                Ok(SubframePlan {
                    bits: with_residual_bits(head, &residuals)?,
                    kind: SubframeKind::Lpc {
                        lpc,
                        residuals,
                        bits_per_sample,
                        wasted_bps,
                    },
                })
            };

            // the smaller candidate, FIXED when they are the same size
            match join(|| fixed(fixed_cache), || lpc(lpc_cache)) {
                (Ok(fixed), Ok(lpc)) if lpc.bits < fixed.bits => lpc,
                (Ok(fixed), _) => fixed,
                (Err(_), Ok(lpc)) => lpc,
                (Err(_), Err(_)) => return verbatim(channel, bits_per_sample, wasted_bps),
            }
        }
        _ => match fixed(fixed_cache) {
            Ok(fixed) => fixed,
            Err(_) => return verbatim(channel, bits_per_sample, wasted_bps),
        },
    };

    let verbatim_len = channel.len() as u32 * u32::from(bits_per_sample);

    if best.bits < verbatim_len {
        Ok(best)
    } else {
        verbatim(channel, bits_per_sample, wasted_bps)
    }
}

fn encode_constant_subframe<W: BitWrite>(
    writer: &mut W,
    sample: i32,
    bits_per_sample: SignedBitCount<32>,
    wasted_bps: u32,
) -> Result<(), Error> {
    use crate::stream::{SubframeHeader, SubframeHeaderType};

    writer.build(&SubframeHeader {
        type_: SubframeHeaderType::Constant,
        wasted_bps,
    })?;

    writer
        .write_signed_counted(bits_per_sample, sample)
        .map_err(Error::Io)
}

fn encode_verbatim_subframe<W: BitWrite>(
    writer: &mut W,
    channel: &[i32],
    bits_per_sample: SignedBitCount<32>,
    wasted_bps: u32,
) -> Result<(), Error> {
    use crate::stream::{SubframeHeader, SubframeHeaderType};

    writer.build(&SubframeHeader {
        type_: SubframeHeaderType::Verbatim,
        wasted_bps,
    })?;

    channel
        .iter()
        .try_for_each(|i| writer.write_signed_counted(bits_per_sample, *i))?;

    Ok(())
}

/// The FIXED order for `channel` with the smallest residuals:
/// the order, its warm-up samples and its residuals
fn fixed_parameters<'c>(
    FixedCache {
        fixed_buffers: buffers,
    }: &'c mut FixedCache,
    channel: &'c [i32],
) -> (u8, &'c [i32], &'c [i32]) {
    // calculate residuals for FIXED subframe orders 0-4
    // (or fewer, if we don't have enough samples)
    {
        let mut fixed_orders = ArrayVec::<&[i32], 5>::new();
        fixed_orders.push(channel);

        // accumulate a set of FIXED diffs
        'outer: for buf in buffers.iter_mut() {
            let prev_order = fixed_orders.last().unwrap();
            match prev_order.split_at_checked(1) {
                Some((_, r)) => {
                    buf.clear();
                    // every difference, then whether one overflowed, which stops
                    // at the same order as checking each one
                    let mut overflow = false;
                    buf.extend(r.iter().zip(*prev_order).map(|(n, p)| {
                        let difference = i64::from(*n) - i64::from(*p);
                        overflow |= difference != i64::from(difference as i32);
                        difference as i32
                    }));
                    if overflow {
                        break 'outer;
                    }
                    if buf.is_empty() {
                        break;
                    } else {
                        fixed_orders.push(buf.as_slice());
                    }
                }
                None => break,
            }
        }

        let min_fixed = fixed_orders.last().unwrap().len();

        // choose diff with the smallest abs sum
        fixed_orders
            .into_iter()
            .enumerate()
            .min_by_key(|(_, residuals)| {
                residuals[(residuals.len() - min_fixed)..]
                    .iter()
                    .map(|r| u64::from(r.unsigned_abs()))
                    .sum::<u64>()
            })
            .map(|(order, residuals)| (order as u8, &channel[0..order], residuals))
            .unwrap()
    }
}

#[test]
fn fixed_parameters_match_checking_each_difference() {
    // the previous algorithm: stop at the first difference that overflows
    fn previous(channel: &[i32]) -> (u8, Vec<i32>) {
        let mut orders: Vec<Vec<i32>> = vec![channel.to_vec()];
        'outer: for _ in 0..4 {
            let prev = orders.last().unwrap().clone();
            if prev.len() < 2 {
                break;
            }
            let mut next = vec![];
            for (n, p) in prev[1..].iter().zip(&prev) {
                match n.checked_sub(*p) {
                    Some(v) => next.push(v),
                    None => break 'outer,
                }
            }
            orders.push(next);
        }
        let min = orders.last().unwrap().len();
        let (order, residuals) = orders
            .into_iter()
            .enumerate()
            .min_by_key(|(_, r)| {
                r[r.len() - min..]
                    .iter()
                    .map(|v| u64::from(v.unsigned_abs()))
                    .sum::<u64>()
            })
            .unwrap();
        (order as u8, residuals)
    }

    let mut seed = 0x1357_9BDF_2468_ACE0u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..400 {
        // full-scale 32-bit values overflow their differences, smaller ones do not
        let bits = [8, 16, 24, 30, 32][round % 5];
        let len = 1 + (next() % 300) as usize;
        let channel: Vec<i32> = (0..len).map(|_| (next() as i32) >> (32 - bits)).collect();
        let mut cache = FixedCache::default();
        let (order, warm_up, residuals) = fixed_parameters(&mut cache, &channel);
        let (expected_order, expected_residuals) = previous(&channel);
        assert_eq!(order, expected_order, "round {round}");
        assert_eq!(warm_up, &channel[..usize::from(order)]);
        assert_eq!(residuals, expected_residuals.as_slice(), "round {round}");
    }
}

/// Writes a FIXED subframe's header and warm-up samples
fn write_fixed_head<W: BitWrite>(
    writer: &mut W,
    (order, warm_up, _): (u8, &[i32], &[i32]),
    bits_per_sample: SignedBitCount<32>,
    wasted_bps: u32,
) -> Result<(), Error> {
    use crate::stream::{SubframeHeader, SubframeHeaderType};

    writer.build(&SubframeHeader {
        type_: SubframeHeaderType::Fixed { order },
        wasted_bps,
    })?;

    warm_up
        .iter()
        .try_for_each(|sample: &i32| writer.write_signed_counted(bits_per_sample, *sample))?;

    Ok(())
}

/// Writes an LPC subframe's header, warm-up samples and coefficients
fn write_lpc_head<W: BitWrite>(
    writer: &mut W,
    LpcSubframeParameters {
        warm_up,
        parameters:
            LpcParameters {
                order,
                precision,
                shift,
                coefficients,
            },
        ..
    }: &LpcSubframeParameters,
    bits_per_sample: SignedBitCount<32>,
    wasted_bps: u32,
) -> Result<(), Error> {
    use crate::stream::{SubframeHeader, SubframeHeaderType};

    writer.build(&SubframeHeader {
        type_: SubframeHeaderType::Lpc { order: *order },
        wasted_bps,
    })?;

    for sample in *warm_up {
        writer.write_signed_counted(bits_per_sample, *sample)?;
    }

    writer.write_count::<0b1111>(
        precision
            .count()
            .checked_sub(1)
            .ok_or(Error::InvalidQlpPrecision)?,
    )?;

    writer.write::<5, i32>(*shift as i32)?;

    for coeff in coefficients {
        writer.write_signed_counted(*precision, *coeff)?;
    }

    Ok(())
}

struct LpcSubframeParameters<'w, 'r> {
    parameters: LpcParameters,
    warm_up: &'w [i32],
    residuals: &'r [i32],
}

impl<'w, 'r> LpcSubframeParameters<'w, 'r> {
    fn best(
        options: &EncoderOptions,
        bits_per_sample: SignedBitCount<32>,
        max_lpc_order: NonZero<u8>,
        LpcCache {
            residuals,
            window,
            windowed,
        }: &'r mut LpcCache,
        channel: &'w [i32],
    ) -> Result<Self, Error> {
        let parameters = LpcParameters::best(
            options,
            bits_per_sample,
            max_lpc_order,
            window,
            windowed,
            channel,
        )?;

        Self::encode_residuals(&parameters, channel, residuals)
            .map(|(warm_up, residuals)| Self {
                warm_up,
                residuals,
                parameters,
            })
            .map_err(|ResidualOverflow| Error::ResidualOverflow)
    }

    fn encode_residuals(
        parameters: &LpcParameters,
        channel: &'w [i32],
        residuals: &'r mut Vec<i32>,
    ) -> Result<(&'w [i32], &'r [i32]), ResidualOverflow> {
        residuals.clear();

        // fixed-size loops for the usual orders, which the compiler unrolls
        // and vectorizes; the same arithmetic as the general loop below
        macro_rules! orders {
            ($($n:literal)*) => {
                match parameters.coefficients.len() {
                    $($n => {
                        if let Ok(coefficients) =
                            <&[i32; $n]>::try_from(parameters.coefficients.as_slice())
                        {
                            lpc_residuals(coefficients, parameters.shift, channel, residuals)?;
                            return Ok((&channel[0..$n], residuals.as_slice()));
                        }
                    })*
                    _ => {}
                }
            };
        }
        if usize::from(parameters.order.get()) == parameters.coefficients.len() {
            orders!(1 2 3 4 5 6 7 8 9 10 11 12);
        }

        for split in usize::from(parameters.order.get())..channel.len() {
            let (previous, current) = channel.split_at(split);

            residuals.push(
                current[0]
                    .checked_sub(
                        (previous
                            .iter()
                            .rev()
                            .zip(&parameters.coefficients)
                            .map(|(x, y)| *x as i64 * *y as i64)
                            .sum::<i64>()
                            >> parameters.shift) as i32,
                    )
                    .ok_or(ResidualOverflow)?,
            );
        }

        Ok((
            &channel[0..parameters.order.get().into()],
            residuals.as_slice(),
        ))
    }
}

/// LPC residuals for exactly `N` coefficients: each sample less its
/// prediction, an error if one does not fit in 32 bits
#[inline]
fn lpc_residuals<const N: usize>(
    coefficients: &[i32; N],
    shift: u32,
    channel: &[i32],
    residuals: &mut Vec<i32>,
) -> Result<(), ResidualOverflow> {
    let mut overflow = false;
    residuals.extend(channel.windows(N + 1).map(|window| {
        let (history, current) = window.split_at(N);
        let prediction = history
            .iter()
            .rev()
            .zip(coefficients)
            .map(|(x, y)| i64::from(*x) * i64::from(*y))
            .sum::<i64>()
            >> shift;
        // as `i32::checked_sub` in the general loop, but without stopping
        let residual = i64::from(current[0]) - i64::from(prediction as i32);
        overflow |= residual != i64::from(residual as i32);
        residual as i32
    }));
    match overflow {
        false => Ok(()),
        true => Err(ResidualOverflow),
    }
}

#[test]
fn lpc_residuals_by_order_match_the_general_loop() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for order in 1..=12usize {
        for round in 0..60 {
            // large samples and coefficients, so that some residuals overflow
            let bits = [8, 16, 24, 31][round % 4];
            let coefficients: Vec<i32> = (0..order).map(|_| (next() as i16 >> 1).into()).collect();
            let shift = (next() % 16) as u32;
            let channel: Vec<i32> = (0..200).map(|_| (next() as i32) >> (32 - bits)).collect();

            let general: Result<Vec<i32>, ()> = (order..channel.len())
                .map(|split| {
                    let (previous, current) = channel.split_at(split);
                    current[0]
                        .checked_sub(
                            (previous
                                .iter()
                                .rev()
                                .zip(&coefficients)
                                .map(|(x, y)| *x as i64 * *y as i64)
                                .sum::<i64>()
                                >> shift) as i32,
                        )
                        .ok_or(())
                })
                .collect();

            let parameters = LpcParameters {
                order: NonZero::new(order as u8).unwrap(),
                precision: SignedBitCount::new::<15>(),
                shift,
                coefficients: coefficients.iter().copied().collect(),
            };
            let mut residuals = vec![];
            let fast =
                LpcSubframeParameters::encode_residuals(&parameters, &channel, &mut residuals)
                    .map(|(warm_up, residuals)| {
                        assert_eq!(warm_up, &channel[..order]);
                        residuals.to_vec()
                    })
                    .map_err(|ResidualOverflow| ());
            assert_eq!(fast, general, "order {order}, {bits} bits");
        }
    }
}

#[derive(Debug)]
struct ResidualOverflow;

impl From<ResidualOverflow> for Error {
    #[inline]
    fn from(_: ResidualOverflow) -> Self {
        Error::ResidualOverflow
    }
}

#[test]
fn test_residual_encoding_1() {
    let samples = [
        0, 16, 31, 44, 54, 61, 64, 63, 58, 49, 38, 24, 8, -8, -24, -38, -49, -58, -63, -64, -61,
        -54, -44, -31, -16,
    ];

    let expected_residuals = [
        2, 2, 2, 3, 3, 3, 2, 2, 3, 0, 0, 0, -1, -1, -1, -3, -2, -2, -2, -1, -1, 0, 0,
    ];

    let mut actual_residuals = Vec::with_capacity(expected_residuals.len());

    let (warm_up, residuals) = LpcSubframeParameters::encode_residuals(
        &LpcParameters {
            order: NonZero::new(2).unwrap(),
            precision: SignedBitCount::new::<7>(),
            shift: 5,
            coefficients: arrayvec![59, -30],
        },
        &samples,
        &mut actual_residuals,
    )
    .unwrap();

    assert_eq!(warm_up, &samples[0..2]);
    assert_eq!(residuals, &expected_residuals);
}

#[test]
fn test_residual_encoding_2() {
    let samples = [
        64, 62, 56, 47, 34, 20, 4, -12, -27, -41, -52, -60, -63, -63, -60, -52, -41, -27, -12, 4,
        20, 34, 47, 56, 62,
    ];

    let expected_residuals = [
        2, 2, 0, 1, -1, -1, -1, -2, -2, -2, -1, -3, -2, 0, -1, 1, 0, 2, 2, 2, 4, 2, 4,
    ];

    let mut actual_residuals = Vec::with_capacity(expected_residuals.len());

    let (warm_up, residuals) = LpcSubframeParameters::encode_residuals(
        &LpcParameters {
            order: NonZero::new(2).unwrap(),
            precision: SignedBitCount::new::<7>(),
            shift: 5,
            coefficients: arrayvec![58, -29],
        },
        &samples,
        &mut actual_residuals,
    )
    .unwrap();

    assert_eq!(warm_up, &samples[0..2]);
    assert_eq!(residuals, &expected_residuals);
}

#[derive(Debug)]
struct LpcParameters {
    order: NonZero<u8>,
    precision: SignedBitCount<15>,
    shift: u32,
    coefficients: ArrayVec<i32, MAX_LPC_COEFFS>,
}

// There isn't any particular *best* way to determine
// the ideal LPC subframe parameters (though there are
// some worst ways, like choosing them at random).
// Even the reference implementation has changed its
// defaults over time.  So long as the subframe's residuals
// are calculated correctly, decoders don't care one way or another.
//
// I'll try to use an approach similar to the reference implementation's.

impl LpcParameters {
    fn best(
        options: &EncoderOptions,
        bits_per_sample: SignedBitCount<32>,
        max_lpc_order: NonZero<u8>,
        window: &mut Vec<f64>,
        windowed: &mut Vec<f64>,
        channel: &[i32],
    ) -> Result<Self, Error> {
        if channel.len() <= usize::from(max_lpc_order.get()) {
            // not enough samples in channel to calculate LPC parameters
            return Err(Error::InsufficientLpcSamples);
        }

        let precision = match channel.len() {
            // this shouldn't be possible
            0 => panic!("at least one sample required in channel"),
            1..=192 => SignedBitCount::new::<7>(),
            193..=384 => SignedBitCount::new::<8>(),
            385..=576 => SignedBitCount::new::<9>(),
            577..=1152 => SignedBitCount::new::<10>(),
            1153..=2304 => SignedBitCount::new::<11>(),
            2305..=4608 => SignedBitCount::new::<12>(),
            4609.. => SignedBitCount::new::<13>(),
        };

        let (order, lp_coeffs) = compute_best_order(
            bits_per_sample,
            precision,
            channel
                .len()
                .try_into()
                // this shouldn't be possible
                .expect("excessive samples for subframe"),
            lp_coefficients(autocorrelate(
                options.window.apply(window, windowed, channel),
                max_lpc_order,
            )),
        )?;

        Self::quantize(order, lp_coeffs, precision)
    }

    fn quantize(
        order: NonZero<u8>,
        coeffs: ArrayVec<f64, MAX_LPC_COEFFS>,
        precision: SignedBitCount<15>,
    ) -> Result<Self, Error> {
        const MAX_SHIFT: i32 = (1 << 4) - 1;
        const MIN_SHIFT: i32 = -(1 << 4);

        // verified output against reference implementation
        // See: FLAC__lpc_quantize_coefficients

        debug_assert!(coeffs.len() == usize::from(order.get()));

        let max_coeff = (1 << (u32::from(precision) - 1)) - 1;
        let min_coeff = -(1 << (u32::from(precision) - 1));

        let l = coeffs
            .iter()
            .map(|c| c.abs())
            .max_by(|x, y| x.total_cmp(y))
            // f64.log2() gives unfortunate results when <= 0.0
            .filter(|l| *l > 0.0)
            .ok_or(Error::ZeroLpCoefficients)?;

        let mut error = 0.0;

        match ((u32::from(precision) - 1) as i32 - ((l.log2().floor()) as i32) - 1).min(MAX_SHIFT) {
            shift @ 0.. => {
                // normal, positive shift case
                let shift = shift as u32;

                Ok(Self {
                    order,
                    precision,
                    shift,
                    coefficients: coeffs
                        .into_iter()
                        .map(|lp_coeff| {
                            let sum: f64 = lp_coeff.mul_add((1 << shift) as f64, error);
                            let qlp_coeff = (sum.round() as i32).clamp(min_coeff, max_coeff);
                            error = sum - (qlp_coeff as f64);
                            qlp_coeff
                        })
                        .collect(),
                })
            }
            shift @ MIN_SHIFT..0 => {
                // unusual negative shift case
                let shift = -shift as u32;

                Ok(Self {
                    order,
                    precision,
                    shift: 0,
                    coefficients: coeffs
                        .into_iter()
                        .map(|lp_coeff| {
                            let sum: f64 = (lp_coeff / (1 << shift) as f64) + error;
                            let qlp_coeff = (sum.round() as i32).clamp(min_coeff, max_coeff);
                            error = sum - (qlp_coeff as f64);
                            qlp_coeff
                        })
                        .collect(),
                })
            }
            ..MIN_SHIFT => Err(Error::LpNegativeShiftError),
        }
    }
}

#[test]
fn test_quantization() {
    // test against numbers generated from reference implementation

    let order = NonZero::new(4).unwrap();

    let quantized = LpcParameters::quantize(
        order,
        arrayvec![0.797774, -0.045362, -0.050136, -0.054254],
        SignedBitCount::new::<10>(),
    )
    .unwrap();

    assert_eq!(quantized.order, order);
    assert_eq!(quantized.precision, SignedBitCount::new::<10>());
    assert_eq!(quantized.shift, 9);
    assert_eq!(quantized.coefficients, arrayvec![408, -23, -25, -28]);

    // note the relationship between the un-quantized,
    // floating point parameters and the shift value (9)
    //
    // 409 / 2 ** 9 ≈ 0.796875
    // -23 / 2 ** 9 ≈ -0.044921
    // -25 / 2 ** 9 ≈ -0.048828
    // -28 / 2 ** 9 ≈ -0.054687
    //
    // we're converting floats to fractions

    let quantized = LpcParameters::quantize(
        order,
        arrayvec![-0.054687, -0.953216, -0.027115, 0.033537],
        SignedBitCount::new::<10>(),
    )
    .unwrap();

    assert_eq!(quantized.order, order);
    assert_eq!(quantized.precision, SignedBitCount::new::<10>());
    assert_eq!(quantized.shift, 9);
    assert_eq!(quantized.coefficients, arrayvec![-28, -488, -14, 17]);

    // coefficients should never be all zero, which is bad
    assert!(matches!(
        LpcParameters::quantize(
            order,
            arrayvec![0.0, 0.0, 0.0, 0.0],
            SignedBitCount::new::<10>(),
        ),
        Err(Error::ZeroLpCoefficients)
    ));

    // negative shifts should also be handled properly
    let quantized = LpcParameters::quantize(
        order,
        arrayvec![-0.1, 0.1, 10000000.0, -0.2],
        SignedBitCount::new::<10>(),
    )
    .unwrap();

    assert_eq!(quantized.order, order);
    assert_eq!(quantized.precision, SignedBitCount::new::<10>());
    assert_eq!(quantized.shift, 0);
    assert_eq!(quantized.coefficients, arrayvec![0, 0, 305, 0]);

    // and massive negative shifts must be an error
    assert!(matches!(
        LpcParameters::quantize(
            order,
            arrayvec![-0.1, 0.1, 100000000.0, -0.2],
            SignedBitCount::new::<10>(),
        ),
        Err(Error::LpNegativeShiftError)
    ));
}

fn autocorrelate(
    windowed: &[f64],
    max_lpc_order: NonZero<u8>,
) -> ArrayVec<f64, { MAX_LPC_COEFFS + 1 }> {
    // verified output against reference implementation
    // See: FLAC__lpc_compute_autocorrelation

    debug_assert!(usize::from(max_lpc_order.get()) <= MAX_LPC_COEFFS);

    let mut tail = windowed;
    // let mut autocorrelated = Vec::with_capacity(max_lpc_order.get().into());
    let mut autocorrelated = ArrayVec::default();

    for _ in 0..=max_lpc_order.get() {
        if tail.is_empty() {
            return autocorrelated;
        } else {
            autocorrelated.push(windowed.iter().zip(tail).map(|(x, y)| x * y).sum());
            tail.split_off_first();
        }
    }

    autocorrelated
}

#[test]
fn test_autocorrelation() {
    // test against numbers generated from reference implementation

    assert_eq!(
        autocorrelate(&[1.0], NonZero::new(1).unwrap()),
        arrayvec![1.0]
    );

    assert_eq!(
        autocorrelate(&[1.0, 2.0, 3.0, 4.0, 5.0], NonZero::new(4).unwrap()),
        arrayvec![55.0, 40.0, 26.0, 14.0, 5.0],
    );

    assert_eq!(
        autocorrelate(
            &[
                0.0, 16.0, 31.0, 44.0, 54.0, 61.0, 64.0, 63.0, 58.0, 49.0, 38.0, 24.0, 8.0, -8.0,
                -24.0, -38.0, -49.0, -58.0, -63.0, -64.0, -61.0, -54.0, -44.0, -31.0, -16.0,
            ],
            NonZero::new(4).unwrap()
        ),
        arrayvec![51408.0, 49792.0, 45304.0, 38466.0, 29914.0],
    )
}

#[test]
fn test_autocorrelation_at_max_order() {
    // Options::max_lpc_order accepts up to 32, which takes 33 values
    let windowed: Vec<f64> = (0..64).map(f64::from).collect();
    let autocorrelated = autocorrelate(&windowed, NonZero::new(32).unwrap());
    assert_eq!(autocorrelated.len(), 33);
    assert_eq!(
        autocorrelated[32],
        (0..32)
            .map(|n| f64::from(n) * f64::from(n + 32))
            .sum::<f64>()
    );
}

#[derive(Debug)]
struct LpCoeff {
    coeffs: ArrayVec<f64, MAX_LPC_COEFFS>,
    error: f64,
}

// returns a Vec of (coefficients, error) pairs
fn lp_coefficients(
    autocorrelated: ArrayVec<f64, { MAX_LPC_COEFFS + 1 }>,
) -> ArrayVec<LpCoeff, MAX_LPC_COEFFS> {
    // verified output against reference implementation
    // See: FLAC__lpc_compute_lp_coefficients

    match autocorrelated.len() {
        0 | 1 => panic!("must have at least 2 autocorrelation values"),
        _ => {
            let k = autocorrelated[1] / autocorrelated[0];
            let mut lp_coefficients = arrayvec![LpCoeff {
                coeffs: arrayvec![k],
                error: autocorrelated[0] * (1.0 - k.powi(2)),
            }];

            for i in 1..(autocorrelated.len() - 1) {
                if let [prev @ .., next] = &autocorrelated[0..=i + 1] {
                    let LpCoeff { coeffs, error } = lp_coefficients.last().unwrap();

                    let q = next
                        - prev
                            .iter()
                            .rev()
                            .zip(coeffs)
                            .map(|(x, y)| x * y)
                            .sum::<f64>();

                    let k = q / error;

                    lp_coefficients.push(LpCoeff {
                        coeffs: coeffs
                            .iter()
                            .zip(coeffs.iter().rev().map(|c| k * c))
                            .map(|(c1, c2)| c1 - c2)
                            .chain(std::iter::once(k))
                            .collect(),
                        error: error * (1.0 - k.powi(2)),
                    });
                }
            }

            lp_coefficients
        }
    }
}

#[allow(unused)]
macro_rules! assert_float_approx {
    ($a:expr, $b:expr) => {{
        let a = $a;
        let b = $b;
        assert!((a - b).abs() < 1.0e-6, "{a} != {b}");
    }};
}

#[test]
fn test_lp_coefficients_1() {
    // test against numbers generated from reference implementation

    let lp_coeffs = lp_coefficients(arrayvec![55.0, 40.0, 26.0, 14.0, 5.0]);

    assert_eq!(lp_coeffs.len(), 4);

    assert_float_approx!(lp_coeffs[0].error, 25.909091);
    assert_float_approx!(lp_coeffs[1].error, 25.540351);
    assert_float_approx!(lp_coeffs[2].error, 25.316142);
    assert_float_approx!(lp_coeffs[3].error, 25.241623);

    assert_eq!(lp_coeffs[0].coeffs.len(), 1);
    assert_float_approx!(lp_coeffs[0].coeffs[0], 0.727273);

    assert_eq!(lp_coeffs[1].coeffs.len(), 2);
    assert_float_approx!(lp_coeffs[1].coeffs[0], 0.814035);
    assert_float_approx!(lp_coeffs[1].coeffs[1], -0.119298);

    assert_eq!(lp_coeffs[2].coeffs.len(), 3);
    assert_float_approx!(lp_coeffs[2].coeffs[0], 0.802858);
    assert_float_approx!(lp_coeffs[2].coeffs[1], -0.043028);
    assert_float_approx!(lp_coeffs[2].coeffs[2], -0.093694);

    assert_eq!(lp_coeffs[3].coeffs.len(), 4);
    assert_float_approx!(lp_coeffs[3].coeffs[0], 0.797774);
    assert_float_approx!(lp_coeffs[3].coeffs[1], -0.045362);
    assert_float_approx!(lp_coeffs[3].coeffs[2], -0.050136);
    assert_float_approx!(lp_coeffs[3].coeffs[3], -0.054254);
}

#[test]
fn test_lp_coefficients_2() {
    // test against numbers generated from reference implementation

    let lp_coeffs = lp_coefficients(arrayvec![51408.0, 49792.0, 45304.0, 38466.0, 29914.0]);

    assert_eq!(lp_coeffs.len(), 4);

    assert_float_approx!(lp_coeffs[0].error, 3181.201369);
    assert_float_approx!(lp_coeffs[1].error, 495.815931);
    assert_float_approx!(lp_coeffs[2].error, 495.161449);
    assert_float_approx!(lp_coeffs[3].error, 494.604514);

    assert_eq!(lp_coeffs[0].coeffs.len(), 1);
    assert_float_approx!(lp_coeffs[0].coeffs[0], 0.968565);

    assert_eq!(lp_coeffs[1].coeffs.len(), 2);
    assert_float_approx!(lp_coeffs[1].coeffs[0], 1.858456);
    assert_float_approx!(lp_coeffs[1].coeffs[1], -0.918772);

    assert_eq!(lp_coeffs[2].coeffs.len(), 3);
    assert_float_approx!(lp_coeffs[2].coeffs[0], 1.891837);
    assert_float_approx!(lp_coeffs[2].coeffs[1], -0.986293);
    assert_float_approx!(lp_coeffs[2].coeffs[2], 0.036332);

    assert_eq!(lp_coeffs[3].coeffs.len(), 4);
    assert_float_approx!(lp_coeffs[3].coeffs[0], 1.890618);
    assert_float_approx!(lp_coeffs[3].coeffs[1], -0.953216);
    assert_float_approx!(lp_coeffs[3].coeffs[2], -0.027115);
    assert_float_approx!(lp_coeffs[3].coeffs[3], 0.033537);
}

// Returns (bits, order, coeffients) tuples
fn subframe_bits_by_order(
    bits_per_sample: SignedBitCount<32>,
    precision: SignedBitCount<15>,
    sample_count: u16,
    coeffs: ArrayVec<LpCoeff, MAX_LPC_COEFFS>,
) -> impl Iterator<Item = (f64, u8, ArrayVec<f64, MAX_LPC_COEFFS>)> {
    debug_assert!(sample_count > 0);

    let error_scale = 0.5 / f64::from(sample_count);

    coeffs
        .into_iter()
        .take_while(|coeffs| coeffs.error > 0.0)
        .zip(1..)
        .map(move |(LpCoeff { coeffs, error }, order)| {
            let header_bits =
                u32::from(order) * (u32::from(bits_per_sample) + u32::from(precision));

            let bits_per_residual =
                (error * error_scale).ln() / (2.0 * std::f64::consts::LN_2).max(0.0);

            let subframe_bits = bits_per_residual.mul_add(
                f64::from(sample_count - u16::from(order)),
                f64::from(header_bits),
            );

            (subframe_bits, order, coeffs)
        })
}

// Uses the error in the LP coefficients to determine the best order
// and returns that order along with the stripped-out coefficients
fn compute_best_order(
    bits_per_sample: SignedBitCount<32>,
    precision: SignedBitCount<15>,
    sample_count: u16,
    coeffs: ArrayVec<LpCoeff, MAX_LPC_COEFFS>,
) -> Result<(NonZero<u8>, ArrayVec<f64, MAX_LPC_COEFFS>), Error> {
    // verified output against reference implementation
    // See: FLAC__lpc_compute_best_order  and
    // See: FLAC__lpc_compute_expected_bits_per_residual_sample_with_error_scale

    subframe_bits_by_order(bits_per_sample, precision, sample_count, coeffs)
        .min_by(|(x, _, _), (y, _, _)| x.total_cmp(y))
        .and_then(|(_, order, coeffs)| Some((NonZero::new(order)?, coeffs)))
        .ok_or(Error::NoBestLpcOrder)
}

#[test]
fn test_compute_best_order() {
    // test against numbers generated from reference implementation

    let mut bits = subframe_bits_by_order(
        SignedBitCount::new::<16>(),
        SignedBitCount::new::<5>(),
        20,
        [3181.201369, 495.815931, 495.161449, 494.604514]
            .into_iter()
            .map(|error| LpCoeff {
                coeffs: ArrayVec::default(),
                error,
            })
            .collect(),
    )
    .map(|t| t.0);

    assert_float_approx!(bits.next().unwrap(), 80.977565);
    assert_float_approx!(bits.next().unwrap(), 74.685594);
    assert_float_approx!(bits.next().unwrap(), 93.853530);
    assert_float_approx!(bits.next().unwrap(), 113.025628);

    let mut bits = subframe_bits_by_order(
        SignedBitCount::new::<16>(),
        SignedBitCount::new::<10>(),
        4096,
        [15000.0, 25000.0, 20000.0, 30000.0]
            .into_iter()
            .map(|error| LpCoeff {
                coeffs: ArrayVec::default(),
                error,
            })
            .collect(),
    )
    .map(|t| t.0);

    assert_float_approx!(bits.next().unwrap(), 1812.801817);
    assert_float_approx!(bits.next().unwrap(), 3346.934051);
    assert_float_approx!(bits.next().unwrap(), 2713.303385);
    assert_float_approx!(bits.next().unwrap(), 3935.492805);
}

#[derive(Debug)]
struct Partition<'r, const RICE_MAX: u32> {
    header: ResidualPartitionHeader<RICE_MAX>,
    residuals: &'r [i32],
}

impl<'r, const RICE_MAX: u32> Partition<'r, RICE_MAX> {
    /// `partition_sum` is the sum of the residuals' magnitudes
    fn new(partition: &'r [i32], partition_sum: u64, estimated_bits: &mut u32) -> Option<Self> {
        let partition_samples = partition.len() as u16;
        if partition_samples == 0 {
            return None;
        }

        if partition_sum > 0 {
            let rice = if partition_sum > partition_samples.into() {
                let bits_needed = ((partition_sum as f64) / f64::from(partition_samples))
                    .log2()
                    .ceil() as u32;

                match BitCount::try_from(bits_needed).ok().filter(|rice| {
                    u32::from(*rice) < u32::from(BitCount::<RICE_MAX>::new::<RICE_MAX>())
                }) {
                    Some(rice) => rice,
                    None => {
                        let escape_size = (partition_sum.ilog2() + 2).try_into().ok()?;

                        *estimated_bits += u32::from(escape_size) * u32::from(partition_samples);

                        return Some(Self {
                            header: ResidualPartitionHeader::Escaped { escape_size },
                            residuals: partition,
                        });
                    }
                }
            } else {
                BitCount::new::<0>()
            };

            let partition_size: u32 = 4u32
                + ((1 + u32::from(rice)) * u32::from(partition_samples))
                + if u32::from(rice) > 0 {
                    u32::try_from(partition_sum >> (u32::from(rice) - 1)).ok()?
                } else {
                    u32::try_from(partition_sum << 1).ok()?
                }
                - (u32::from(partition_samples) / 2);

            *estimated_bits += partition_size;

            Some(Partition {
                header: ResidualPartitionHeader::Standard { rice },
                residuals: partition,
            })
        } else {
            // all partition residuals are 0, so use a constant
            Some(Partition {
                header: ResidualPartitionHeader::Constant,
                residuals: partition,
            })
        }
    }
}

/// Bit writers that write Rice codes
///
/// The provided method writes the unary count and the remaining bits
/// separately; the frame writer writes the whole code in one go.
trait WriteRice: BitWrite {
    /// Writes `folded` (a zigzag-folded residual) as a Rice code with
    /// parameter `rice`
    #[inline]
    fn write_rice<const RICE_MAX: u32>(
        &mut self,
        rice: BitCount<RICE_MAX>,
        folded: u32,
    ) -> std::io::Result<()> {
        let (msb, lsb) = rice.mask_lsb()(folded);
        self.write_unary::<1>(msb)?;
        self.write_checked(lsb)
    }
}

impl WriteRice for BitsWritten<u32> {}

impl WriteRice for crate::frames::FrameWriter<'_> {
    #[inline]
    fn write_rice<const RICE_MAX: u32>(
        &mut self,
        rice: BitCount<RICE_MAX>,
        folded: u32,
    ) -> std::io::Result<()> {
        self.rice(u32::from(rice), folded);
        Ok(())
    }
}

/// Writes a partition's header and residuals
#[inline]
fn write_partition<const RICE_MAX: u32, W: WriteRice>(
    w: &mut W,
    partition: &Partition<'_, RICE_MAX>,
) -> Result<(), std::io::Error> {
    w.build(&partition.header)?;
    match partition.header {
        ResidualPartitionHeader::Standard { rice } => {
            partition.residuals.iter().try_for_each(|s| {
                w.write_rice(
                    rice,
                    if s.is_negative() {
                        ((s.unsigned_abs() - 1) << 1) + 1
                    } else {
                        (*s as u32) << 1
                    },
                )
            })?;
        }
        ResidualPartitionHeader::Escaped { escape_size } => {
            partition
                .residuals
                .iter()
                .try_for_each(|s| w.write_signed_counted(escape_size, *s))?;
        }
        ResidualPartitionHeader::Constant => { /* nothing left to do */ }
    }
    Ok(())
}

// a partition order of up to 15 allows 32768 partitions,
// more than fit on the stack
fn best_partitions<'r, const RICE_MAX: u32>(
    options: &EncoderOptions,
    block_size: usize,
    residuals: &'r [i32],
) -> Vec<Partition<'r, RICE_MAX>> {
    let max_order = block_size.trailing_zeros().min(options.max_partition_order);

    // the residuals' magnitudes summed per partition of the finest order,
    // from the end as `rchunks` splits them, so that each coarser order
    // adds up runs of these instead of reading every residual again
    let finest = block_size >> max_order;
    let sums: Vec<u64> = residuals
        .rchunks(finest)
        .map(|partition| partition.iter().map(|i| u64::from(i.unsigned_abs())).sum())
        .collect();

    (0..=max_order)
        .map(|partition_order| 1 << partition_order)
        .take_while(|partition_count: &usize| partition_count.is_power_of_two())
        .filter_map(|partition_count| {
            let mut estimated_bits = 0;

            // a predictor order of at least the partition size leaves fewer
            // chunks than partitions, which this partition order cannot describe
            if block_size / partition_count <= block_size - residuals.len() {
                return None;
            }

            let partition_size = block_size / partition_count;
            let partitions = residuals
                .rchunks(partition_size)
                .zip(sums.chunks(partition_size / finest))
                .rev()
                .map(|(partition, sums)| {
                    Partition::new(partition, sums.iter().sum(), &mut estimated_bits)
                })
                .collect::<Option<Vec<_>>>()
                .filter(|p| !p.is_empty() && p.len().is_power_of_two())?;

            Some((partitions, estimated_bits))
        })
        .min_by_key(|(_, estimated_bits)| *estimated_bits)
        .map(|(partitions, _)| partitions)
        .unwrap_or_else(|| {
            std::iter::once(Partition {
                header: ResidualPartitionHeader::Escaped {
                    escape_size: SignedBitCount::new::<0b11111>(),
                },
                residuals,
            })
            .collect()
        })
}

fn write_partitions<const RICE_MAX: u32, W: WriteRice>(
    writer: &mut W,
    partitions: &[Partition<'_, RICE_MAX>],
) -> Result<(), Error> {
    writer.write::<4, u32>(partitions.len().ilog2())?; // partition order
    for partition in partitions {
        write_partition(writer, partition)?;
    }
    Ok(())
}

#[inline]
fn try_shrink_header<const RICE_MAX: u32, const RICE_NEW_MAX: u32>(
    header: ResidualPartitionHeader<RICE_MAX>,
) -> Option<ResidualPartitionHeader<RICE_NEW_MAX>> {
    Some(match header {
        ResidualPartitionHeader::Standard { rice } => ResidualPartitionHeader::Standard {
            rice: rice.try_map(|r| (r < RICE_NEW_MAX).then_some(r))?,
        },
        ResidualPartitionHeader::Escaped { escape_size } => {
            ResidualPartitionHeader::Escaped { escape_size }
        }
        ResidualPartitionHeader::Constant => ResidualPartitionHeader::Constant,
    })
}

enum CodingMethod<'p> {
    Rice(Vec<Partition<'p, 0b1111>>),
    Rice2(Vec<Partition<'p, 0b11111>>),
}

fn try_reduce_rice(partitions: Vec<Partition<'_, 0b11111>>) -> CodingMethod<'_> {
    match partitions
        .iter()
        .map(|Partition { header, residuals }| {
            try_shrink_header(*header).map(|header| Partition { header, residuals })
        })
        .collect()
    {
        Some(partitions) => CodingMethod::Rice(partitions),
        None => CodingMethod::Rice2(partitions),
    }
}

/// How a subframe's residuals are coded, chosen but not yet written
impl CodingMethod<'_> {
    /// The number of bits `write` produces,
    /// or `None` where `write` would fail
    fn bits(&self) -> Option<u64> {
        Some(
            2 + match self {
                Self::Rice(partitions) => partitions_bits(partitions)?,
                Self::Rice2(partitions) => partitions_bits(partitions)?,
            },
        )
    }

    fn write<W: WriteRice>(&self, writer: &mut W) -> Result<(), Error> {
        match self {
            Self::Rice(partitions) => {
                writer.write::<2, u8>(0)?; // coding method 0
                write_partitions(writer, partitions)
            }
            Self::Rice2(partitions) => {
                writer.write::<2, u8>(1)?; // coding method 1
                write_partitions(writer, partitions)
            }
        }
    }
}

/// The partitions' bits as `write_partitions` writes them: the partition order,
/// then each partition's header and residuals; `None` where writing would fail
/// (an escaped residual that does not fit in the escape size)
fn partitions_bits<const RICE_MAX: u32>(partitions: &[Partition<'_, RICE_MAX>]) -> Option<u64> {
    // a Rice parameter or the escape code takes the bits of RICE_MAX
    let parameter = u64::from((RICE_MAX + 1).ilog2());
    partitions
        .iter()
        .map(|Partition { header, residuals }| match header {
            ResidualPartitionHeader::Standard { rice } => {
                let rice = u32::from(*rice);
                Some(
                    parameter
                        + residuals
                            .iter()
                            .map(|s| {
                                let folded = if s.is_negative() {
                                    ((s.unsigned_abs() - 1) << 1) + 1
                                } else {
                                    (*s as u32) << 1
                                };
                                u64::from(folded >> rice) + 1 + u64::from(rice)
                            })
                            .sum::<u64>(),
                )
            }
            ResidualPartitionHeader::Escaped { escape_size } => {
                let bits = u32::from(escape_size.count());
                let (min, max) = (-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
                residuals
                    .iter()
                    .all(|s| (min..=max).contains(&i64::from(*s)))
                    .then(|| parameter + 5 + residuals.len() as u64 * u64::from(bits))
            }
            ResidualPartitionHeader::Constant => Some(parameter + 5),
        })
        .try_fold(4, |total, bits| Some(total + bits?))
}

#[test]
fn residual_bits_match_what_is_written() {
    // random residuals from small to the edges of i32, every partition order,
    // with and without RICE2: `bits` must be what `write` writes, and `None`
    // exactly where writing fails
    let mut seed = 0xD1B5_4A32_D192_ED03u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut failures = 0;
    for round in 0..3000 {
        let options = EncoderOptions {
            max_partition_order: (next() % 16) as u32,
            mid_side: false,
            seektable_interval: None,
            max_lpc_order: None,
            window: Window::default(),
            exhaustive_channel_correlation: false,
            use_rice2: next() % 2 == 0,
        };
        let block = [16usize, 192, 1152, 4096, 4608][round % 5];
        let order = (next() % 9) as usize;
        let bits = [1u32, 4, 12, 20, 28, 31, 32][(next() % 7) as usize];
        let residuals: Vec<i32> = (0..block - order)
            .map(|_| match next() % 10 {
                0 => i32::MIN,
                1 => i32::MAX,
                2 => 0,
                _ => (next() as i32) >> (32 - bits),
            })
            .collect();
        let plan = plan_residuals(&options, order, &residuals);
        let mut written = BitsWritten::<u32>::new();
        match (plan.bits(), plan.write(&mut written)) {
            (Some(bits), Ok(())) => assert_eq!(bits, u64::from(written.written()), "round {round}"),
            (None, Err(_)) => failures += 1,
            (bits, result) => panic!("round {round}: planned {bits:?}, written {result:?}"),
        }
    }
    // some rounds do fail to write, so that case is checked too
    assert!(failures > 0);
}

/// Chooses how to code a subframe's residuals
fn plan_residuals<'r>(
    options: &EncoderOptions,
    predictor_order: usize,
    residuals: &'r [i32],
) -> CodingMethod<'r> {
    let block_size = predictor_order + residuals.len();

    if options.use_rice2 {
        try_reduce_rice(best_partitions(options, block_size, residuals))
    } else {
        CodingMethod::Rice(best_partitions::<0b1111>(options, block_size, residuals))
    }
}

fn try_join<A, B, RA, RB, E>(oper_a: A, oper_b: B) -> Result<(RA, RB), E>
where
    A: FnOnce() -> Result<RA, E> + Send,
    B: FnOnce() -> Result<RB, E> + Send,
    RA: Send,
    RB: Send,
    E: Send,
{
    let (a, b) = join(oper_a, oper_b);
    Ok((a?, b?))
}

#[cfg(feature = "rayon")]
use rayon::join;

#[cfg(not(feature = "rayon"))]
fn join<A, B, RA, RB>(oper_a: A, oper_b: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    (oper_a(), oper_b())
}

#[cfg(feature = "rayon")]
fn vec_map<T, U, F>(src: Vec<T>, f: F) -> Vec<U>
where
    T: Send,
    U: Send,
    F: Fn(T) -> U + Send + Sync,
{
    use rayon::iter::{IntoParallelIterator, ParallelIterator};

    src.into_par_iter().map(f).collect()
}

#[cfg(not(feature = "rayon"))]
fn vec_map<T, U, F>(src: Vec<T>, f: F) -> Vec<U>
where
    T: Send,
    U: Send,
    F: Fn(T) -> U + Send + Sync,
{
    src.into_iter().map(f).collect()
}

fn exact_div<N>(n: N, rhs: N) -> Option<N>
where
    N: std::ops::Div<Output = N> + std::ops::Rem<Output = N> + std::cmp::PartialEq + Copy + Default,
{
    (n % rhs == N::default()).then_some(n / rhs)
}
