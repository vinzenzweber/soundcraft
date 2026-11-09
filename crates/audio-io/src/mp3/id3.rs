//! A minimal ID3v2.3 tag (title, artist, BPM) for the head of an MP3. Version 2.3 rather than 2.4
//! because older DJ players and car stereos read it more reliably. Layout from the public ID3v2.3
//! specification: a 10-byte header with a syncsafe size, then text frames with plain big-endian
//! sizes, no unsynchronisation and no padding.

use crate::Id3Tags;

/// Characters kept per text field; the rest is dropped (a title, not a document).
const MAX_CHARS: usize = 256;

/// The tag bytes, or nothing when every field is empty.
pub(super) fn tag(tags: &Id3Tags) -> Vec<u8> {
    let mut frames = Vec::new();
    text_frame(&mut frames, b"TIT2", &tags.title);
    text_frame(&mut frames, b"TPE1", &tags.artist);
    if let Some(bpm) = tags.bpm.filter(|b| (1..=999).contains(b)) {
        text_frame(&mut frames, b"TBPM", &bpm.to_string());
    }
    if frames.is_empty() {
        return Vec::new();
    }
    // At most three frames of MAX_CHARS UTF-16 units: far below the 28-bit syncsafe limit.
    let size = u32::try_from(frames.len()).unwrap_or(0x0FFF_FFFF).min(0x0FFF_FFFF);
    let mut out = Vec::with_capacity(10 + frames.len());
    out.extend_from_slice(b"ID3");
    out.extend_from_slice(&[3, 0, 0]);
    out.extend_from_slice(&[(size >> 21) as u8 & 0x7F, (size >> 14) as u8 & 0x7F, (size >> 7) as u8 & 0x7F, size as u8 & 0x7F]);
    out.extend_from_slice(&frames);
    out
}

/// Appends a text frame for `text`, without control characters and capped at [`MAX_CHARS`]:
/// ISO-8859-1 when every character fits, UTF-16 with a byte-order mark otherwise.
fn text_frame(out: &mut Vec<u8>, id: &[u8; 4], text: &str) {
    let text: String = text.chars().filter(|c| !c.is_control()).take(MAX_CHARS).collect();
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let mut body = Vec::new();
    if text.chars().all(|c| u32::from(c) <= 0xFF) {
        body.push(0);
        body.extend(text.chars().map(|c| u32::from(c) as u8));
    } else {
        body.push(1);
        body.extend_from_slice(&[0xFF, 0xFE]);
        body.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    }
    out.extend_from_slice(id);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&body);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(title: &str, artist: &str, bpm: Option<u32>) -> Id3Tags {
        Id3Tags { title: title.into(), artist: artist.into(), bpm }
    }

    #[test]
    fn writes_a_v23_header_and_latin1_frames() {
        let t = tag(&tags("Traag Intro", "Bizzey", Some(98)));
        assert_eq!(&t[..6], b"ID3\x03\x00\x00");
        let size = t[6..10].iter().fold(0usize, |a, &b| (a << 7) | usize::from(b));
        assert_eq!(size, t.len() - 10);
        let frames = &t[10..];
        assert_eq!(&frames[..4], b"TIT2");
        assert_eq!(&frames[4..8], &12u32.to_be_bytes());
        assert_eq!(&frames[10..22], b"\x00Traag Intro");
        assert!(t.windows(4).any(|w| w == b"TPE1"));
        assert!(t.windows(7).any(|w| w == b"TBPM\x00\x00\x00"));
        assert!(t.ends_with(b"\x0098"));
    }

    #[test]
    fn text_beyond_latin1_is_utf16_with_a_bom() {
        let t = tag(&tags("Qué tengo — DJ", "", None));
        let body = &t[20..];
        assert_eq!(&body[..3], &[1, 0xFF, 0xFE]);
        let units: Vec<u16> = body[3..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "Qué tengo — DJ");
    }

    #[test]
    fn empty_fields_write_no_tag_and_hostile_text_is_bounded() {
        assert!(tag(&tags("", "  ", None)).is_empty());
        assert!(tag(&tags("", "", Some(0))).is_empty(), "a BPM of 0 is left out");
        let t = tag(&tags(&"x\u{0}\n".repeat(10_000), "", Some(5000)));
        assert_eq!(t.len(), 10 + 10 + 1 + MAX_CHARS, "control characters dropped, length capped, BPM out of range skipped");
    }
}
