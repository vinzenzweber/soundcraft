//! Lyrics transcribed from a vocal: timed lines on the session timeline, with the chorus marked.

use crate::ClipId;
use soundcraft_time::Samples;

/// One line of lyrics, in session samples.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LyricLine {
    pub start: Samples,
    pub end: Samples,
    pub text: String,
    /// Part of a chorus (the block of lines that repeats most).
    #[serde(default)]
    pub chorus: bool,
}

/// The session's lyrics.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Lyrics {
    #[serde(default)]
    pub lines: Vec<LyricLine>,
    /// Highlight the chorus lines in the Lyrics ruler (on by default).
    #[serde(default = "yes")]
    pub highlight_chorus: bool,
    /// The clip the lyrics were transcribed from.
    #[serde(default)]
    pub clip: Option<ClipId>,
    /// The detected or chosen language code (`es`, `en`, ...).
    #[serde(default)]
    pub language: String,
    /// The speech model that wrote them.
    #[serde(default)]
    pub model: String,
}

fn yes() -> bool {
    true
}

impl Default for Lyrics {
    fn default() -> Self {
        Lyrics { lines: Vec::new(), highlight_chorus: true, clip: None, language: String::new(), model: String::new() }
    }
}

/// The most lines a session keeps (an hour of dense rap is a few thousand).
pub const MAX_LYRIC_LINES: usize = 20_000;
/// The longest line kept, in characters.
pub const MAX_LYRIC_CHARS: usize = 500;

impl Lyrics {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The chorus as time ranges: each run of consecutive chorus lines, split where two lines are
    /// more than `max_gap` apart.
    pub fn chorus_ranges(&self, max_gap: Samples) -> Vec<(Samples, Samples)> {
        let mut out: Vec<(Samples, Samples)> = Vec::new();
        let mut open = false;
        for l in &self.lines {
            if l.chorus {
                match out.last_mut() {
                    Some(r) if open && l.start.saturating_sub(r.1) <= max_gap => r.1 = r.1.max(l.end),
                    _ => out.push((l.start, l.end)),
                }
                open = true;
            } else {
                open = false;
            }
        }
        out
    }

    /// Move every line at or after `at` by `by` samples (time inserted or removed).
    pub fn shift_from(&mut self, at: Samples, by: Samples) {
        for l in self.lines.iter_mut().filter(|l| l.start >= at) {
            l.start = l.start.saturating_add(by).max(0);
            l.end = l.end.saturating_add(by).max(l.start);
        }
    }

    /// Cap counts and lengths and order the lines; used when a session file is loaded.
    pub fn sanitize(&mut self, bound: impl Fn(&mut Samples)) {
        self.lines.truncate(MAX_LYRIC_LINES);
        for l in &mut self.lines {
            bound(&mut l.start);
            bound(&mut l.end);
            if l.end < l.start {
                l.end = l.start;
            }
            if l.text.chars().count() > MAX_LYRIC_CHARS {
                l.text = l.text.chars().take(MAX_LYRIC_CHARS).collect();
            }
        }
        self.lines.sort_by_key(|l| l.start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start: Samples, end: Samples, chorus: bool) -> LyricLine {
        LyricLine { start, end, text: "x".into(), chorus }
    }

    #[test]
    fn chorus_ranges_join_consecutive_lines() {
        let l = Lyrics {
            lines: vec![line(0, 10, false), line(10, 20, true), line(20, 30, true), line(30, 40, false), line(50, 60, true)],
            ..Lyrics::default()
        };
        assert_eq!(l.chorus_ranges(100), vec![(10, 30), (50, 60)]);
        let far = Lyrics { lines: vec![line(0, 10, true), line(50, 60, true)], ..Lyrics::default() };
        assert_eq!(far.chorus_ranges(5), vec![(0, 10), (50, 60)]);
        assert_eq!(far.chorus_ranges(100), vec![(0, 60)]);
    }

    #[test]
    fn shift_moves_only_later_lines() {
        let mut l = Lyrics { lines: vec![line(0, 10, false), line(100, 110, true)], ..Lyrics::default() };
        l.shift_from(50, 1000);
        assert_eq!((l.lines[0].start, l.lines[1].start, l.lines[1].end), (0, 1100, 1110));
    }

    #[test]
    fn old_sessions_load_with_chorus_highlight_on() {
        let l: Lyrics = serde_json::from_str("{}").unwrap();
        assert!(l.highlight_chorus);
        assert!(l.is_empty());
    }
}
