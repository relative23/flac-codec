// Copyright 2025 Brian Langenberger
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! For handling PCM audio in frame-sized chunks

use arrayvec::ArrayVec;

#[derive(Clone, Default)]
pub struct Frame {
    // all samples, stacked by channel
    samples: Vec<i32>,

    // total number of channels
    channels: usize,

    // total length of each channel in samples
    channel_len: usize,

    // bits-per-sample
    bits_per_sample: u32,
}

impl Frame {
    /// Returns empty Frame which can be filled as needed
    #[inline]
    pub fn empty(channels: usize, bits_per_sample: u32) -> Self {
        Self {
            samples: Vec::new(),
            channels,
            channel_len: 0,
            bits_per_sample,
        }
    }

    /// Returns PCM frame count
    #[inline]
    pub fn pcm_frames(&self) -> usize {
        self.channel_len
    }

    pub fn resize(
        &mut self,
        bits_per_sample: u32,
        channels: usize,
        block_size: usize,
    ) -> &mut [i32] {
        self.bits_per_sample = bits_per_sample;
        self.channels = channels;
        self.channel_len = block_size;
        self.samples.resize(channels * block_size, 0);
        &mut self.samples
    }

    /// Resizes our frame with the given parameters and returns channel iterator
    pub fn resized_channels(
        &mut self,
        bits_per_sample: u32,
        channels: usize,
        block_size: usize,
    ) -> impl Iterator<Item = &mut [i32]> {
        self.resize(bits_per_sample, channels, block_size)
            .chunks_exact_mut(block_size)
    }

    /// Resizes our frame for two channels and returns both
    pub fn resized_stereo(
        &mut self,
        bits_per_sample: u32,
        block_size: usize,
    ) -> (&mut [i32], &mut [i32]) {
        self.resize(bits_per_sample, 2, block_size)
            .split_at_mut(block_size)
    }

    /// Returns bytes-per-sample
    #[inline]
    pub fn bytes_per_sample(&self) -> usize {
        self.bits_per_sample.div_ceil(8) as usize
    }

    /// Returns total number of samples, in all channels
    #[inline]
    pub fn samples_len(&self) -> usize {
        self.channels * self.channel_len
    }

    /// Returns total length of buffer in bytes
    #[inline]
    pub fn bytes_len(&self) -> usize {
        self.bytes_per_sample() * self.samples.len()
    }

    /// Iterates over any samples in interleaved order
    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = i32> + '_ {
        Interleaved {
            samples: &self.samples[..self.channels * self.channel_len],
            channels: self.channels,
            channel_len: self.channel_len,
            frame: 0,
            channel: 0,
        }
    }

    #[inline]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut i32> {
        MultiZip {
            iters: self.channels_mut().map(|c| c.iter_mut()).collect(),
        }
        .flatten()
    }

    /// Fills buffer with our samples in the given endianness
    pub fn to_buf<E: crate::byteorder::Endianness>(&self, buf: &mut [u8]) {
        match self.bytes_per_sample() {
            1 => self.interleave(buf.as_chunks_mut().0, |s| E::i8_to_bytes(s as i8)),
            2 => self.interleave(buf.as_chunks_mut().0, |s| E::i16_to_bytes(s as i16)),
            3 => self.interleave(buf.as_chunks_mut().0, E::i24_to_bytes),
            4 => self.interleave(buf.as_chunks_mut().0, E::i32_to_bytes),
            _ => panic!("unsupported number of bytes per sample"),
        }
    }

    /// Writes our samples to `out` in interleaved order, like:
    /// [left₀ , right₀ , left₁ , right₁ , left₂ , right₂ , …]
    ///
    /// `out` is filled as far as it and the samples reach.
    pub fn interleave<T>(&self, out: &mut [T], convert: impl Fn(i32) -> T) {
        let samples = &self.samples[..self.channels * self.channel_len];
        if samples.is_empty() {
            // (no channels, or channels of no samples, which cannot be split)
            return;
        }
        match self.channels {
            1 => {
                for (sample, place) in samples.iter().zip(out) {
                    *place = convert(*sample);
                }
            }
            // stereo, by far the most common, in one pass over both channels
            2 => {
                let (left, right) = samples.split_at(self.channel_len);
                let (pairs, rest) = out.as_chunks_mut();
                for ((left, right), places) in left.iter().zip(right).zip(pairs.iter_mut()) {
                    *places = [convert(*left), convert(*right)];
                }
                // room for half a PCM frame more
                if let (Some(place), Some(left)) = (rest.first_mut(), left.get(pairs.len())) {
                    *place = convert(*left);
                }
            }
            // any other layout one channel at a time, each sample to its
            // place in its PCM frame
            channels => {
                for (first, channel) in samples.chunks_exact(self.channel_len).enumerate() {
                    for (sample, place) in channel
                        .iter()
                        .zip(out.iter_mut().skip(first).step_by(channels))
                    {
                        *place = convert(*sample);
                    }
                }
            }
        }
    }

    /// Iterates over all channels
    #[inline]
    pub fn channels(&self) -> impl Iterator<Item = &[i32]> {
        self.samples.chunks_exact(self.channel_len)
    }

    /// Iterates over all channels as mutable samples
    #[inline]
    pub fn channels_mut(&mut self) -> impl Iterator<Item = &mut [i32]> {
        self.samples.chunks_exact_mut(self.channel_len)
    }

    /// Fills frame samples from bytes of the given endianness
    pub fn fill_from_buf<E: crate::byteorder::Endianness>(&mut self, buf: &[u8]) -> &mut Self {
        match self.bytes_per_sample() {
            1 => {
                self.channel_len = buf.len() / self.channels;
                self.samples.resize(buf.len(), 0);

                for (sample, bytes) in self.iter_mut().zip(buf.as_chunks().0) {
                    *sample = E::bytes_to_i8(*bytes).into()
                }
            }
            2 => {
                self.channel_len = (buf.len() / 2) / self.channels;
                self.samples.resize(buf.len() / 2, 0);

                for (sample, bytes) in self.iter_mut().zip(buf.as_chunks().0) {
                    *sample = E::bytes_to_i16(*bytes).into()
                }
            }
            3 => {
                self.channel_len = (buf.len() / 3) / self.channels;
                self.samples.resize(buf.len() / 3, 0);

                for (sample, bytes) in self.iter_mut().zip(buf.as_chunks().0) {
                    *sample = E::bytes_to_i24(*bytes)
                }
            }
            4 => {
                self.channel_len = (buf.len() / 4) / self.channels;
                self.samples.resize(buf.len() / 4, 0);

                for (sample, bytes) in self.iter_mut().zip(buf.as_chunks().0) {
                    *sample = E::bytes_to_i32(*bytes)
                }
            }
            _ => panic!("unsupported number of bytes per sample"),
        }

        self
    }

    /// Fills frame samples from interleaved samples
    pub fn fill_from_samples(&mut self, samples: &[i32]) -> &mut Self {
        self.channel_len = samples.len() / self.channels;
        self.samples.resize(samples.len(), 0);

        // each channel's samples in turn, from whole PCM frames
        let len = self.channel_len;
        match self.channels {
            1 => self.samples[..len].copy_from_slice(&samples[..len]),
            2 => {
                let (left, right) = self.samples.split_at_mut(len);
                for ((l, r), pair) in left
                    .iter_mut()
                    .zip(&mut right[..len])
                    .zip(samples.as_chunks::<2>().0)
                {
                    *l = pair[0];
                    *r = pair[1];
                }
            }
            channels => {
                for (i, frame) in samples.chunks_exact(channels).enumerate() {
                    for (c, sample) in frame.iter().enumerate() {
                        self.samples[c * len + i] = *sample;
                    }
                }
            }
        }

        self
    }

    /// Fills frame samples from indepedent channels
    ///
    /// Channel count must match our internal channel count
    /// (Which must be at least 1 channel)
    pub fn fill_from_channels<C, S>(&mut self, channels: C) -> &mut Self
    where
        C: AsRef<[S]>,
        S: AsRef<[i32]>,
    {
        let channels = channels.as_ref();
        debug_assert_eq!(self.channels, channels.len());

        self.channel_len = channels[0].as_ref().len();
        self.samples.resize(channels.len() * self.channel_len, 0);

        for (o, i) in self
            .samples
            .chunks_exact_mut(self.channel_len)
            .zip(channels)
        {
            o.copy_from_slice(i.as_ref());
        }

        self
    }
}

/// A frame's samples in interleaved order, read straight from the
/// channel-stacked buffer
struct Interleaved<'f> {
    samples: &'f [i32],
    channels: usize,
    channel_len: usize,
    // the next PCM frame and channel
    frame: usize,
    channel: usize,
}

impl Iterator for Interleaved<'_> {
    type Item = i32;

    #[inline]
    fn next(&mut self) -> Option<i32> {
        if self.frame == self.channel_len {
            return None;
        }
        let sample = self
            .samples
            .get(self.channel * self.channel_len + self.frame)
            .copied();
        self.channel += 1;
        if self.channel == self.channels {
            self.channel = 0;
            self.frame += 1;
        }
        sample
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = (self.channel_len - self.frame) * self.channels - self.channel;
        (left, Some(left))
    }
}

impl ExactSizeIterator for Interleaved<'_> {}

const MAX_CHANNELS: usize = 8;

pub struct MultiZip<I> {
    iters: ArrayVec<I, MAX_CHANNELS>,
}

impl<I: Iterator> Iterator for MultiZip<I> {
    type Item = ArrayVec<I::Item, MAX_CHANNELS>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.iters.iter_mut().map(|i| i.next()).collect()
    }
}

impl<I> FromIterator<I> for MultiZip<I> {
    #[inline]
    fn from_iter<T: IntoIterator<Item = I>>(iter: T) -> Self {
        Self {
            iters: iter.into_iter().collect(),
        }
    }
}

#[test]
fn interleaves_like_multizip() {
    for channels in 1..=8 {
        for len in [0, 1, 7, 4096] {
            let mut frame = Frame::empty(channels, 16);
            frame
                .resize(16, channels, len)
                .iter_mut()
                .enumerate()
                .for_each(|(i, s)| *s = i as i32);
            // (`channels` needs at least one sample per channel)
            let expected: Vec<i32> = match len {
                0 => vec![],
                _ => MultiZip {
                    iters: frame.channels().map(|c| c.iter().copied()).collect(),
                }
                .flatten()
                .collect(),
            };
            let iter = frame.iter();
            assert_eq!(iter.len(), channels * len);
            assert_eq!(
                iter.collect::<Vec<_>>(),
                expected,
                "{channels} channels, {len}"
            );
        }
    }
}

#[test]
fn fills_from_samples_like_multizip() {
    for channels in 1..=8 {
        for len in [1, 7, 4096] {
            let samples: Vec<i32> = (0..(channels * len) as i32).map(|i| i * 3 - 7).collect();
            let mut frame = Frame::empty(channels, 16);
            frame.fill_from_samples(&samples);
            // the previous way: every interleaved position in turn
            let mut expected = Frame::empty(channels, 16);
            expected.channel_len = len;
            expected.samples.resize(samples.len(), 0);
            for (o, i) in expected.iter_mut().zip(&samples) {
                *o = *i;
            }
            assert_eq!(
                frame.samples, expected.samples,
                "{channels} channels, {len}"
            );
            assert_eq!(frame.channel_len, len);
        }
    }
}

#[test]
fn interleaves_like_the_iterator() {
    use crate::byteorder::{BigEndian, LittleEndian};

    for channels in 0..=8 {
        for len in [0, 1, 2, 7, 1000] {
            for bits in [8, 12, 16, 20, 24, 32] {
                let mut frame = Frame::empty(channels, bits);
                for (i, sample) in frame.resize(bits, channels, len).iter_mut().enumerate() {
                    *sample = (i as i32).wrapping_mul(0x0101_0907) >> (32 - bits);
                }
                // the previous way: every interleaved position in turn
                let expected: Vec<i32> = frame.iter().collect();
                let case = format!("{channels} channels, {len} samples, {bits} bits");

                let mut samples = vec![i32::MIN; expected.len()];
                frame.interleave(&mut samples, |sample| sample);
                assert_eq!(samples, expected, "{case}");
                assert_eq!(frame.samples_len(), expected.len(), "{case}");

                // room for fewer or more samples is filled as far as both reach
                let mut short = vec![i32::MIN; expected.len() / 2];
                frame.interleave(&mut short, |sample| sample);
                assert_eq!(short, expected[..short.len()], "{case}");
                let mut long = vec![i32::MIN; expected.len() + 3];
                frame.interleave(&mut long, |sample| sample);
                assert_eq!(long[..expected.len()], expected, "{case}");
                assert_eq!(long[expected.len()..], [i32::MIN; 3], "{case}");

                // bytes of both orders, each sample converted on its own
                let width = frame.bytes_per_sample();
                let mut little = vec![0xAA; frame.bytes_len()];
                frame.to_buf::<LittleEndian>(&mut little);
                let mut big = vec![0xAA; frame.bytes_len()];
                frame.to_buf::<BigEndian>(&mut big);
                let bytes = |sample: i32, big: bool| -> Vec<u8> {
                    let all = match big {
                        true => sample.to_be_bytes(),
                        false => sample.to_le_bytes(),
                    };
                    match big {
                        true => all[4 - width..].to_vec(),
                        false => all[..width].to_vec(),
                    }
                };
                let expected_little: Vec<u8> =
                    expected.iter().flat_map(|s| bytes(*s, false)).collect();
                let expected_big: Vec<u8> = expected.iter().flat_map(|s| bytes(*s, true)).collect();
                assert_eq!(little, expected_little, "{case}");
                assert_eq!(big, expected_big, "{case}");
            }
        }
    }
}
