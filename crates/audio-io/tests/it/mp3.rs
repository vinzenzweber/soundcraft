//! The MP3 encoder, checked by decoding its output with symphonia.

use soundcraft_audio_io::{AudioBuffer, EncodeOptions, FileEncoder, FileFormat, MP3_BITRATES, SampleFormat, decode, detect_format, encode, probe};
use std::f32::consts::TAU;
use std::io::Cursor;

/// Where the input starts in the decoded output: the gapless `LAME` tag tells the decoder to skip
/// the encoder's delay and the decoder's own, so nothing.
const DELAY: usize = 0;
/// The encoder delay the tag declares (the decoder adds its 529).
const ENC_DELAY: u32 = 528;

fn mp3(kbps: Option<u32>) -> EncodeOptions {
    EncodeOptions { format: FileFormat::Mp3, bitrate_kbps: kbps, ..EncodeOptions::default() }
}

/// A chord with a slow swell per channel, a little detuned between channels.
fn music(sample_rate: u32, channels: usize, secs: f32) -> AudioBuffer {
    let n = (sample_rate as f32 * secs) as usize;
    let chans = (0..channels)
        .map(|c| {
            (0..n)
                .map(|i| {
                    let t = i as f32 / sample_rate as f32;
                    let swell = 0.6 + 0.4 * (t * 3.0 + c as f32).sin();
                    let chord: f32 = [220.0f32, 277.2, 329.6, 1760.0]
                        .iter()
                        .enumerate()
                        .map(|(k, f)| (t * TAU * f * (1.0 + 0.002 * c as f32)).sin() / (k + 2) as f32)
                        .sum();
                    0.5 * swell * chord
                })
                .collect()
        })
        .collect();
    AudioBuffer { sample_rate, channels: chans }
}

/// Signal-to-noise ratio (dB) of `out` against `src` delayed by [`DELAY`], away from the ends.
fn snr(src: &[f32], out: &[f32]) -> f64 {
    let range = 4096..src.len() - 4096;
    let signal: f64 = range.clone().map(|i| f64::from(src[i]).powi(2)).sum();
    let noise: f64 = range.map(|i| (f64::from(out[i + DELAY]) - f64::from(src[i])).powi(2)).sum();
    10.0 * (signal / noise).log10()
}

#[test]
fn round_trips_at_the_expected_quality() {
    let src = music(44_100, 2, 3.0);
    for (kbps, min_db) in [(320, 50.0), (192, 42.0), (128, 40.0), (64, 35.0)] {
        let bytes = encode(&src, &mp3(Some(kbps))).unwrap();
        assert_eq!(detect_format(&bytes, None), FileFormat::Mp3);
        let (info, out) = decode(&bytes, None).unwrap();
        assert_eq!((info.format, info.sample_format, info.sample_rate, info.channels), (FileFormat::Mp3, SampleFormat::Compressed, 44_100, 2));
        assert_eq!(out.frames(), src.frames(), "{kbps}: gapless decoding gives the input's length");
        for c in 0..2 {
            let db = snr(&src.channels[c], &out.channels[c]);
            assert!(db > min_db, "{kbps} kbps channel {c}: {db:.1} dB");
        }
        // Constant bitrate: the size follows the bitrate.
        let kbps_seen = bytes.len() as f64 * 8.0 / (src.frames() as f64 / 44_100.0) / 1000.0;
        assert!((kbps_seen / f64::from(kbps) - 1.0).abs() < 0.06, "{kbps} kbps came out at {kbps_seen:.1}");
    }
}

#[test]
fn every_rate_and_bitrate_decodes() {
    for sr in [32_000, 44_100, 48_000] {
        for channels in [1, 2] {
            let src = music(sr, channels, 0.5);
            for kbps in MP3_BITRATES {
                let bytes = encode(&src, &mp3(Some(kbps))).unwrap();
                let (info, out) = decode(&bytes, None).unwrap();
                assert_eq!((info.sample_rate, usize::from(info.channels)), (sr, channels), "{sr} Hz {kbps} kbps");
                let db = snr(&src.channels[0], &out.channels[0]);
                assert!(db > 15.0, "{sr} Hz, {channels} ch, {kbps} kbps: {db:.1} dB");
            }
        }
    }
}

#[test]
fn other_rates_are_resampled() {
    for (from, to) in [(96_000, 48_000), (88_200, 44_100), (192_000, 48_000), (22_050, 44_100), (8_000, 48_000)] {
        // A 1 kHz tone keeps its pitch and level.
        let n = from as usize / 2;
        let tone: Vec<f32> = (0..n).map(|i| 0.5 * (i as f32 * TAU * 1000.0 / from as f32).sin()).collect();
        let bytes = encode(&AudioBuffer { sample_rate: from, channels: vec![tone] }, &mp3(Some(192))).unwrap();
        let (info, out) = decode(&bytes, None).unwrap();
        assert_eq!(info.sample_rate, to, "{from}");
        let expected = n * to as usize / from as usize;
        assert!(out.frames().abs_diff(expected) <= 2, "{from}: {} frames, expected {expected}", out.frames());
        let body = &out.channels[0][DELAY + 2000..DELAY + expected - 2000];
        let crossings = body.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count() as f32;
        let hz = crossings * to as f32 / body.len() as f32;
        assert!((hz - 1000.0).abs() < 10.0, "{from}: {hz} Hz");
        let rms = (body.iter().map(|x| x * x).sum::<f32>() / body.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "{from}: rms {rms}");
    }
}

#[test]
fn streaming_matches_whole_buffer_encoding() {
    let src = music(48_000, 2, 1.3);
    let whole = encode(&src, &mp3(None)).unwrap();
    let mut enc = FileEncoder::new(Cursor::new(Vec::new()), &mp3(None), 2, 48_000, None).unwrap();
    let mut at = 0;
    for size in [1usize, 1000, 1152, 7, 40_000].iter().cycle() {
        let n = (*size).min(src.frames() - at);
        let block: Vec<Vec<f32>> = src.channels.iter().map(|c| c[at..at + n].to_vec()).collect();
        enc.write(&block, n).unwrap();
        at += n;
        if at == src.frames() {
            break;
        }
    }
    assert_eq!(enc.finish().unwrap().into_inner(), whole);
}

#[test]
fn the_info_frame_counts_the_audio() {
    let src = music(44_100, 1, 2.0);
    let bytes = encode(&src, &mp3(Some(128))).unwrap();
    let info = probe(&bytes, None).unwrap();
    let frames = u32::from_be_bytes(bytes[4 + 17 + 8..4 + 17 + 12].try_into().unwrap());
    let total = u32::from_be_bytes(bytes[4 + 17 + 12..4 + 17 + 16].try_into().unwrap());
    assert_eq!(&bytes[4 + 17..4 + 17 + 4], b"Info");
    assert_eq!(total as usize, bytes.len());
    // The frame count covers the audio frames, which hold the delay, the input and the padding.
    assert_eq!(info.frames, src.frames() as u64);
    let (delay, padding) = lame_delay_padding(&bytes, 17);
    assert_eq!(delay, ENC_DELAY);
    assert_eq!(u64::from(frames) * 1152, u64::from(delay) + src.frames() as u64 + u64::from(padding));
}

/// The LAME tag's encoder delay and padding, with `side` bytes of side information.
fn lame_delay_padding(bytes: &[u8], side: usize) -> (u32, u32) {
    let lame = 4 + side + 4 + 4 + 4 + 4 + 100 + 4;
    assert_eq!(&bytes[lame..lame + 4], b"LAME");
    let d = &bytes[lame + 21..lame + 24];
    ((u32::from(d[0]) << 4) | (u32::from(d[1]) >> 4), ((u32::from(d[1]) & 0x0F) << 8) | u32::from(d[2]))
}

/// CRC-16 as the LAME tag uses it (polynomial 0x8005 reflected, initial 0).
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xA001 } else { crc >> 1 };
        }
    }
    crc
}

#[test]
fn the_gapless_tag_is_complete_and_checksummed() {
    for (sr, channels, side) in [(44_100, 2, 32), (48_000, 2, 32), (44_100, 1, 17)] {
        // (Low bitrates are covered by every_rate_and_bitrate_decodes: the tag frame grows to fit.)
        let src = music(sr, channels, 1.7);
        let bytes = encode(&src, &mp3(Some(320))).unwrap();
        let lame = 4 + side + 4 + 4 + 4 + 4 + 100 + 4;
        // The tag's own CRC covers everything before it; the music CRC every frame after this one.
        let tag_crc = u16::from_be_bytes([bytes[lame + 34], bytes[lame + 35]]);
        assert_eq!(tag_crc, crc16(&bytes[..lame + 34]), "{sr} Hz {channels} ch");
        let frame_len = 144_000 * 320 / sr as usize; // 320 kbps: the tag frame is a normal frame
        let music_crc = u16::from_be_bytes([bytes[lame + 32], bytes[lame + 33]]);
        assert_eq!(music_crc, crc16(&bytes[frame_len..]), "{sr} Hz {channels} ch");
        let (delay, _) = lame_delay_padding(&bytes, side);
        assert_eq!(delay, ENC_DELAY);
        // The decoded audio lines up with the input to the sample.
        let (_, out) = decode(&bytes, None).unwrap();
        assert_eq!(out.frames(), src.frames());
        let db = snr(&src.channels[0], &out.channels[0]);
        assert!(db > 40.0, "{sr} Hz {channels} ch: {db:.1} dB");
    }
}

#[test]
fn edge_cases_encode() {
    // Nothing, one sample, silence, clipping, non-finite samples and a click all make valid files.
    let mut wild = vec![0.0f32; 5000];
    wild[10] = f32::NAN;
    wild[11] = f32::INFINITY;
    wild[12] = f32::NEG_INFINITY;
    wild[2000] = 1.0;
    for (i, x) in wild.iter_mut().enumerate().skip(3000) {
        *x = if i % 2 == 0 { 8.0 } else { -8.0 };
    }
    for channels in [vec![vec![]], vec![vec![0.25]], vec![vec![0.0; 10_000]; 2], vec![wild]] {
        let src = AudioBuffer { sample_rate: 48_000, channels };
        let bytes = encode(&src, &mp3(Some(32))).unwrap();
        let (info, out) = decode(&bytes, None).unwrap();
        assert!(info.frames >= (src.frames() + DELAY) as u64);
        assert!(out.channels.iter().flatten().all(|x| x.is_finite() && x.abs() <= 1.5));
    }
    let (_, out) = decode(&encode(&AudioBuffer::new(44_100, 2, 5000), &mp3(None)).unwrap(), None).unwrap();
    assert!(out.channels.iter().flatten().all(|&x| x == 0.0), "silence stays silent");
}

#[test]
fn refuses_what_mp3_cannot_hold() {
    let surround = music(48_000, 6, 0.1);
    assert!(encode(&surround, &mp3(None)).unwrap_err().to_string().contains("mono or stereo"));
    let stereo = music(48_000, 2, 0.1);
    for kbps in [0, 8, 129, 384] {
        assert!(encode(&stereo, &mp3(Some(kbps))).unwrap_err().to_string().contains("bitrate"), "{kbps}");
    }
    assert!(encode(&AudioBuffer::default(), &mp3(None)).is_err());
}
