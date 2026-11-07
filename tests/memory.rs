// Heap use of the NASTRA branch (see PATCHES.md) while decoding damaged and hostile
// streams: no input may make the decoder allocate far more than the input's size.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use flac_codec::decode::FlacSampleReader;
use flac_codec::encode::{FlacSampleWriter, Options};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Counts the bytes allocated on the heap and their peak.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    PEAK.fetch_max(LIVE.fetch_add(by, Relaxed) + by, Relaxed);
}

// SAFETY: every call goes straight to the system allocator; only counters are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new_size) };
        if !q.is_null() {
            if new_size > layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The heap's peak above its level before `f`, in bytes.
fn peak_while(f: impl FnOnce()) -> usize {
    let before = LIVE.load(Relaxed);
    PEAK.store(before, Relaxed);
    f();
    PEAK.load(Relaxed) - before
}

fn decode(flac: &[u8]) -> Result<Vec<i32>, flac_codec::Error> {
    let mut samples = vec![];
    FlacSampleReader::new(Cursor::new(flac))?.read_to_end(&mut samples)?;
    Ok(samples)
}

/// A short stream with STREAMINFO as its only metadata block.
fn small_stream() -> Vec<u8> {
    let samples: Vec<i32> = (0..4000).map(|i| ((i * 37) % 2000) - 1000).collect();
    let mut flac = Cursor::new(vec![]);
    let options = Options::default().no_padding().no_seektable();
    let mut writer =
        FlacSampleWriter::new(&mut flac, options, 44100, 16, 2, Some(samples.len() as u64))
            .unwrap();
    writer.write(&samples).unwrap();
    writer.finalize().unwrap();
    let flac = flac.into_inner();
    // fLaC, then STREAMINFO marked as the last block
    assert_eq!(&flac[..4], b"fLaC");
    assert_eq!(flac[4], 0x80);
    flac
}

/// `small_stream` with one more metadata block (type, contents) after STREAMINFO.
fn with_block(block_type: u8, contents: &[u8]) -> Vec<u8> {
    let flac = small_stream();
    let size = u32::try_from(contents.len()).unwrap();
    assert!(size < 1 << 24);
    let mut out = flac[..4].to_vec();
    out.push(0x00); // STREAMINFO, no longer the last block
    out.extend_from_slice(&flac[5..42]);
    out.push(0x80 | block_type);
    out.extend_from_slice(&size.to_be_bytes()[1..]);
    out.extend_from_slice(contents);
    out.extend_from_slice(&flac[42..]);
    out
}

const MIB: usize = 1 << 20;

/// Decodes `flac`, notes its peak in `report` and fails above `limit` bytes.
fn check(report: &mut Vec<String>, name: &str, flac: &[u8], limit: usize) {
    let peak = peak_while(|| {
        let _ = decode(flac);
    });
    report.push(format!(
        "{name}: {} bytes in, peak {} KiB",
        flac.len(),
        peak / 1024
    ));
    assert!(peak <= limit, "{name}: peak {peak} bytes, limit {limit}");
}

#[test]
fn decoding_memory_stays_bounded() {
    let mut report = vec![];

    // the undamaged stream, for comparison
    let clean = small_stream();
    check(&mut report, "clean stream", &clean, 4 * MIB);

    // damaged streams: bit flips, truncation and garbage, as in tests/hardening.rs
    let mut rng = 0xF1AC_5EEDu64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut worst = 0;
    for _ in 0..2000 {
        let mut flac = clean.clone();
        match next() % 3 {
            0 => (0..1 + next() % 8).for_each(|_| {
                let at = (next() % flac.len() as u64) as usize;
                flac[at] ^= 1 << (next() % 8);
            }),
            1 => flac.truncate((next() % flac.len() as u64) as usize),
            _ => {
                let at = (next() % flac.len() as u64) as usize;
                let len = ((next() % 64) as usize).min(flac.len() - at);
                flac[at..at + len]
                    .iter_mut()
                    .for_each(|b| *b = next() as u8);
            }
        }
        worst = worst.max(peak_while(|| {
            let _ = decode(&flac);
        }));
    }
    report.push(format!(
        "2000 damaged streams: worst peak {} KiB",
        worst / 1024
    ));
    // a damaged frame header can ask for the largest block: 65,535 samples × 8 channels
    assert!(worst <= 16 * MIB, "damaged streams: peak {worst} bytes");

    // a VORBIS_COMMENT block of almost 16 MiB holding 4,194,301 empty comments
    let count = ((1usize << 24) - 1 - 8) / 4;
    let mut comments = vec![0u8; 4]; // empty vendor string
    comments.extend_from_slice(&u32::try_from(count).unwrap().to_le_bytes());
    comments.resize(8 + 4 * count, 0);
    check(
        &mut report,
        "16 MiB of empty comments",
        &with_block(4, &comments),
        256 * MIB,
    );

    // a comment count of 4 billion in a small block
    let mut comments = vec![0u8; 4];
    comments.extend_from_slice(&u32::MAX.to_le_bytes());
    check(
        &mut report,
        "4 billion comments claimed",
        &with_block(4, &comments),
        4 * MIB,
    );

    // a comment and a vendor string claiming 4 GiB
    check(
        &mut report,
        "4 GiB vendor string claimed",
        &with_block(4, &u32::MAX.to_le_bytes()),
        4 * MIB,
    );

    // a picture claiming 4 GiB of data in a small block
    let mut picture = vec![];
    picture.extend_from_slice(&3u32.to_be_bytes()); // front cover
    picture.extend_from_slice(&9u32.to_be_bytes());
    picture.extend_from_slice(b"image/png");
    picture.extend_from_slice(&0u32.to_be_bytes()); // no description
    picture.extend_from_slice(&[0; 16]); // width, height, depth, colours
    picture.extend_from_slice(&u32::MAX.to_be_bytes());
    picture.extend_from_slice(&[0x89, b'P', b'N', b'G']);
    check(
        &mut report,
        "4 GiB picture claimed",
        &with_block(6, &picture),
        4 * MIB,
    );

    // a seek table and an application block of almost 16 MiB
    let seekpoints = vec![0xFF; ((1 << 24) - 1) / 18 * 18];
    check(
        &mut report,
        "16 MiB seek table",
        &with_block(3, &seekpoints),
        64 * MIB,
    );
    let application = vec![0x41; (1 << 24) - 1];
    check(
        &mut report,
        "16 MiB application block",
        &with_block(2, &application),
        64 * MIB,
    );

    println!("{}", report.join("\n"));
}
