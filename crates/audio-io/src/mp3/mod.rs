//! A small pure-Rust MP3 encoder (ISO/IEC 11172-3 MPEG-1 Layer III): constant bitrate, long
//! blocks, joint (mid/side) stereo, a bit reservoir, scalefactor noise shaping against a simple
//! masking estimate, and an `Info` header with the frame and byte counts. Other session rates are
//! resampled to 44.1 or 48 kHz. Decoding is symphonia's.
#![allow(clippy::needless_range_loop)]

mod analysis;
mod huffman;
mod quantize;
mod resample;
mod tables;

use crate::pcm::validate_layout;

/// A sample clamped to [-1, 1], with NaN as silence.
fn clamp_float(x: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x.clamp(-1.0, 1.0) }
}
use crate::{AudioBuffer, AudioError, Result};
use analysis::{Analysis, Bank, GRANULE};
use quantize::{Granule, Quantizer, Work};
use resample::Resampler;
use std::collections::VecDeque;
use std::io::{Cursor, Seek, SeekFrom, Write};

/// Bitrates (kbps) by header index 1–14.
pub(crate) const BITRATES: [u32; 14] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
/// The bitrate used when none is given.
pub(crate) const DEFAULT_KBPS: u32 = 320;
/// Sample rates by header index.
const RATES: [u32; 3] = [44_100, 48_000, 32_000];
/// Samples per channel in a frame.
const FRAME: usize = 2 * GRANULE;
/// Silence appended at the end so the decoder's delay doesn't cut the last samples off.
const TAIL: usize = GRANULE + 529;
/// The encoder's own delay in samples: the decoded signal starts this many samples (plus the
/// decoder's 529) after the frame data begins. Measured as 1057 in total through symphonia and
/// ffmpeg, which is what the gapless `LAME` tag tells decoders to skip.
const ENC_DELAY: u64 = 528;
/// The largest `main_data_begin` (bytes of earlier frames a frame's data may start in).
const MAX_RESERVOIR: u64 = 511;
/// Level applied before the filterbank so a decoder reproduces the input level (the MDCT and the
/// decoder's IMDCT are unnormalised, together a gain of 9).
const INPUT_GAIN: f32 = 1.0 / 9.0;

/// Bits written most significant first.
pub(super) struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn new() -> Self {
        Bits { out: Vec::new(), acc: 0, n: 0 }
    }

    /// Writes the low `n` (at most 32) bits of `v`.
    pub(super) fn put(&mut self, v: u32, n: u32) {
        if n == 0 {
            return;
        }
        let n = n.min(32);
        self.acc = (self.acc << n) | (u64::from(v) & ((1u64 << n) - 1));
        self.n += n;
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
        self.acc &= (1u64 << self.n) - 1;
    }

    fn bits(&self) -> u64 {
        self.out.len() as u64 * 8 + u64::from(self.n)
    }

    /// The bytes, the last one padded with zero bits.
    fn into_bytes(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc << (8 - self.n)) as u8);
        }
        self.out
    }
}

/// The header index of `kbps`, or an error naming the bitrates MP3 has.
fn bitrate_index(kbps: u32) -> Result<u32> {
    BITRATES.iter().position(|&b| b == kbps).map(|i| i as u32 + 1).ok_or_else(|| {
        let list: Vec<String> = BITRATES.iter().map(u32::to_string).collect();
        AudioError::Unsupported(format!("MP3 bitrate {kbps} kbps (use one of {})", list.join(", ")))
    })
}

/// The MPEG-1 rate a session rate is written at: itself when MP3 has it, else 44.1 kHz for the
/// 44.1 kHz family and 48 kHz for everything else.
fn mp3_rate(sample_rate: u32) -> u32 {
    if RATES.contains(&sample_rate) {
        sample_rate
    } else if sample_rate.is_multiple_of(11_025) {
        44_100
    } else {
        48_000
    }
}

/// The lowpass for `kbps` per channel: the fewer the bits, the less treble is worth coding.
fn lowpass_hz(kbps_per_channel: u32, sample_rate: u32) -> f32 {
    const CURVE: [(f32, f32); 12] = [
        (16.0, 5_000.0),
        (24.0, 7_000.0),
        (32.0, 9_500.0),
        (40.0, 11_500.0),
        (48.0, 13_500.0),
        (56.0, 15_000.0),
        (64.0, 16_000.0),
        (80.0, 17_000.0),
        (96.0, 18_000.0),
        (112.0, 19_000.0),
        (128.0, 19_500.0),
        (160.0, 20_000.0),
    ];
    let k = kbps_per_channel as f32;
    let hz = CURVE
        .windows(2)
        .find(|w| k <= w[1].0)
        .map_or(CURVE[CURVE.len() - 1].1, |w| if k <= w[0].0 { w[0].1 } else { w[0].1 + (w[1].1 - w[0].1) * (k - w[0].0) / (w[1].0 - w[0].0) });
    hz.min(sample_rate as f32 * 0.5 * 0.95)
}

/// A frame waiting for later frames' main data to fill the free end of its slot.
struct Pending {
    bytes: Vec<u8>,
    /// Where the main-data slot starts in `bytes`, and in the stream of all slots.
    slot_at: usize,
    slot_start: u64,
}

/// The frames written so far whose slots may still receive main data (the bit reservoir).
#[derive(Default)]
struct Reservoir {
    pending: VecDeque<Pending>,
    /// Where the next main data goes, and the end of all slots so far.
    fill: u64,
    end: u64,
    /// CRC-16 of every frame written so far (the `LAME` tag's music CRC).
    music_crc: u16,
}

impl Reservoir {
    /// Bytes of free slot the next frame's main data can begin in.
    fn free(&mut self) -> u64 {
        // Free space more than `main_data_begin` can reach back to stays unused.
        self.fill = self.fill.max(self.end.saturating_sub(MAX_RESERVOIR));
        self.end - self.fill
    }

    /// Adds a frame (header and side info) with a slot of `slot` bytes, and writes `main` from the
    /// start of the free space. `main` must fit in [`Reservoir::free`] plus `slot`.
    fn push(&mut self, head: Vec<u8>, slot: usize, main: &[u8]) {
        let slot_at = head.len();
        let mut bytes = head;
        bytes.resize(slot_at + slot, 0);
        self.pending.push_back(Pending { bytes, slot_at, slot_start: self.end });
        self.end += slot as u64;
        let mut at = self.fill;
        let mut rest = main;
        for p in &mut self.pending {
            let slot_end = p.slot_start + (p.bytes.len() - p.slot_at) as u64;
            if rest.is_empty() || at >= slot_end {
                continue;
            }
            let from = p.slot_at + (at - p.slot_start) as usize;
            let n = rest.len().min(p.bytes.len() - from);
            if let (Some(dst), Some(src)) = (p.bytes.get_mut(from..from + n), rest.get(..n)) {
                dst.copy_from_slice(src);
            }
            rest = rest.get(n..).unwrap_or_default();
            at += n as u64;
        }
        self.fill = at;
    }

    /// Writes the frames no later main data can reach (all of them when `all`).
    fn flush<W: Write>(&mut self, out: &mut W, all: bool) -> Result<()> {
        while let Some(p) = self.pending.front() {
            let slot_end = p.slot_start + (p.bytes.len() - p.slot_at) as u64;
            if !all && slot_end > self.fill.max(self.end.saturating_sub(MAX_RESERVOIR)) {
                break;
            }
            if let Some(p) = self.pending.pop_front() {
                out.write_all(&p.bytes)?;
                self.music_crc = crc16_update(self.music_crc, &p.bytes);
            }
        }
        Ok(())
    }
}

/// The MP3 encoder proper, at an MPEG-1 rate.
struct Encoder {
    channels: usize,
    sample_rate: u32,
    rate_index: u32,
    bitrate_index: u32,
    kbps: u32,
    bank: Box<Bank>,
    analysis: Vec<Box<Analysis>>,
    quant: Quantizer,
    work: Box<Work>,
    /// The quantized granules of the frame being built, `[granule][channel]`.
    granules: [[Box<Granule>; 2]; 2],
    /// LAME's slot lag: bytes owed before the next padded frame (frames are 144 × bitrate / rate
    /// bytes, plus one now and then at 44.1 kHz).
    padding: u64,
    reservoir: Reservoir,
    /// Audio frames written (not counting the `Info` frame).
    frames: u64,
    /// Whether the `Info` frame took a padding byte (its length must not change when rewritten).
    info_pad: bool,
}

impl Encoder {
    fn new(channels: usize, sample_rate: u32, kbps: u32) -> Result<Self> {
        let rate_index = RATES.iter().position(|&r| r == sample_rate).ok_or_else(|| AudioError::Unsupported(format!("MP3 at {sample_rate} Hz")))?;
        let sfb = tables::SFB_LONG[rate_index];
        Ok(Encoder {
            channels,
            sample_rate,
            rate_index: rate_index as u32,
            bitrate_index: bitrate_index(kbps)?,
            kbps,
            bank: Bank::new(),
            analysis: (0..channels).map(|_| Analysis::new()).collect(),
            quant: Quantizer::new(sfb, sample_rate, lowpass_hz(kbps / channels as u32, sample_rate)),
            work: Work::new(),
            granules: [[Granule::new(), Granule::new()], [Granule::new(), Granule::new()]],
            // LAME's slot lag starts at the fractional part, so the first frame is not padded.
            padding: (144_000 * u64::from(kbps)) % u64::from(sample_rate),
            reservoir: Reservoir::default(),
            frames: 0,
            info_pad: false,
        })
    }

    fn side_info_len(&self) -> usize {
        if self.channels == 1 { 17 } else { 32 }
    }

    /// The length of the next frame in bytes, and whether it is padded. Padding follows LAME's
    /// schedule (the first padded frame comes one frame later than a plain running total would
    /// put it): Apple's decoder estimates the frame count from the file's byte count, and one
    /// byte more than LAME writes makes it count the `Info` frame as audio.
    fn next_frame_len(&mut self) -> (usize, bool) {
        let num = 144_000 * u64::from(self.kbps);
        let rate = u64::from(self.sample_rate);
        let frac = num % rate;
        let mut pad = false;
        if frac != 0 {
            if self.padding >= frac {
                self.padding -= frac;
            } else {
                self.padding = self.padding + rate - frac;
                pad = true;
            }
        }
        ((num / rate) as usize + usize::from(pad), pad)
    }

    /// The four header bytes.
    fn header(&self, pad: bool, mid_side: bool) -> Vec<u8> {
        self.header_at(self.bitrate_index, pad, mid_side)
    }

    /// The four header bytes at bitrate index `bitrate_index`.
    fn header_at(&self, bitrate_index: u32, pad: bool, mid_side: bool) -> Vec<u8> {
        let mut h = Bits::new();
        h.put(0x7FF, 11); // sync
        h.put(0b11, 2); // MPEG-1
        h.put(0b01, 2); // Layer III
        h.put(1, 1); // no CRC
        h.put(bitrate_index, 4);
        h.put(self.rate_index, 2);
        h.put(u32::from(pad), 1);
        h.put(0, 1); // private
        h.put(if self.channels == 1 { 0b11 } else { 0b01 }, 2); // mono or joint stereo
        h.put(if mid_side { 0b10 } else { 0 }, 2);
        h.put(0, 1); // copyright
        h.put(0, 1); // original
        h.put(0, 2); // no emphasis
        h.into_bytes()
    }

    /// The `Info` frame that heads the file, written before any audio with zero counts;
    /// [`Writer::finish`] rewrites it with [`Encoder::tag_frame`].
    fn info_frame(&mut self) -> Vec<u8> {
        // Unpadded and outside the padding schedule, as LAME writes it.
        self.info_pad = false;
        self.tag_frame(0, 0, 0, 0, 0)
    }

    /// The `Info` frame with the gapless `LAME` extension: frame and byte counts, a seek table,
    /// the encoder `delay` and end `padding` in samples, and the tag's CRC. Decoders (ffmpeg,
    /// Apple's, DJ software) trim exactly `delay` + 529 samples at the start and `padding` - 529
    /// at the end, so the decoded file has the input's length and timing. They read the delay
    /// only from a tag in LAME's documented layout and marked as such, hence the version string.
    fn tag_frame(&self, frames: u32, bytes: u32, delay: u32, padding: u32, music_crc: u16) -> Vec<u8> {
        // The tag needs 160 bytes plus the side information. At low bitrates a frame is smaller,
        // so the tag frame (which decoders skip) takes the lowest bitrate that holds it, as LAME's does.
        let need = (4 + self.side_info_len() + 156 + 36) as u64;
        let rate = u64::from(self.sample_rate);
        let frame_len = |kbps: u32| 144_000 * u64::from(kbps) / rate;
        let (index, kbps) = if frame_len(self.kbps) >= need {
            (self.bitrate_index, self.kbps)
        } else {
            BITRATES.iter().position(|&k| frame_len(k) >= need).map_or((self.bitrate_index, self.kbps), |i| (i as u32 + 1, BITRATES[i]))
        };
        let pad = self.info_pad && kbps == self.kbps;
        let len = frame_len(kbps) as usize + usize::from(pad);
        let mut f = self.header_at(index, pad, false);
        f.resize(4 + self.side_info_len(), 0);
        f.extend_from_slice(b"Info");
        f.extend_from_slice(&0x0Fu32.to_be_bytes()); // frames, bytes, seek table and quality follow
        f.extend_from_slice(&frames.to_be_bytes());
        f.extend_from_slice(&bytes.to_be_bytes());
        // Constant bitrate: the seek table is a straight line.
        f.extend((0..100u32).map(|i| (i * 256 / 100) as u8));
        f.extend_from_slice(&0u32.to_be_bytes()); // quality
        f.extend_from_slice(b"LAME3.100");
        f.push(0x01); // tag revision 0, constant bitrate
        f.push(0); // lowpass unknown
        f.extend_from_slice(&[0; 4]); // peak amplitude unknown
        f.extend_from_slice(&[0; 4]); // replay gain unknown
        f.push(0); // encoding flags, ATH type
        f.push(self.kbps.min(255) as u8);
        let (d, p) = (delay.min(4095), padding.min(4095));
        f.push((d >> 4) as u8);
        f.push((((d & 0x0F) << 4) | (p >> 8)) as u8);
        f.push((p & 0xFF) as u8);
        let source = match self.sample_rate {
            44_100 => 1,
            48_000 => 2,
            _ => 0,
        };
        let mode = if self.channels == 2 { 3 } else { 0 }; // joint stereo / mono
        f.push((source << 6) | (mode << 2));
        f.push(0); // MP3 gain
        f.extend_from_slice(&[0; 2]); // preset, surround
        f.extend_from_slice(&bytes.to_be_bytes()); // music length, from this frame on
        f.extend_from_slice(&music_crc.to_be_bytes()); // CRC of every audio frame after this one
        let crc = crc16(&f);
        f.extend_from_slice(&crc.to_be_bytes());
        f.resize(len, 0);
        f
    }

    /// Encodes one frame of `FRAME` samples per channel and queues it.
    fn frame(&mut self, pcm: &[Vec<f32>]) {
        let (len, pad) = self.next_frame_len();
        let side = self.side_info_len();
        let slot = len - 4 - side;
        let free = self.reservoir.free();

        let mut xr = [[[0.0f32; GRANULE]; 2]; 2];
        let mut input = [0.0f32; GRANULE];
        for gr in 0..2 {
            for ch in 0..self.channels {
                for (i, x) in input.iter_mut().enumerate() {
                    *x = pcm.get(ch).and_then(|c| c.get(gr * GRANULE + i)).copied().unwrap_or(0.0) * INPUT_GAIN;
                }
                self.analysis[ch].granule(&self.bank, &input, &mut xr[gr][ch]);
            }
        }
        // Mid/side unless the channels are mostly out of phase.
        let mid_side = self.channels == 2 && {
            let (mut mid, mut side) = (0.0f32, 0.0f32);
            for g in &xr {
                for (l, r) in g[0].iter().zip(&g[1]) {
                    mid += (l + r) * (l + r);
                    side += (l - r) * (l - r);
                }
            }
            side <= mid
        };
        if mid_side {
            for g in &mut xr {
                let [l, r] = g;
                for (l, r) in l.iter_mut().zip(r.iter_mut()) {
                    let (m, s) = ((*l + *r) * std::f32::consts::FRAC_1_SQRT_2, (*l - *r) * std::f32::consts::FRAC_1_SQRT_2);
                    *l = m;
                    *r = s;
                }
            }
        }

        // Each granule gets its share of the frame plus half of what the reservoir holds then, and
        // spends at least what the reservoir couldn't keep for later.
        let mean = (slot * 8 / 2) as i64;
        let mut avail = ((free as usize + slot) * 8) as i64;
        for gr in 0..2 {
            let masks: Vec<_> = (0..self.channels).map(|ch| self.quant.mask(&mut xr[gr][ch])).collect();
            let later = mean * (1 - gr as i64);
            let mut budget = (mean + (avail - later - mean).max(0) / 2).min(avail);
            let mut floor = (avail - later - MAX_RESERVOIR as i64 * 8).max(0);
            let total_pe: f32 = masks.iter().map(|m| m.pe.max(1.0)).sum();
            for ch in 0..self.channels {
                let share = if ch + 1 == self.channels { 1.0 } else { (masks[ch].pe.max(1.0) / total_pe).clamp(0.2, 0.8) };
                let max = ((budget as f32 * share) as i64).clamp(0, 4095) as u32;
                let min = ((floor as f32 * share) as i64).clamp(0, i64::from(max)) as u32;
                let g = &mut self.granules[gr][ch];
                self.quant.quantize(&xr[gr][ch], &masks[ch], min, max, &mut self.work, g);
                budget -= i64::from(g.bits());
                floor -= i64::from(g.bits());
                avail -= i64::from(g.bits());
            }
        }

        let mut main = Bits::new();
        for gr in 0..2 {
            for ch in 0..self.channels {
                let g = &self.granules[gr][ch];
                let (s1, s2) = tables::SLEN[g.scalefac_compress as usize];
                for (b, &sf) in g.sf.iter().enumerate() {
                    main.put(u32::from(sf), if b < 11 { s1 } else { s2 });
                }
                huffman::write(&mut main, &g.ix, &g.neg, &g.coding, &self.quant.sfb);
            }
        }
        debug_assert_eq!(main.bits(), self.granules.iter().flat_map(|g| g[..self.channels].iter()).map(|g| u64::from(g.bits())).sum::<u64>());

        let mut head = Bits::new();
        head.put(free as u32, 9); // main_data_begin
        head.put(0, if self.channels == 1 { 5 } else { 3 }); // private bits
        head.put(0, 4 * self.channels as u32); // scfsi
        for gr in 0..2 {
            for ch in 0..self.channels {
                let g = &self.granules[gr][ch];
                head.put(g.bits(), 12);
                head.put(g.coding.big_values as u32, 9);
                head.put(g.global_gain, 8);
                head.put(g.scalefac_compress, 4);
                head.put(0, 1); // long blocks
                for t in g.coding.table_select {
                    head.put(t, 5);
                }
                head.put(g.coding.region0_count, 4);
                head.put(g.coding.region1_count, 3);
                head.put(0, 1); // preflag
                head.put(0, 1); // scalefac_scale
                head.put(u32::from(g.coding.count1_b), 1);
            }
        }
        let mut bytes = self.header(pad, mid_side);
        bytes.extend(head.into_bytes());
        self.reservoir.push(bytes, slot, &main.into_bytes());
        self.frames += 1;
    }
}

/// CRC-16 (polynomial 0x8005, reflected, initial 0) as the LAME tag uses it.
fn crc16(data: &[u8]) -> u16 {
    crc16_update(0, data)
}

/// Continues a [`crc16`] over more bytes.
fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

/// An MP3 file written block by block; [`Writer::finish`] fills in the `Info` frame's counts.
pub(crate) struct Writer<W> {
    out: W,
    start: u64,
    enc: Box<Encoder>,
    from_rate: u32,
    resampler: Option<Resampler>,
    /// Input not yet encoded, per channel, at the MP3 rate.
    pcm: Vec<Vec<f32>>,
    /// Input frames received.
    received: u64,
    /// Samples per channel at the MP3 rate handed to the encoder (before the end padding).
    real: u64,
    /// Set when writing frames has failed: the sink may hold some of them while the reservoir
    /// has let them go, so the writer refuses everything after (see [`crate::failed_write_error`]).
    failed: bool,
}

impl<W: Write + Seek> Writer<W> {
    /// Starts a constant-bitrate MP3 of `kbps` (one of [`BITRATES`]; `None` is
    /// [`DEFAULT_KBPS`]). MP3 holds one or two channels.
    pub(crate) fn new(mut out: W, channels: usize, sample_rate: u32, kbps: Option<u32>) -> Result<Self> {
        validate_layout(channels, sample_rate)?;
        if channels > 2 {
            return Err(AudioError::Unsupported(format!("MP3 holds mono or stereo, not {channels} channels")));
        }
        let rate = mp3_rate(sample_rate);
        let mut enc = Box::new(Encoder::new(channels, rate, kbps.unwrap_or(DEFAULT_KBPS))?);
        let start = out.stream_position()?;
        out.write_all(&enc.info_frame())?;
        Ok(Writer {
            out,
            start,
            enc,
            from_rate: sample_rate,
            resampler: (rate != sample_rate).then(|| Resampler::new(channels, sample_rate, rate)),
            pcm: vec![Vec::with_capacity(2 * FRAME); channels],
            received: 0,
            real: 0,
            failed: false,
        })
    }

    /// The frame count after `frames` more, or an error past what the `Info` frame can count
    /// (a 4 GiB file).
    pub(crate) fn check_room(&self, frames: u64) -> Result<u64> {
        let total = self.received.saturating_add(frames);
        let at_rate = (u128::from(total) * u128::from(self.enc.sample_rate)).div_ceil(u128::from(self.from_rate));
        let mp3_frames = at_rate / FRAME as u128 + 3;
        let bytes = mp3_frames * (144_000 * u128::from(self.enc.kbps) / u128::from(self.enc.sample_rate) + 1);
        if bytes > u128::from(u32::MAX) {
            return Err(AudioError::TooLarge("too long for an MP3 file".into()));
        }
        Ok(total)
    }

    /// Appends `frames` frames of planar audio (missing samples are silence), unless they don't
    /// fit (see [`Writer::check_room`]).
    pub(crate) fn write(&mut self, block: &[Vec<f32>], frames: usize) -> Result<()> {
        if self.failed {
            return Err(crate::failed_write_error());
        }
        let total = self.check_room(frames as u64)?;
        let clamped: Vec<Vec<f32>> = (0..self.enc.channels)
            .map(|c| {
                let src = block.get(c).map_or(&[][..], |b| b.get(..frames.min(b.len())).unwrap_or_default());
                let mut v: Vec<f32> = src.iter().map(|&x| clamp_float(x)).collect();
                v.resize(frames, 0.0);
                v
            })
            .collect();
        let before = self.pcm.first().map_or(0, Vec::len);
        match &mut self.resampler {
            Some(r) => r.push(&clamped, frames, &mut self.pcm),
            None => {
                for (p, c) in self.pcm.iter_mut().zip(clamped) {
                    p.extend(c);
                }
            }
        }
        self.real += self.pcm.first().map_or(0, Vec::len).saturating_sub(before) as u64;
        self.received = total;
        self.encode_ready()?;
        let flushed = self.enc.reservoir.flush(&mut self.out, false);
        self.failed = flushed.is_err();
        flushed
    }

    /// Encodes every whole frame of pending input.
    fn encode_ready(&mut self) -> Result<()> {
        let mut at = 0;
        while self.pcm.first().is_some_and(|p| p.len() >= at + FRAME) {
            let frame: Vec<Vec<f32>> = self.pcm.iter().map(|p| p.get(at..at + FRAME).unwrap_or_default().to_vec()).collect();
            self.enc.frame(&frame);
            at += FRAME;
        }
        for p in &mut self.pcm {
            p.drain(..at.min(p.len()));
        }
        Ok(())
    }

    /// Encodes the rest, completes the `Info` frame and hands back `out`, positioned at the end
    /// of the file.
    pub(crate) fn finish(mut self) -> Result<W> {
        if self.failed {
            return Err(crate::failed_write_error());
        }
        if let Some(r) = &mut self.resampler {
            let before = self.pcm.first().map_or(0, Vec::len);
            r.finish(&mut self.pcm);
            self.real += self.pcm.first().map_or(0, Vec::len).saturating_sub(before) as u64;
        }
        let held = self.pcm.first().map_or(0, Vec::len) + TAIL;
        let padded = held.div_ceil(FRAME) * FRAME;
        for p in &mut self.pcm {
            p.resize(padded, 0.0);
        }
        self.encode_ready()?;
        self.enc.reservoir.flush(&mut self.out, true)?;
        let end = self.out.stream_position()?;
        let frames = u32::try_from(self.enc.frames).map_err(|_| AudioError::TooLarge("too many MP3 frames".into()))?;
        let bytes = u32::try_from(end.saturating_sub(self.start)).map_err(|_| AudioError::TooLarge("too long for an MP3 file".into()))?;
        // Everything decoded after the encoder delay that isn't input is end padding.
        let total = self.enc.frames.saturating_mul(FRAME as u64);
        let padding = u32::try_from(total.saturating_sub(ENC_DELAY + self.real)).unwrap_or(u32::MAX);
        let tag = self.enc.tag_frame(frames, bytes, ENC_DELAY as u32, padding, self.enc.reservoir.music_crc);
        self.out.seek(SeekFrom::Start(self.start))?;
        self.out.write_all(&tag)?;
        self.out.seek(SeekFrom::End(0))?;
        self.out.flush()?;
        Ok(self.out)
    }
}

pub(crate) fn encode(buf: &AudioBuffer, kbps: Option<u32>) -> Result<Vec<u8>> {
    crate::pcm::validate_buffer(buf)?;
    let frames = buf.frames();
    let mut w = Writer::new(Cursor::new(Vec::new()), buf.channels.len(), buf.sample_rate, kbps)?;
    w.write(&buf.channels, frames)?;
    Ok(w.finish()?.into_inner())
}
