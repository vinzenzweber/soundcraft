//! Library search: one query over the shown columns.
//!
//! Plain words match any shown column. `120-124` is a BPM range and `8A` a key (each also
//! matches as plain text, so "69" still finds the title "69"). `column:value` searches one
//! column, shown or not (`artist:feid`, `bpm:>125`, `time:<4:00`), and `-word` leaves tracks out.
//! Every word must match.

use super::Track;
use serde::{Deserialize, Serialize};

/// Longest query read, in characters; the rest is ignored.
pub const MAX_QUERY_CHARS: usize = 512;
/// Most terms read from one query; the rest are ignored.
pub const MAX_TERMS: usize = 32;

/// A library column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Column {
    Title,
    Artist,
    Bpm,
    Key,
    Genre,
    Time,
    Rating,
    Comment,
}

impl Column {
    pub const ALL: [Column; 8] =
        [Column::Title, Column::Artist, Column::Bpm, Column::Key, Column::Genre, Column::Time, Column::Rating, Column::Comment];
    /// The columns shown until the user picks others.
    pub const DEFAULT: [Column; 4] = [Column::Title, Column::Artist, Column::Bpm, Column::Key];

    pub const fn id(self) -> &'static str {
        match self {
            Column::Title => "title",
            Column::Artist => "artist",
            Column::Bpm => "bpm",
            Column::Key => "key",
            Column::Genre => "genre",
            Column::Time => "time",
            Column::Rating => "rating",
            Column::Comment => "comment",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Column::Title => "Track Title",
            Column::Artist => "Artist",
            Column::Bpm => "BPM",
            Column::Key => "Key",
            Column::Genre => "Genre",
            Column::Time => "Time",
            Column::Rating => "Rating",
            Column::Comment => "Comment",
        }
    }

    pub fn parse(s: &str) -> Option<Column> {
        Column::ALL.into_iter().find(|c| c.id().eq_ignore_ascii_case(s.trim()))
    }

    /// The text this column shows for `t`.
    pub fn text(self, t: &Track) -> String {
        match self {
            Column::Title => t.title.clone(),
            Column::Artist => t.artist.clone(),
            Column::Bpm => t.bpm().map(|b| b.to_string()).unwrap_or_default(),
            Column::Key => t.key().unwrap_or_default().to_string(),
            Column::Genre => t.genre.clone(),
            Column::Time => format_time(t.seconds),
            Column::Rating => match t.rating() {
                0 => String::new(),
                r => r.to_string(),
            },
            Column::Comment => t.comment().to_string(),
        }
    }
}

/// `m:ss`, or `h:mm:ss` from an hour.
pub fn format_time(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Seconds from `m:ss`, `h:mm:ss` or plain seconds.
pub fn parse_time(s: &str) -> Option<u32> {
    let mut total: u32 = 0;
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for p in parts {
        let n: u32 = p.parse().ok()?;
        total = total.checked_mul(60)?.checked_add(n)?;
    }
    Some(total)
}

/// How a number is compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Gt,
    Lt,
    Between(u32, u32),
}

impl Cmp {
    fn holds(self, v: u32, n: u32) -> bool {
        match self {
            Cmp::Eq => v == n,
            Cmp::Gt => v > n,
            Cmp::Lt => v < n,
            Cmp::Between(lo, hi) => v >= lo.min(hi) && v <= lo.max(hi),
        }
    }

    fn parse(s: &str, number: impl Fn(&str) -> Option<u32>) -> Option<(Cmp, u32)> {
        if let Some(rest) = s.strip_prefix('>') {
            return Some((Cmp::Gt, number(rest)?));
        }
        if let Some(rest) = s.strip_prefix('<') {
            return Some((Cmp::Lt, number(rest)?));
        }
        if let Some((a, b)) = s.split_once('-')
            && !a.is_empty()
        {
            let (a, b) = (number(a)?, number(b)?);
            return Some((Cmp::Between(a, b), a));
        }
        Some((Cmp::Eq, number(s)?))
    }

    fn describe(self, n: u32, show: impl Fn(u32) -> String) -> String {
        match self {
            Cmp::Eq => show(n),
            Cmp::Gt => format!("over {}", show(n)),
            Cmp::Lt => format!("under {}", show(n)),
            Cmp::Between(a, b) => format!("{}–{}", show(a.min(b)), show(a.max(b))),
        }
    }
}

/// One word of a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// Text in any shown column.
    Any(String),
    /// Text in one column.
    In(Column, String),
    /// A BPM, range or bound; `auto` when it came from a bare number, which also matches as text.
    Bpm { cmp: Cmp, n: u32, auto: Option<String> },
    /// A key like `8A`; `auto` when it came from a bare word, which also matches as text.
    Key { key: String, auto: bool },
    /// A length or bound, in seconds.
    Time { cmp: Cmp, n: u32 },
    /// Stars (0–5), or a bound.
    Rating { cmp: Cmp, n: u32 },
}

/// A query word, maybe negated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub not: bool,
    pub term: Term,
}

/// A parsed query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub words: Vec<Word>,
}

/// A key in the 1A–12B wheel notation, normalized (`08a` → `8A`).
pub fn wheel_key(s: &str) -> Option<String> {
    let s = s.trim();
    let letter = s.chars().last()?.to_ascii_uppercase();
    if letter != 'A' && letter != 'B' {
        return None;
    }
    let digits = s.get(..s.len().saturating_sub(1))?;
    if digits.is_empty() || digits.len() > 2 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (1..=12).contains(&n).then(|| format!("{n}{letter}"))
}

fn bpm_number(s: &str) -> Option<u32> {
    let n: u32 = s.parse().ok()?;
    (20..=400).contains(&n).then_some(n)
}

impl Query {
    pub fn parse(q: &str) -> Query {
        let q: String = q.chars().take(MAX_QUERY_CHARS).collect();
        let words = q.split_whitespace().take(MAX_TERMS).filter_map(Self::word).collect();
        Query { words }
    }

    fn word(raw: &str) -> Option<Word> {
        let (not, w) = match raw.strip_prefix('-') {
            Some(rest) if !rest.is_empty() && !rest.starts_with(|c: char| c.is_ascii_digit()) => (true, rest),
            _ => (false, raw),
        };
        let lower = w.to_lowercase();
        if let Some((field, value)) = lower.split_once(':')
            && let Some(col) = Column::parse(field)
        {
            if value.is_empty() {
                return None;
            }
            let term = match col {
                Column::Bpm => Cmp::parse(value, bpm_number).map(|(cmp, n)| Term::Bpm { cmp, n, auto: None }),
                Column::Key => Some(wheel_key(value).map_or_else(|| Term::In(Column::Key, value.to_string()), |key| Term::Key { key, auto: false })),
                Column::Time => Cmp::parse(value, parse_time).map(|(cmp, n)| Term::Time { cmp, n }),
                Column::Rating => Cmp::parse(value, |s| s.parse::<u32>().ok().filter(|n| *n <= 5)).map(|(cmp, n)| Term::Rating { cmp, n }),
                _ => None,
            }
            .unwrap_or_else(|| Term::In(col, value.to_string()));
            return Some(Word { not, term });
        }
        let auto_bpm = w.contains('-') && w.chars().all(|c| c.is_ascii_digit() || c == '-');
        if auto_bpm && let Some((cmp, n)) = Cmp::parse(w, bpm_number) {
            return Some(Word { not, term: Term::Bpm { cmp, n, auto: Some(lower) } });
        }
        if let Some(key) = wheel_key(w) {
            return Some(Word { not, term: Term::Key { key, auto: true } });
        }
        Some(Word { not, term: Term::Any(lower) })
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Whether `t` matches every word, searching `shown` for plain text.
    pub fn matches(&self, t: &Track, shown: &[Column]) -> bool {
        self.words.iter().all(|w| w.term.matches(t, shown) != w.not)
    }

    /// One short line per word: how it was read.
    pub fn describe(&self) -> Vec<String> {
        self.words
            .iter()
            .map(|w| {
                let s = match &w.term {
                    Term::Any(s) => format!("“{s}” in any column"),
                    Term::In(c, s) => format!("{} “{s}”", c.label()),
                    Term::Bpm { cmp, n, .. } => format!("BPM {}", cmp.describe(*n, |v| v.to_string())),
                    Term::Key { key, .. } => format!("Key {key}"),
                    Term::Time { cmp, n } => format!("Time {}", cmp.describe(*n, format_time)),
                    Term::Rating { cmp, n } => format!("Rating {}", cmp.describe(*n, |v| format!("{v}★"))),
                };
                if w.not { format!("not {s}") } else { s }
            })
            .collect()
    }

    /// The plain words, for highlighting matches.
    pub fn highlight_words(&self) -> Vec<String> {
        self.words
            .iter()
            .filter(|w| !w.not)
            .filter_map(|w| match &w.term {
                Term::Any(s) | Term::In(_, s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }
}

fn contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

fn any_shown(t: &Track, shown: &[Column], needle: &str) -> bool {
    shown.iter().any(|c| contains(&c.text(t), needle))
}

impl Term {
    fn matches(&self, t: &Track, shown: &[Column]) -> bool {
        match self {
            Term::Any(s) => any_shown(t, shown, s),
            Term::In(c, s) => contains(&c.text(t), s),
            Term::Bpm { cmp, n, auto } => {
                let by_bpm = (auto.is_none() || shown.contains(&Column::Bpm)) && t.bpm().is_some_and(|b| cmp.holds(u32::from(b), *n));
                by_bpm || auto.as_ref().is_some_and(|s| any_shown(t, shown, s))
            }
            Term::Key { key, auto } => {
                let by_key = (!auto || shown.contains(&Column::Key)) && t.key().is_some_and(|k| k.eq_ignore_ascii_case(key));
                by_key || (*auto && any_shown(t, shown, &key.to_lowercase()))
            }
            Term::Time { cmp, n } => cmp.holds(t.seconds, *n),
            Term::Rating { cmp, n } => cmp.holds(u32::from(t.rating()), *n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(title: &str, artist: &str, bpm: u16, key: &str, genre: &str, seconds: u32) -> Track {
        Track {
            id: title.into(),
            title: title.into(),
            artist: artist.into(),
            bpm: (bpm > 0).then_some(bpm),
            key: (!key.is_empty()).then(|| key.into()),
            genre: genre.into(),
            seconds,
            path: None,
            rb: None,
        }
    }

    fn shown_all() -> Vec<Column> {
        Column::ALL.to_vec()
    }

    #[test]
    fn plain_words_match_any_shown_column() {
        let q = Query::parse("afro");
        let siren = t("Siren", "Rancido", 120, "7A", "Afro House", 408);
        assert!(q.matches(&siren, &shown_all()));
        assert!(!q.matches(&siren, &Column::DEFAULT), "genre hidden: not searched");
        assert!(Query::parse("genre:afro").matches(&siren, &Column::DEFAULT), "named columns are searched even hidden");
    }

    #[test]
    fn ranges_keys_and_bounds() {
        let siren = t("Siren", "Rancido", 120, "7A", "Afro House", 408);
        let bongo = t("Electric Bongo", "SIDEPIECE", 130, "11A", "Tech House", 326);
        let all = shown_all();
        let q = Query::parse("120-124");
        assert!(q.matches(&siren, &all) && !q.matches(&bongo, &all));
        assert!(Query::parse("124-120").matches(&siren, &all), "a reversed range still works");
        assert!(Query::parse("7a").matches(&siren, &all) && !Query::parse("7A").matches(&bongo, &all));
        assert!(Query::parse("bpm:>125").matches(&bongo, &all) && !Query::parse("bpm:>125").matches(&siren, &all));
        assert!(Query::parse("time:>6:00").matches(&siren, &all) && !Query::parse("time:>6:00").matches(&bongo, &all));
        assert!(Query::parse("house -tech").matches(&siren, &all) && !Query::parse("house -tech").matches(&bongo, &all));
    }

    #[test]
    fn bare_numbers_also_match_text() {
        let song = t("69 (Intro Dirty)", "Nicky Jam & Feid", 94, "6A", "Reggaeton", 210);
        assert!(Query::parse("69").matches(&song, &Column::DEFAULT));
        assert!(Query::parse("feid 6A").matches(&song, &Column::DEFAULT));
    }

    #[test]
    fn hostile_queries_stay_bounded() {
        let long = "a ".repeat(10_000);
        assert!(Query::parse(&long).words.len() <= MAX_TERMS);
        for q in ["bpm:", "key:", ":", "-", "--", "bpm:99999999999999999999", "time:1:2:3:4", "time:>", "bpm:-", "0-0", "999-1000"] {
            let parsed = Query::parse(q);
            let _ = parsed.matches(&t("x", "y", 120, "1A", "z", 1), &shown_all());
            let _ = parsed.describe();
        }
        assert_eq!(parse_time("99999999:99"), None, "overflow is rejected, not wrapped");
    }

    #[test]
    fn describes_what_it_understood() {
        let d = Query::parse("afro 120-124 8a -remix").describe();
        assert_eq!(d, vec!["“afro” in any column", "BPM 120–124", "Key 8A", "not “remix” in any column"]);
    }
}
