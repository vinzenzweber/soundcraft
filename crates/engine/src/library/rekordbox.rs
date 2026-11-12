//! Reading a rekordbox XML export ("File › Export Collection in xml format"), the documented
//! interchange format DJ software uses to share a library: a `COLLECTION` of `TRACK`s with their
//! analysis (key, BPM, cues), and a `PLAYLISTS` tree of folder and playlist `NODE`s whose entries
//! name tracks by `TrackID` (or by file location). Only read; never written.

use super::search::wheel_key;
use super::{Cue, MAX_DEPTH, MAX_NODES, MAX_PLAYLIST_TRACKS, MAX_TEXT_CHARS, MAX_TRACKS};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// Most cue points kept per track.
pub const MAX_CUES: usize = 128;

/// A track of the export's collection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RbTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub genre: String,
    pub comment: String,
    pub bpm: Option<u16>,
    /// In 1A–12B wheel notation, whatever notation the export used.
    pub key: Option<String>,
    /// 0–5 stars.
    pub rating: u8,
    pub colour: Option<[u8; 3]>,
    pub play_count: u32,
    pub seconds: u32,
    /// The audio file, decoded from the export's `file://` location.
    pub path: Option<String>,
    pub cues: Vec<Cue>,
}

/// A folder or playlist of the export.
#[derive(Clone, Debug, PartialEq)]
pub enum RbNode {
    Folder {
        name: String,
        children: Vec<RbNode>,
    },
    /// `entries` are `TrackID`s, or file paths when `by_location`.
    Playlist {
        name: String,
        entries: Vec<String>,
        by_location: bool,
    },
}

/// A parsed export.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Export {
    pub tracks: Vec<RbTrack>,
    /// The children of the export's root folder.
    pub nodes: Vec<RbNode>,
}

impl Export {
    /// How many folders and playlists the tree holds.
    pub fn counts(&self) -> (usize, usize) {
        fn walk(nodes: &[RbNode], c: &mut (usize, usize)) {
            for n in nodes {
                match n {
                    RbNode::Folder { children, .. } => {
                        c.0 += 1;
                        walk(children, c);
                    }
                    RbNode::Playlist { .. } => c.1 += 1,
                }
            }
        }
        let mut c = (0, 0);
        walk(&self.nodes, &mut c);
        c
    }
}

/// A musical key (`Fm`, `F#m`, `Dbmaj`, `A min`) or a wheel key (`4A`) in wheel notation.
pub fn to_wheel(key: &str) -> Option<String> {
    if let Some(k) = wheel_key(key) {
        return Some(k);
    }
    let k: String = key.trim().chars().filter(|c| !c.is_whitespace()).collect::<String>().replace('♯', "#").replace('♭', "b");
    let lower = k.to_ascii_lowercase();
    let (root, minor) = if let Some(r) = lower.strip_suffix("min") {
        (r.to_string(), true)
    } else if let Some(r) = lower.strip_suffix("maj") {
        (r.to_string(), false)
    } else if let Some(r) = lower.strip_suffix('m') {
        (r.to_string(), true)
    } else {
        (lower, false)
    };
    // Semitones above C, sharps and flats both.
    let pc = match root.as_str() {
        "c" | "b#" => 0,
        "c#" | "db" => 1,
        "d" => 2,
        "d#" | "eb" => 3,
        "e" | "fb" => 4,
        "f" | "e#" => 5,
        "f#" | "gb" => 6,
        "g" => 7,
        "g#" | "ab" => 8,
        "a" => 9,
        "a#" | "bb" => 10,
        "b" | "cb" => 11,
        _ => return None,
    };
    // The wheel numbers fifths: C major is 8B and A minor 8A, each fifth up adds one.
    let major_pc = if minor { (pc + 3) % 12 } else { pc };
    let n = (major_pc * 7 % 12 + 8 - 1) % 12 + 1;
    Some(format!("{n}{}", if minor { 'A' } else { 'B' }))
}

/// The file path of a `file://localhost/...` location, percent-decoded and in NFC form.
pub fn location_path(loc: &str) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;
    let rest = loc.strip_prefix("file://localhost").or_else(|| loc.strip_prefix("file://"))?;
    let decoded = percent_encoding::percent_decode_str(rest).decode_utf8().ok()?;
    let path: String = decoded.nfc().collect();
    // A Windows export has "/C:/…".
    let path = if path.len() > 2 && path.as_bytes().get(2) == Some(&b':') { path.get(1..).unwrap_or(&path).to_string() } else { path };
    (!path.is_empty() && path.len() <= 4096).then_some(path)
}

fn text(s: &str) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(MAX_TEXT_CHARS).collect()
}

fn attrs(e: &BytesStart) -> Vec<(String, String)> {
    e.attributes()
        .filter_map(Result::ok)
        .take(64)
        .filter_map(|a| a.normalized_value(XmlVersion::Implicit1_0).ok().map(|v| (a.key.as_ref().to_string(), v.into_owned())))
        .collect()
}

fn get<'a>(a: &'a [(String, String)], key: &str) -> &'a str {
    a.iter().find(|(k, _)| k == key).map_or("", |(_, v)| v.as_str())
}

fn num(a: &[(String, String)], key: &str) -> Option<f64> {
    get(a, key).trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

fn colour(s: &str) -> Option<[u8; 3]> {
    let hex = s.trim().strip_prefix("0x").or_else(|| s.trim().strip_prefix('#'))?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    (v != 0).then_some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn track(a: &[(String, String)]) -> RbTrack {
    let tonality = get(a, "Tonality");
    let rating = num(a, "Rating").map_or(0, |r| ((r.clamp(0.0, 255.0) + 25.0) / 51.0).floor() as u8).min(5);
    RbTrack {
        id: get(a, "TrackID").chars().take(64).collect(),
        title: text(get(a, "Name")),
        artist: text(get(a, "Artist")),
        genre: text(get(a, "Genre")),
        comment: text(get(a, "Comments")),
        bpm: num(a, "AverageBpm").filter(|b| *b >= 1.0 && *b <= 999.0).map(|b| b.round() as u16),
        key: to_wheel(tonality),
        rating,
        colour: colour(get(a, "Colour")),
        play_count: num(a, "PlayCount").map_or(0, |p| p.clamp(0.0, f64::from(u32::MAX)) as u32),
        seconds: num(a, "TotalTime").map_or(0, |s| s.clamp(0.0, f64::from(u32::MAX)) as u32),
        path: location_path(get(a, "Location")),
        cues: Vec::new(),
    }
}

fn cue(a: &[(String, String)]) -> Option<Cue> {
    let start = num(a, "Start").filter(|s| *s >= 0.0 && *s < 86_400.0)?;
    let end = num(a, "End").filter(|e| *e > start && *e < 86_400.0);
    let hot = num(a, "Num").filter(|n| (0.0..=15.0).contains(n)).map(|n| n as u8);
    let rgb = |k: &str| num(a, k).map(|v| v.clamp(0.0, 255.0) as u8);
    let colour = match (rgb("Red"), rgb("Green"), rgb("Blue")) {
        (Some(r), Some(g), Some(b)) => Some([r, g, b]),
        _ => None,
    };
    Some(Cue { name: text(get(a, "Name")), start, end, hot, colour })
}

/// A folder or playlist being read, with the children read so far.
struct Open {
    name: String,
    folder: bool,
    by_location: bool,
    children: Vec<RbNode>,
    entries: Vec<String>,
}

impl Open {
    fn close(self) -> RbNode {
        if self.folder {
            RbNode::Folder { name: self.name, children: self.children }
        } else {
            RbNode::Playlist { name: self.name, entries: self.entries, by_location: self.by_location }
        }
    }
}

/// Reads an export. A file that is not one, or is damaged, is an error, never a crash; sizes
/// beyond the library's limits are cut there.
pub fn parse(bytes: &[u8]) -> Result<Export, String> {
    let mut r = Reader::from_reader(bytes);
    r.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = Export::default();
    let mut seen_root = false;
    let mut in_collection = false;
    let mut in_playlists = false;
    let mut in_track = false;
    // The open NODEs; the first is the export's ROOT folder.
    let mut stack: Vec<Open> = Vec::new();
    let mut nodes = 0usize;
    loop {
        let ev = r.read_event_into(&mut buf).map_err(|e| format!("this is not a readable rekordbox XML file ({e})"))?;
        let (start, empty) = match &ev {
            Event::Start(e) => (Some(e.clone().into_owned()), false),
            Event::Empty(e) => (Some(e.clone().into_owned()), true),
            Event::End(e) => {
                match e.local_name().as_ref() {
                    "COLLECTION" => in_collection = false,
                    "PLAYLISTS" => in_playlists = false,
                    "TRACK" => in_track = false,
                    "NODE" => {
                        if let Some(done) = stack.pop() {
                            let node = done.close();
                            match stack.last_mut() {
                                Some(parent) => parent.children.push(node),
                                None => {
                                    if let RbNode::Folder { children, .. } = node {
                                        out.nodes = children;
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                (None, false)
            }
            Event::Eof => break,
            _ => (None, false),
        };
        let Some(e) = start else {
            buf.clear();
            continue;
        };
        match e.local_name().as_ref() {
            "DJ_PLAYLISTS" => seen_root = true,
            "COLLECTION" => in_collection = !empty,
            "PLAYLISTS" => in_playlists = !empty,
            "TRACK" if in_collection && stack.is_empty() => {
                if out.tracks.len() < MAX_TRACKS {
                    out.tracks.push(track(&attrs(&e)));
                }
                in_track = !empty;
            }
            "POSITION_MARK" if in_track => {
                if let Some(t) = out.tracks.last_mut()
                    && t.cues.len() < MAX_CUES
                    && let Some(c) = cue(&attrs(&e))
                {
                    t.cues.push(c);
                }
            }
            "NODE" if in_playlists => {
                let a = attrs(&e);
                let folder = get(&a, "Type").trim() == "0";
                nodes += 1;
                let open = Open {
                    name: text(get(&a, "Name")),
                    folder,
                    by_location: get(&a, "KeyType").trim() == "1",
                    children: Vec::new(),
                    entries: Vec::new(),
                };
                if stack.len() > MAX_DEPTH || nodes > MAX_NODES + 1 {
                    // Too deep or too many: skip this node (its entries land nowhere).
                    if !empty {
                        stack.push(Open { name: String::new(), folder: true, by_location: false, children: Vec::new(), entries: Vec::new() });
                    }
                } else if empty {
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(open.close());
                    }
                } else {
                    stack.push(open);
                }
            }
            "TRACK" if in_playlists => {
                if let Some(list) = stack.last_mut()
                    && !list.folder
                    && list.entries.len() < MAX_PLAYLIST_TRACKS
                {
                    let a = attrs(&e);
                    let key = get(&a, "Key");
                    let entry = if list.by_location { location_path(key) } else { Some(key.chars().take(64).collect()) };
                    if let Some(entry) = entry.filter(|k| !k.is_empty()) {
                        list.entries.push(entry);
                    }
                }
            }
            _ => {}
        }
        buf.clear();
    }
    if !seen_root {
        return Err("this is not a rekordbox XML export (File › Export Collection in xml format)".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DJ_PLAYLISTS Version="1.0.0">
  <PRODUCT Name="rekordbox" Version="7.2.19" Company="AlphaTheta"/>
  <COLLECTION Entries="2">
    <TRACK TrackID="11" Name="Siren (Original Mix)" Artist="Rancido" Genre="Afro House" Kind="MP3 File" TotalTime="408" AverageBpm="120.00" Comments="warm up" PlayCount="7" Rating="204" Location="file://localhost/Users/me/Music/Siren%20(Original%20Mix).mp3" Tonality="Dm" Colour="0xFF007F">
      <TEMPO Inizio="0.120" Bpm="120.00" Metro="4/4" Battito="1"/>
      <POSITION_MARK Name="drop" Type="0" Start="64.120" Num="0" Red="40" Green="226" Blue="20"/>
      <POSITION_MARK Name="" Type="0" Start="0.120" Num="-1"/>
    </TRACK>
    <TRACK TrackID="12" Name="La Romana &amp; more" Artist="Bad Bunny" AverageBpm="111.98" Tonality="1A" Location="file://localhost/Users/me/Music/Ca%C3%B1a.mp3"/>
  </COLLECTION>
  <PLAYLISTS>
    <NODE Type="0" Name="ROOT" Count="2">
      <NODE Type="0" Name="LATIN" Count="1">
        <NODE Name="salsa" Type="1" KeyType="0" Entries="2">
          <TRACK Key="12"/>
          <TRACK Key="11"/>
        </NODE>
      </NODE>
      <NODE Name="by path" Type="1" KeyType="1" Entries="1">
        <TRACK Key="file://localhost/Users/me/Music/Ca%C3%B1a.mp3"/>
      </NODE>
      <NODE Name="empty" Type="1" KeyType="0" Entries="0"/>
    </NODE>
  </PLAYLISTS>
</DJ_PLAYLISTS>"#;

    #[test]
    fn reads_tracks_cues_and_the_tree() {
        let x = parse(SAMPLE.as_bytes()).unwrap_or_default();
        assert_eq!(x.tracks.len(), 2);
        let siren = &x.tracks[0];
        assert_eq!((siren.bpm, siren.key.as_deref(), siren.rating, siren.play_count, siren.seconds), (Some(120), Some("7A"), 4, 7, 408));
        assert_eq!(siren.path.as_deref(), Some("/Users/me/Music/Siren (Original Mix).mp3"));
        assert_eq!(siren.colour, Some([0xFF, 0x00, 0x7F]));
        assert_eq!(siren.cues.len(), 2);
        assert_eq!((siren.cues[0].hot, siren.cues[1].hot), (Some(0), None));
        assert_eq!(x.tracks[1].title, "La Romana & more");
        assert_eq!(x.tracks[1].bpm, Some(112));
        assert_eq!(x.tracks[1].path.as_deref(), Some("/Users/me/Music/Caña.mp3"));
        assert_eq!(x.counts(), (1, 3));
        let RbNode::Folder { name, children } = &x.nodes[0] else { panic!("folder first") };
        assert_eq!(name, "LATIN");
        assert_eq!(children[0], RbNode::Playlist { name: "salsa".into(), entries: vec!["12".into(), "11".into()], by_location: false });
        assert_eq!(x.nodes[1], RbNode::Playlist { name: "by path".into(), entries: vec!["/Users/me/Music/Caña.mp3".into()], by_location: true });
    }

    #[test]
    fn keys_in_any_notation() {
        for (k, w) in [
            ("Am", "8A"),
            ("C", "8B"),
            ("Fm", "4A"),
            ("F#m", "11A"),
            ("Gbm", "11A"),
            ("Dbm", "12A"),
            ("C#m", "12A"),
            ("Abm", "1A"),
            ("B", "1B"),
            ("E", "12B"),
            ("Bb", "6B"),
            ("A min", "8A"),
            ("Dmaj", "10B"),
            ("08a", "8A"),
            ("E♭m", "2A"),
        ] {
            assert_eq!(to_wheel(k).as_deref(), Some(w), "{k}");
        }
        for k in ["", "H", "13A", "xm", "Ammm"] {
            assert_eq!(to_wheel(k), None, "{k}");
        }
    }

    #[test]
    fn junk_is_an_error_not_a_crash() {
        assert!(parse(b"").is_err());
        assert!(parse(b"<html><body/></html>").is_err());
        assert!(parse(b"<DJ_PLAYLISTS><COLLECTION><TRACK").is_err() || parse(b"<DJ_PLAYLISTS><COLLECTION><TRACK").is_ok());
        let deep =
            format!("<DJ_PLAYLISTS><PLAYLISTS>{}{}</PLAYLISTS></DJ_PLAYLISTS>", "<NODE Type=\"0\" Name=\"f\">".repeat(200), "</NODE>".repeat(200));
        assert!(parse(deep.as_bytes()).is_ok(), "deep trees are cut, not a stack overflow");
        let odd = r#"<DJ_PLAYLISTS><COLLECTION><TRACK TrackID="1" AverageBpm="NaN" TotalTime="-4" Rating="99999" PlayCount="1e300" Location="file://localhost/%FF%FE"><POSITION_MARK Start="inf"/></TRACK></COLLECTION></DJ_PLAYLISTS>"#;
        let x = parse(odd.as_bytes()).unwrap_or_default();
        assert_eq!(
            (x.tracks[0].bpm, x.tracks[0].seconds, x.tracks[0].rating, x.tracks[0].path.clone(), x.tracks[0].cues.len()),
            (None, 0, 5, None, 0)
        );
    }
}
