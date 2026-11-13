//! The DJ library: the tracks of one Music playlist (the source), and the user's own folders
//! and playlists of them, each in the user's order.
//!
//! The library is app data, not part of a session: it is saved to its own file in the app's
//! settings folder (see `Engine::library_path`) and its commands have no undo. The source is
//! only ever read.

pub mod apple_music;
pub mod rekordbox;
pub mod search;

use search::{Column, Query};
use serde::{Deserialize, Serialize};

/// The Music playlist read when none is named.
pub const DEFAULT_SOURCE: &str = "! DJ Library";
/// Most tracks kept from the source.
pub const MAX_TRACKS: usize = 200_000;
/// Most folders and playlists.
pub const MAX_NODES: usize = 10_000;
/// Most tracks in one playlist.
pub const MAX_PLAYLIST_TRACKS: usize = 100_000;
/// Deepest folder nesting.
pub const MAX_DEPTH: usize = 16;
/// Longest folder, playlist, title, artist or genre text kept, in characters.
pub const MAX_TEXT_CHARS: usize = 256;
/// Largest library file read.
pub const MAX_FILE_BYTES: u64 = 256 << 20;

/// A track of the source playlist.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Track {
    /// The Music app's persistent id, stable across reads.
    pub id: String,
    pub title: String,
    pub artist: String,
    pub bpm: Option<u16>,
    /// In 1A–12B wheel notation.
    pub key: Option<String>,
    pub genre: String,
    pub seconds: u32,
    /// The audio file, when the Music app knows it.
    pub path: Option<String>,
    /// When it was added to the Music app, in seconds since 1970 (UTC).
    pub added: Option<u64>,
    /// What a rekordbox export says about this track; it wins over the Music app's BPM and key.
    pub rb: Option<RbInfo>,
}

/// Seconds since 1970 from an RFC 3339 time (`2026-11-05T20:45:46.000Z`) or a date
/// (`2026-11-05`, read as midnight UTC). Times before 1970 or unreadable text give `None`.
pub fn parse_date(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return u64::try_from(t.timestamp()).ok();
    }
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    u64::try_from(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp()).ok()
}

/// `05.11.26` (day, month, year, UTC).
pub fn format_date(secs: u64) -> String {
    i64::try_from(secs).ok().and_then(|s| chrono::DateTime::from_timestamp(s, 0)).map_or_else(String::new, |t| t.format("%d.%m.%y").to_string())
}

/// A cue point from rekordbox: a hot cue (`hot`: its pad) or a memory cue, maybe a loop.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cue {
    pub name: String,
    /// Seconds from the start of the file.
    pub start: f64,
    pub end: Option<f64>,
    pub hot: Option<u8>,
    pub colour: Option<[u8; 3]>,
}

/// A track's data from the last rekordbox import.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RbInfo {
    pub bpm: Option<u16>,
    pub key: Option<String>,
    /// 0–5 stars.
    pub rating: u8,
    pub colour: Option<[u8; 3]>,
    pub comment: String,
    pub play_count: u32,
    pub cues: Vec<Cue>,
    /// When it was added to rekordbox, in seconds since 1970.
    pub added: Option<u64>,
}

/// Tracks only rekordbox knows have ids starting with this, then their file path.
pub const REKORDBOX_ID: &str = "rb:";

impl Track {
    /// The BPM shown: rekordbox's, else the Music app's.
    pub fn bpm(&self) -> Option<u16> {
        self.rb.as_ref().and_then(|r| r.bpm).or(self.bpm)
    }

    /// The key shown: rekordbox's, else the Music app's.
    pub fn key(&self) -> Option<&str> {
        self.rb.as_ref().and_then(|r| r.key.as_deref()).or(self.key.as_deref())
    }

    pub fn rating(&self) -> u8 {
        self.rb.as_ref().map_or(0, |r| r.rating)
    }

    pub fn comment(&self) -> &str {
        self.rb.as_ref().map_or("", |r| r.comment.as_str())
    }

    /// When it was added: to the Music app, else to rekordbox.
    pub fn added(&self) -> Option<u64> {
        self.added.or_else(|| self.rb.as_ref().and_then(|r| r.added))
    }

    pub fn colour(&self) -> Option<[u8; 3]> {
        self.rb.as_ref().and_then(|r| r.colour)
    }

    /// Whether only a rekordbox export knows this track (it is not in the Music playlist).
    pub fn from_rekordbox_only(&self) -> bool {
        self.id.starts_with(REKORDBOX_ID)
    }
}

/// A key's place on the wheel for sorting: 1A, 1B, 2A, … 12B; no key sorts after all keys.
fn key_order(k: Option<&str>) -> u32 {
    k.and_then(search::wheel_key).map_or(u32::MAX, |k| {
        let (n, l) = k.split_at(k.len().saturating_sub(1));
        n.parse::<u32>().unwrap_or(99) * 2 + u32::from(l == "B")
    })
}

/// Sorts rows by a column, ascending or descending. Tracks with nothing in that column (no BPM,
/// no key, no date) always come last; ties keep the playlist's order.
pub fn sort_tracks(rows: &mut [(usize, &Track)], col: Column, descending: bool) {
    use std::cmp::Ordering;
    let num = |t: &Track| -> Option<u64> {
        match col {
            Column::Bpm => t.bpm().map(u64::from),
            Column::Time => Some(u64::from(t.seconds)),
            Column::Rating => Some(u64::from(t.rating())),
            Column::Added => t.added(),
            Column::Key => Some(u64::from(key_order(t.key()))).filter(|k| *k != u64::from(u32::MAX)),
            _ => None,
        }
    };
    let textual = matches!(col, Column::Title | Column::Artist | Column::Genre | Column::Comment);
    rows.sort_by(|(ia, a), (ib, b)| {
        let ord = if textual {
            let (x, y) = (col.text(a).to_lowercase(), col.text(b).to_lowercase());
            match (x.is_empty(), y.is_empty()) {
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                _ => x.cmp(&y),
            }
        } else {
            match (num(a), num(b)) {
                (Some(x), Some(y)) => x.cmp(&y),
                (None, Some(_)) => return Ordering::Greater,
                (Some(_), None) => return Ordering::Less,
                (None, None) => Ordering::Equal,
            }
        };
        (if descending { ord.reverse() } else { ord }).then(ia.cmp(ib))
    });
}

/// A file path for matching: Unicode NFC (macOS paths may come decomposed) and lower case.
pub fn path_key(p: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    p.nfc().collect::<String>().to_lowercase()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Folder,
    Playlist,
}

/// A folder or playlist. Nodes are kept in display order; a node's children follow their parent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: u64,
    pub name: String,
    pub kind: NodeKind,
    #[serde(default)]
    pub parent: Option<u64>,
    /// Track ids in the user's order (playlists only).
    #[serde(default)]
    pub tracks: Vec<String>,
}

/// The DJ library.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// The Music playlist the tracks come from.
    pub source: String,
    /// Seconds since 1970 of the last successful read of the source.
    pub synced_at: Option<u64>,
    pub tracks: Vec<Track>,
    pub nodes: Vec<Node>,
    next_id: u64,
    /// The top-level folder the last rekordbox import made; the next import replaces it.
    pub rekordbox_root: Option<u64>,
    /// Seconds since 1970 of the last rekordbox import.
    pub rekordbox_at: Option<u64>,
    /// Bumped on every saved change, so views can cache what they list.
    #[serde(skip)]
    pub rev: u64,
}

/// What a rekordbox import did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub folders: usize,
    pub playlists: usize,
    pub entries: usize,
    /// Export tracks found in the Music playlist.
    pub matched: usize,
    /// Export tracks not in the Music playlist, added from their files.
    pub added: usize,
    /// Export tracks with no file, left out.
    pub missing: usize,
    /// Folders and playlists past the library's limits, left out.
    pub skipped: usize,
}

/// What a sync changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    pub tracks: usize,
    pub added: usize,
    pub removed: usize,
}

fn clean_name(name: &str) -> Result<String, String> {
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_TEXT_CHARS).collect();
    if name.is_empty() { Err("the name is empty".into()) } else { Ok(name) }
}

impl Library {
    pub fn source(&self) -> &str {
        if self.source.is_empty() { DEFAULT_SOURCE } else { &self.source }
    }

    pub fn track(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn node(&self, id: u64) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    fn node_mut(&mut self, id: u64) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    /// How deep `id` sits: 0 at the top.
    pub fn depth(&self, id: u64) -> usize {
        let mut depth = 0;
        let mut at = self.node(id).and_then(|n| n.parent);
        while let Some(p) = at {
            depth += 1;
            if depth > MAX_DEPTH {
                break;
            }
            at = self.node(p).and_then(|n| n.parent);
        }
        depth
    }

    /// Whether `id` is `ancestor` or inside it.
    fn within(&self, id: u64, ancestor: u64) -> bool {
        let mut at = Some(id);
        for _ in 0..=MAX_DEPTH {
            match at {
                Some(a) if a == ancestor => return true,
                Some(a) => at = self.node(a).and_then(|n| n.parent),
                None => return false,
            }
        }
        false
    }

    fn folder_param(&self, parent: Option<u64>) -> Result<Option<u64>, String> {
        let Some(p) = parent else { return Ok(None) };
        match self.node(p) {
            Some(n) if n.kind == NodeKind::Folder => {
                if self.depth(p) + 1 >= MAX_DEPTH {
                    return Err("folders are nested too deep".into());
                }
                Ok(Some(p))
            }
            Some(_) => Err(format!("{p} is a playlist, not a folder")),
            None => Err(format!("no folder {p}")),
        }
    }

    /// Where a new last child of `parent` goes: after the parent's last descendant.
    fn insert_at(&self, parent: Option<u64>) -> usize {
        match parent {
            None => self.nodes.len(),
            Some(p) => self.nodes.iter().rposition(|n| self.within(n.id, p)).map_or(self.nodes.len(), |i| i.saturating_add(1)),
        }
    }

    /// Adds a folder or playlist as the last child of `parent` (a folder, or the top).
    pub fn add(&mut self, kind: NodeKind, name: &str, parent: Option<u64>) -> Result<u64, String> {
        if self.nodes.len() >= MAX_NODES {
            return Err(format!("the library holds at most {MAX_NODES} folders and playlists"));
        }
        let name = clean_name(name)?;
        let parent = self.folder_param(parent)?;
        self.next_id = self.next_id.max(self.nodes.iter().map(|n| n.id).max().unwrap_or(0)).saturating_add(1);
        let id = self.next_id;
        let at = self.insert_at(parent);
        self.nodes.insert(at.min(self.nodes.len()), Node { id, name, kind, parent, tracks: Vec::new() });
        Ok(id)
    }

    pub fn rename(&mut self, id: u64, name: &str) -> Result<(), String> {
        let name = clean_name(name)?;
        self.node_mut(id).ok_or_else(|| format!("no folder or playlist {id}"))?.name = name;
        Ok(())
    }

    /// Removes a playlist, or a folder with everything in it. Returns how many were removed.
    pub fn remove(&mut self, id: u64) -> Result<usize, String> {
        if self.node(id).is_none() {
            return Err(format!("no folder or playlist {id}"));
        }
        let gone: Vec<u64> = self.nodes.iter().filter(|n| self.within(n.id, id)).map(|n| n.id).collect();
        self.nodes.retain(|n| !gone.contains(&n.id));
        Ok(gone.len())
    }

    /// Moves a node (with its children) to be the last child of `parent`.
    pub fn move_to(&mut self, id: u64, parent: Option<u64>) -> Result<(), String> {
        if self.node(id).is_none() {
            return Err(format!("no folder or playlist {id}"));
        }
        if let Some(p) = parent
            && self.within(p, id)
        {
            return Err("a folder cannot move into itself".into());
        }
        let parent = self.folder_param(parent)?;
        let moving: Vec<Node> = self.nodes.iter().filter(|n| self.within(n.id, id)).cloned().collect();
        let below = moving.iter().map(|n| self.depth(n.id).saturating_sub(self.depth(id))).max().unwrap_or(0);
        if parent.map_or(0, |p| self.depth(p) + 1) + below >= MAX_DEPTH {
            return Err("folders are nested too deep".into());
        }
        self.nodes.retain(|n| !moving.iter().any(|m| m.id == n.id));
        let at = self.insert_at(parent).min(self.nodes.len());
        for (i, mut n) in moving.into_iter().enumerate() {
            if n.id == id {
                n.parent = parent;
            }
            self.nodes.insert(at.saturating_add(i).min(self.nodes.len()), n);
        }
        Ok(())
    }

    fn playlist_mut(&mut self, id: u64) -> Result<&mut Node, String> {
        match self.node_mut(id) {
            Some(n) if n.kind == NodeKind::Playlist => Ok(n),
            Some(_) => Err(format!("{id} is a folder, not a playlist")),
            None => Err(format!("no playlist {id}")),
        }
    }

    /// Adds tracks to a playlist at `at` (default: the end). Unknown ids are skipped; a track
    /// may appear more than once. Returns how many were added.
    pub fn add_tracks(&mut self, playlist: u64, ids: &[String], at: Option<usize>) -> Result<usize, String> {
        let known: Vec<String> = ids.iter().filter(|id| self.track(id).is_some()).cloned().collect();
        let list = self.playlist_mut(playlist)?;
        if list.tracks.len().saturating_add(known.len()) > MAX_PLAYLIST_TRACKS {
            return Err(format!("a playlist holds at most {MAX_PLAYLIST_TRACKS} tracks"));
        }
        let at = at.unwrap_or(list.tracks.len()).min(list.tracks.len());
        let n = known.len();
        list.tracks.splice(at..at, known);
        Ok(n)
    }

    /// Removes the entries at `positions` (0-based) from a playlist. Returns how many were removed.
    pub fn remove_entries(&mut self, playlist: u64, positions: &[usize]) -> Result<usize, String> {
        let list = self.playlist_mut(playlist)?;
        let before = list.tracks.len();
        let mut i = 0;
        list.tracks.retain(|_| {
            let keep = !positions.contains(&i);
            i += 1;
            keep
        });
        Ok(before - list.tracks.len())
    }

    /// Moves the entry at `from` to `to` in a playlist.
    pub fn reorder(&mut self, playlist: u64, from: usize, to: usize) -> Result<(), String> {
        let list = self.playlist_mut(playlist)?;
        if from >= list.tracks.len() {
            return Err(format!("the playlist has no entry {from}"));
        }
        let t = list.tracks.remove(from);
        let to = to.min(list.tracks.len());
        list.tracks.insert(to, t);
        Ok(())
    }

    /// The tracks of a playlist in its order, or every track for `None`. Entries whose track
    /// left the source are skipped. Each comes with its position in the playlist.
    pub fn listing(&self, playlist: Option<u64>) -> Result<Vec<(usize, &Track)>, String> {
        match playlist {
            None => Ok(self.tracks.iter().enumerate().collect()),
            Some(id) => {
                let n = self.node(id).ok_or_else(|| format!("no playlist {id}"))?;
                if n.kind == NodeKind::Folder {
                    // A folder lists the tracks of every playlist in it, each once.
                    let mut seen = std::collections::HashSet::new();
                    let ids = self.nodes.iter().filter(|m| m.kind == NodeKind::Playlist && self.within(m.id, id)).flat_map(|m| &m.tracks);
                    return Ok(ids.filter(|t| seen.insert(t.as_str())).filter_map(|t| self.track(t)).enumerate().collect());
                }
                let index: std::collections::HashMap<&str, &Track> = self.tracks.iter().map(|t| (t.id.as_str(), t)).collect();
                Ok(n.tracks.iter().enumerate().filter_map(|(i, t)| index.get(t.as_str()).map(|t| (i, *t))).collect())
            }
        }
    }

    /// The tracks of `playlist` (or all) that match `query` over the `shown` columns.
    pub fn search(&self, playlist: Option<u64>, query: &Query, shown: &[Column]) -> Result<Vec<(usize, &Track)>, String> {
        Ok(self.listing(playlist)?.into_iter().filter(|(_, t)| query.matches(t, shown)).collect())
    }

    /// [`Self::search`], keeping only tracks added since `since` (seconds since 1970) when
    /// given, then sorted by `sort` (ascending, or descending), else in the playlist's order.
    pub fn find(
        &self,
        playlist: Option<u64>,
        query: &Query,
        shown: &[Column],
        since: Option<u64>,
        sort: Option<(Column, bool)>,
    ) -> Result<Vec<(usize, &Track)>, String> {
        let mut hits = self.search(playlist, query, shown)?;
        if let Some(since) = since {
            hits.retain(|(_, t)| t.added().is_some_and(|a| a >= since));
        }
        if let Some((col, descending)) = sort {
            sort_tracks(&mut hits, col, descending);
        }
        Ok(hits)
    }

    /// The playlists each track is in, by track id, for whole-library results.
    pub fn memberships(&self) -> std::collections::HashMap<&str, Vec<&str>> {
        let mut m: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
        for n in self.nodes.iter().filter(|n| n.kind == NodeKind::Playlist) {
            for t in &n.tracks {
                let lists = m.entry(t.as_str()).or_default();
                if !lists.contains(&n.name.as_str()) {
                    lists.push(&n.name);
                }
            }
        }
        m
    }

    /// Replaces the tracks with a fresh read of the source. Playlists keep their entries; an
    /// entry whose track left the source stays in the playlist and shows again if it returns.
    pub fn replace_tracks(&mut self, source: &str, mut tracks: Vec<Track>, now: u64) -> SyncReport {
        use std::collections::{HashMap, HashSet};
        tracks.truncate(MAX_TRACKS);
        let report = {
            let old: HashSet<&str> = self.tracks.iter().filter(|t| !t.from_rekordbox_only()).map(|t| t.id.as_str()).collect();
            let new: HashSet<&str> = tracks.iter().map(|t| t.id.as_str()).collect();
            SyncReport { tracks: tracks.len(), added: new.difference(&old).count(), removed: old.difference(&new).count() }
        };
        // Keep what rekordbox said about each track.
        let old_rb: HashMap<String, RbInfo> = self.tracks.iter().filter_map(|t| t.rb.clone().map(|r| (t.id.clone(), r))).collect();
        let by_path: HashMap<String, usize> = tracks.iter().enumerate().filter_map(|(i, t)| t.path.as_deref().map(|p| (path_key(p), i))).collect();
        for t in &mut tracks {
            if t.rb.is_none() {
                t.rb = old_rb.get(&t.id).cloned();
            }
        }
        // A rekordbox-only track now in the Music playlist becomes that track in every playlist.
        let mut renamed: HashMap<String, String> = HashMap::new();
        let mut keep = Vec::new();
        for old in std::mem::take(&mut self.tracks).into_iter().filter(Track::from_rekordbox_only) {
            match old.path.as_deref().and_then(|p| by_path.get(&path_key(p))).and_then(|i| tracks.get_mut(*i)) {
                Some(t) => {
                    if t.rb.is_none() {
                        t.rb = old.rb.clone();
                    }
                    renamed.insert(old.id.clone(), t.id.clone());
                }
                None => keep.push(old),
            }
        }
        if !renamed.is_empty() {
            for n in &mut self.nodes {
                for id in &mut n.tracks {
                    if let Some(new) = renamed.get(id) {
                        id.clone_from(new);
                    }
                }
            }
        }
        tracks.extend(keep.into_iter().take(MAX_TRACKS.saturating_sub(tracks.len())));
        self.tracks = tracks;
        self.source = source.chars().take(MAX_TEXT_CHARS).collect();
        self.synced_at = Some(now);
        report
    }

    /// Brings in a rekordbox export: each export track is matched to a library track by its file
    /// (else by title, artist and length) and takes rekordbox's key, BPM, rating, colour, comment,
    /// play count and cues; tracks only rekordbox has are added from their files. The export's
    /// folders and playlists, in its order, replace the previous import's under a top-level
    /// "rekordbox" folder. Playlists moved out of that folder are the user's and stay.
    pub fn import_rekordbox(&mut self, x: rekordbox::Export, now: u64) -> ImportReport {
        use std::collections::{HashMap, HashSet};
        let mut report = ImportReport::default();
        for t in &mut self.tracks {
            t.rb = None;
        }
        let mut by_path: HashMap<String, usize> = HashMap::new();
        let mut by_meta: HashMap<(String, String), usize> = HashMap::new();
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(p) = &t.path {
                by_path.entry(path_key(p)).or_insert(i);
            }
            by_meta.entry((t.title.to_lowercase(), t.artist.to_lowercase())).or_insert(i);
        }
        let mut ids: HashMap<String, String> = HashMap::new();
        let mut paths: HashMap<String, String> = HashMap::new();
        for rt in x.tracks {
            let info = RbInfo {
                bpm: rt.bpm,
                key: rt.key.clone(),
                rating: rt.rating,
                colour: rt.colour,
                comment: rt.comment.clone(),
                play_count: rt.play_count,
                cues: rt.cues.clone(),
                added: rt.added,
            };
            let pkey = rt.path.as_deref().map(path_key);
            let found = pkey.as_ref().and_then(|p| by_path.get(p).copied()).or_else(|| {
                by_meta
                    .get(&(rt.title.to_lowercase(), rt.artist.to_lowercase()))
                    .copied()
                    .filter(|i| self.tracks.get(*i).is_some_and(|t| t.seconds.abs_diff(rt.seconds) <= 2))
            });
            let id = match (found, &rt.path) {
                (Some(i), _) => match self.tracks.get_mut(i) {
                    Some(t) => {
                        t.rb = Some(info);
                        report.matched += 1;
                        t.id.clone()
                    }
                    None => continue,
                },
                (None, Some(path)) if self.tracks.len() < MAX_TRACKS => {
                    let id = format!("{REKORDBOX_ID}{path}");
                    if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                        t.rb = Some(info);
                    } else {
                        self.tracks.push(Track {
                            id: id.clone(),
                            title: rt.title.clone(),
                            artist: rt.artist.clone(),
                            bpm: None,
                            key: None,
                            genre: rt.genre.clone(),
                            seconds: rt.seconds,
                            path: Some(path.clone()),
                            added: None,
                            rb: Some(info),
                        });
                        by_path.insert(path_key(path), self.tracks.len() - 1);
                    }
                    report.added += 1;
                    id
                }
                _ => {
                    report.missing += 1;
                    continue;
                }
            };
            if let Some(p) = pkey {
                paths.insert(p, id.clone());
            }
            ids.insert(rt.id, id);
        }
        // The tree: replace the previous import's folder.
        if let Some(old) = self.rekordbox_root.take()
            && self.node(old).is_some_and(|n| n.parent.is_none())
        {
            let _ = self.remove(old);
        }
        match self.add(NodeKind::Folder, "rekordbox", None) {
            Ok(root) => {
                self.rekordbox_root = Some(root);
                self.add_rb_nodes(&x.nodes, root, &ids, &paths, &mut report);
            }
            Err(_) => report.skipped += 1,
        }
        // Rekordbox-only tracks this export no longer lists, and no playlist holds, go.
        let listed: HashSet<&str> = ids.values().map(String::as_str).collect();
        let held: HashSet<String> = self.nodes.iter().flat_map(|n| n.tracks.iter().cloned()).collect();
        self.tracks.retain(|t| !t.from_rekordbox_only() || listed.contains(t.id.as_str()) || held.contains(&t.id));
        self.rekordbox_at = Some(now);
        report
    }

    fn add_rb_nodes(
        &mut self,
        nodes: &[rekordbox::RbNode],
        parent: u64,
        ids: &std::collections::HashMap<String, String>,
        paths: &std::collections::HashMap<String, String>,
        report: &mut ImportReport,
    ) {
        for n in nodes {
            match n {
                rekordbox::RbNode::Folder { name, children } => match self.add(NodeKind::Folder, name, Some(parent)) {
                    Ok(id) => {
                        report.folders += 1;
                        self.add_rb_nodes(children, id, ids, paths, report);
                    }
                    Err(_) => report.skipped += 1,
                },
                rekordbox::RbNode::Playlist { name, entries, by_location } => match self.add(NodeKind::Playlist, name, Some(parent)) {
                    Ok(id) => {
                        report.playlists += 1;
                        let tracks: Vec<String> =
                            entries.iter().filter_map(|e| if *by_location { paths.get(&path_key(e)) } else { ids.get(e) }).cloned().collect();
                        report.entries += self.add_tracks(id, &tracks, None).unwrap_or(0);
                    }
                    Err(_) => report.skipped += 1,
                },
            }
        }
    }

    /// A saved library from its file's bytes ([`crate::library_store`] reads the file). Damaged
    /// data is an error, never a crash; the tree is repaired so it is always well formed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Library, String> {
        let mut lib: Library = serde_json::from_slice(bytes).map_err(|e| format!("the library file is damaged: {e}"))?;
        lib.tracks.truncate(MAX_TRACKS);
        lib.nodes.truncate(MAX_NODES);
        // Drop nodes whose parent is missing or not a folder, so the tree is always well formed.
        let ids: Vec<(u64, NodeKind)> = lib.nodes.iter().map(|n| (n.id, n.kind)).collect();
        for n in &mut lib.nodes {
            if n.parent.is_some_and(|p| !ids.iter().any(|(id, k)| *id == p && *k == NodeKind::Folder) || p == n.id) {
                n.parent = None;
            }
            n.tracks.truncate(MAX_PLAYLIST_TRACKS);
        }
        Ok(lib)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track { id: id.into(), title: format!("Song {id}"), ..Track::default() }
    }

    fn lib() -> Library {
        let mut l = Library::default();
        l.replace_tracks("DJ", vec![track("a"), track("b"), track("c")], 1);
        l
    }

    #[test]
    fn folders_keep_their_children_together() {
        let mut l = lib();
        let latin = l.add(NodeKind::Folder, "LATIN", None).unwrap_or(0);
        let cana = l.add(NodeKind::Folder, "la caña", None).unwrap_or(0);
        let salsa = l.add(NodeKind::Playlist, "salsa", Some(latin)).unwrap_or(0);
        let names: Vec<&str> = l.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["LATIN", "salsa", "la caña"]);
        assert_eq!(l.depth(salsa), 1);
        assert!(l.add(NodeKind::Playlist, "x", Some(salsa)).is_err(), "a playlist cannot hold things");
        assert!(l.move_to(latin, Some(salsa)).is_err());
        l.move_to(latin, Some(cana)).unwrap_or_default();
        let names: Vec<&str> = l.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["la caña", "LATIN", "salsa"]);
        assert!(l.move_to(cana, Some(latin)).is_err(), "a folder cannot move into itself");
        assert_eq!(l.remove(cana), Ok(3));
        assert!(l.nodes.is_empty());
    }

    #[test]
    fn playlists_keep_the_users_order_across_syncs() {
        let mut l = lib();
        let p = l.add(NodeKind::Playlist, "warm up 3.10", None).unwrap_or(0);
        assert_eq!(l.add_tracks(p, &["c".into(), "zz".into(), "a".into()], None), Ok(2), "unknown ids are skipped");
        l.reorder(p, 1, 0).unwrap_or_default();
        let order = |l: &Library| l.listing(Some(p)).unwrap_or_default().iter().map(|(_, t)| t.id.clone()).collect::<Vec<_>>();
        assert_eq!(order(&l), ["a", "c"]);
        let r = l.replace_tracks("DJ", vec![track("a"), track("d")], 2);
        assert_eq!((r.tracks, r.added, r.removed), (2, 1, 2));
        assert_eq!(order(&l), ["a"], "a track gone from the source is not listed");
        l.replace_tracks("DJ", vec![track("a"), track("c")], 3);
        assert_eq!(order(&l), ["a", "c"], "and comes back when it returns");
    }

    #[test]
    fn hostile_names_and_indexes_are_errors() {
        let mut l = lib();
        assert!(l.add(NodeKind::Folder, "   \n", None).is_err());
        let p = l.add(NodeKind::Playlist, &"x".repeat(10_000), None).unwrap_or(0);
        assert_eq!(l.node(p).map(|n| n.name.chars().count()), Some(MAX_TEXT_CHARS));
        assert!(l.reorder(p, 99, 0).is_err());
        assert!(l.add_tracks(9999, &["a".into()], None).is_err());
        assert!(l.add_tracks(p, &["a".into()], Some(usize::MAX)).is_ok());
        assert_eq!(l.remove_entries(p, &[usize::MAX, 0]), Ok(1));
    }

    #[test]
    fn deep_nesting_is_refused() {
        let mut l = lib();
        let mut parent = None;
        let mut refused = false;
        for i in 0..MAX_DEPTH + 4 {
            match l.add(NodeKind::Folder, &format!("f{i}"), parent) {
                Ok(id) => parent = Some(id),
                Err(_) => refused = true,
            }
        }
        assert!(refused);
    }

    #[test]
    fn damaged_data_loads_safely() {
        let mut l = lib();
        let f = l.add(NodeKind::Folder, "LATIN", None).unwrap_or(0);
        l.add(NodeKind::Playlist, "salsa", Some(f)).unwrap_or(0);
        let bytes = serde_json::to_vec(&l).unwrap_or_default();
        assert_eq!(Library::from_bytes(&bytes), Ok(l));
        let bad = br#"{"nodes":[{"id":1,"name":"x","kind":"playlist","parent":1},{"id":2,"name":"y","kind":"folder","parent":77}]}"#;
        assert!(Library::from_bytes(bad).unwrap_or_default().nodes.iter().all(|n| n.parent.is_none()));
        assert!(Library::from_bytes(b"\xff\x00garbage").is_err());
    }
}
