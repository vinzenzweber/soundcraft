//! DJ library commands: read the source Music playlist, search, and keep folders and playlists.
//!
//! The library is app data saved beside the settings, so these commands change no session and
//! have no undo. Everything the library panel does goes through them, so an assistant connected
//! over MCP or the control channel can build playlists the same way.

use super::{CommandSpec, bad, str_param};
use crate::library::search::{Column, Query, format_time};
use crate::library::{self, NodeKind, Track};
use crate::{Engine, EngineError, Result, cmd};
use serde_json::{Value, json};

/// Most results one search returns.
const MAX_LIMIT: u64 = 10_000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "library.tree",
            "DJ Library Playlists",
            [],
            None,
            "{} — the source Music playlist, when it was last read, the track count and every folder and playlist in order: id, name, kind (folder | playlist), parent, depth, count",
            super::always,
            tree
        ),
        cmd!(
            query "library.search",
            "Search DJ Library",
            [],
            None,
            "{query?: text, playlist?: id (a folder searches its playlists; omit for the whole library), columns?: [title, artist, bpm, key, genre, time, rating, comment, added] (the shown columns plain words search; default title, artist, bpm, key), sort?: a column (title, artist, bpm, key, genre, time, rating, comment, added), descending?: false, added_within_days?: n (only tracks added in the last n days), offset?: 0, limit?: 200 (max 10000)} — words match any shown column; 120-124 is a BPM range, 8A a key, column:value one column (bpm:>125, time:<4:00, genre:afro, rating:>3), -word leaves out. Returns total, matched, how each word was read, and the tracks: id, position in the playlist, title, artist, bpm, key, genre, time, seconds, path, playlists",
            super::always,
            search
        ),
        cmd!(
            noundo "library.sync",
            "Sync DJ Library",
            [],
            None,
            "{playlist?: Music playlist name (default: the current source, else \"! DJ Library\")} — reads that playlist from the Music app (macOS, read-only) as the library's tracks; your playlists keep their entries",
            super::always,
            sync
        ),
        cmd!(
            noundo "library.folder.new",
            "New DJ Library Folder",
            [],
            None,
            "{name, parent?: folder id} — adds a folder as the last child of parent (or at the top); returns its id",
            super::always,
            |e, p| add(e, p, NodeKind::Folder, "library.folder.new")
        ),
        cmd!(
            noundo "library.playlist.new",
            "New DJ Library Playlist",
            [],
            None,
            "{name, parent?: folder id, tracks?: [track ids]} — adds a playlist (optionally filled, in that order); returns its id",
            super::always,
            new_playlist
        ),
        cmd!(noundo "library.rename", "Rename DJ Library Item", [], None, "{id, name} — renames a folder or playlist", super::always, rename),
        cmd!(
            noundo "library.delete",
            "Delete DJ Library Item",
            [],
            None,
            "{id} — deletes a playlist, or a folder with everything in it (the Music app is never changed)",
            super::always,
            delete
        ),
        cmd!(noundo "library.move", "Move DJ Library Item", [], None, "{id, parent?: folder id (omit for the top)} — moves a folder or playlist, with its contents, to the end of parent", super::always, move_node),
        cmd!(
            noundo "library.playlist.add",
            "Add to DJ Library Playlist",
            [],
            None,
            "{playlist: id, tracks: [track ids], at?: position (default: the end)} — adds tracks in that order; unknown ids are skipped",
            super::always,
            add_tracks
        ),
        cmd!(
            noundo "library.playlist.remove",
            "Remove from DJ Library Playlist",
            [],
            None,
            "{playlist: id, positions: [0-based positions]} — removes those entries",
            super::always,
            remove_entries
        ),
        cmd!(
            noundo "library.playlist.reorder",
            "Reorder DJ Library Playlist",
            [],
            None,
            "{playlist: id, from: position, to: position} — moves one entry",
            super::always,
            reorder
        ),
        cmd!(
            noundo "library.import_rekordbox",
            "Import from rekordbox",
            [],
            None,
            "{path: a rekordbox XML export (rekordbox: File › Export Collection in xml format)} — matches its tracks to the library by file (else title, artist and length), keeps rekordbox's key, BPM, rating, colour, comment, play count and cues with each, adds tracks only rekordbox has, and puts its folders and playlists, in order, in a top-level \"rekordbox\" folder that replaces the previous import's. Read-only for rekordbox. Returns folder (its id), folders, playlists, entries, matched, added, missing, skipped",
            super::always,
            import_rekordbox
        ),
        cmd!(
            "library.import",
            "Add Library Track to Session",
            [],
            None,
            "{track: track id, at?: position (default: the insertion point), new_track?: true} — imports the track's audio file into the session",
            super::always,
            import
        ),
    ]
}

fn err(id: &str) -> impl Fn(String) -> EngineError + '_ {
    move |m| bad(id, m)
}

fn node_id(id: &str, p: &Value, key: &str) -> Result<u64> {
    p.get(key).and_then(Value::as_u64).ok_or_else(|| bad(id, format!("`{key}` must be a folder or playlist id")))
}

fn opt_node(id: &str, p: &Value, key: &str) -> Result<Option<u64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => node_id(id, p, key).map(Some),
    }
}

fn index(id: &str, p: &Value, key: &str) -> Result<usize> {
    let n = p.get(key).and_then(Value::as_u64).ok_or_else(|| bad(id, format!("`{key}` must be a position")))?;
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

fn track_ids(id: &str, p: &Value) -> Result<Vec<String>> {
    let Some(v) = p.get("tracks") else { return Ok(Vec::new()) };
    let a = v.as_array().ok_or_else(|| bad(id, "`tracks` must be a list of track ids"))?;
    if a.len() > library::MAX_PLAYLIST_TRACKS {
        return Err(bad(id, "too many tracks"));
    }
    Ok(a.iter().filter_map(Value::as_str).map(|s| s.chars().take(64).collect()).collect())
}

/// Saves the library after a change, when it has a file.
fn save(e: &mut Engine) {
    e.library.rev = e.library.rev.wrapping_add(1);
    if let Some(path) = e.library_path.clone()
        && let Err(err) = crate::library_store::save(&e.library, &path)
    {
        e.messages.push(format!("The DJ library could not be saved: {err}"));
    }
}

/// Seconds since 1970. The web build has no system clock here (`SystemTime::now` would panic
/// there), so it reads 0 and dates simply go unrecorded.
fn now() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    return std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    #[cfg(target_arch = "wasm32")]
    0
}

fn tree(e: &mut Engine, _: &Value) -> Result<Value> {
    let l = &e.library;
    let count = |id: u64| l.listing(Some(id)).map_or(0, |v| v.len());
    let nodes: Vec<Value> = l
        .nodes
        .iter()
        .map(|n| {
            json!({
                "id": n.id,
                "name": n.name,
                "kind": n.kind,
                "parent": n.parent,
                "depth": l.depth(n.id),
                "count": count(n.id),
            })
        })
        .collect();
    Ok(json!({"source": l.source(), "synced_at": l.synced_at, "tracks": l.tracks.len(), "nodes": nodes}))
}

/// The shown columns named in `columns`, else the default ones.
pub fn columns_param(id: &str, p: &Value) -> Result<Vec<Column>> {
    let Some(v) = p.get("columns") else { return Ok(Column::DEFAULT.to_vec()) };
    let a = v.as_array().ok_or_else(|| bad(id, "`columns` must be a list"))?;
    a.iter()
        .take(Column::ALL.len() * 2)
        .map(|c| {
            c.as_str()
                .and_then(Column::parse)
                .ok_or_else(|| bad(id, format!("unknown column {c} (title, artist, bpm, key, genre, time, rating, comment, added)")))
        })
        .collect()
}

pub fn track_json(t: &Track, position: usize, lists: Option<&Vec<&str>>) -> Value {
    json!({
        "id": t.id,
        "position": position,
        "title": t.title,
        "artist": t.artist,
        "bpm": t.bpm(),
        "key": t.key(),
        "rating": t.rating(),
        "comment": t.comment(),
        "colour": t.colour().map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}")),
        "play_count": t.rb.as_ref().map_or(0, |r| r.play_count),
        "cues": t.rb.as_ref().map_or(0, |r| r.cues.len()),
        "rekordbox_only": t.from_rekordbox_only(),
        "added": t.added().map(library::format_date),
        "genre": t.genre,
        "time": format_time(t.seconds),
        "seconds": t.seconds,
        "path": t.path,
        "playlists": lists.cloned().unwrap_or_default(),
    })
}

fn search(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.search";
    let query = Query::parse(str_param(p, "query").unwrap_or(""));
    let columns = columns_param(ID, p)?;
    let playlist = opt_node(ID, p, "playlist")?;
    let offset = usize::try_from(p.get("offset").and_then(Value::as_u64).unwrap_or(0)).unwrap_or(usize::MAX);
    let limit = usize::try_from(p.get("limit").and_then(Value::as_u64).unwrap_or(200).min(MAX_LIMIT)).unwrap_or(0);
    let sort = match str_param(p, "sort") {
        None => None,
        Some(c) => Some((Column::parse(c).ok_or_else(|| bad(ID, format!("unknown sort column `{c}`")))?, super::bool_or(p, "descending", false))),
    };
    let since = p.get("added_within_days").and_then(Value::as_u64).map(|days| now().saturating_sub(days.min(100_000).saturating_mul(86_400)));
    let l = &e.library;
    let total = l.listing(playlist).map_err(err(ID))?.len();
    let hits = l.find(playlist, &query, &columns, since, sort).map_err(err(ID))?;
    let lists = l.memberships();
    let tracks: Vec<Value> = hits.iter().skip(offset).take(limit).map(|(i, t)| track_json(t, *i, lists.get(t.id.as_str()))).collect();
    Ok(json!({"total": total, "matched": hits.len(), "understood": query.describe(), "tracks": tracks}))
}

fn sync(e: &mut Engine, p: &Value) -> Result<Value> {
    let source: String =
        str_param(p, "playlist").map_or_else(|| e.library.source().to_string(), |s| s.chars().take(library::MAX_TEXT_CHARS).collect());
    let read = match e.take_library_offer(&source) {
        Some(r) => r,
        None => library::apple_music::read_playlist(&source),
    };
    let tracks = read.map_err(EngineError::Io)?;
    let report = e.library.replace_tracks(&source, tracks, now());
    save(e);
    Ok(json!({"source": source, "tracks": report.tracks, "added": report.added, "removed": report.removed}))
}

fn add(e: &mut Engine, p: &Value, kind: NodeKind, id: &str) -> Result<Value> {
    let name = str_param(p, "name").ok_or_else(|| bad(id, "`name` required"))?;
    let parent = opt_node(id, p, "parent")?;
    let new = e.library.add(kind, name, parent).map_err(err(id))?;
    save(e);
    Ok(json!({"id": new}))
}

fn new_playlist(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.playlist.new";
    let tracks = track_ids(ID, p)?;
    let v = add(e, p, NodeKind::Playlist, ID)?;
    if let Some(new) = v.get("id").and_then(Value::as_u64)
        && !tracks.is_empty()
    {
        let n = e.library.add_tracks(new, &tracks, None).map_err(err(ID))?;
        save(e);
        return Ok(json!({"id": new, "added": n}));
    }
    Ok(v)
}

fn rename(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.rename";
    let id = node_id(ID, p, "id")?;
    let name = str_param(p, "name").ok_or_else(|| bad(ID, "`name` required"))?;
    e.library.rename(id, name).map_err(err(ID))?;
    save(e);
    Ok(json!({"id": id}))
}

fn delete(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.delete";
    let n = e.library.remove(node_id(ID, p, "id")?).map_err(err(ID))?;
    save(e);
    Ok(json!({"removed": n}))
}

fn move_node(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.move";
    let id = node_id(ID, p, "id")?;
    e.library.move_to(id, opt_node(ID, p, "parent")?).map_err(err(ID))?;
    save(e);
    Ok(json!({"id": id}))
}

fn add_tracks(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.playlist.add";
    let list = node_id(ID, p, "playlist")?;
    let tracks = track_ids(ID, p)?;
    let at = match p.get("at") {
        None | Some(Value::Null) => None,
        Some(_) => Some(index(ID, p, "at")?),
    };
    let n = e.library.add_tracks(list, &tracks, at).map_err(err(ID))?;
    save(e);
    Ok(json!({"added": n}))
}

fn remove_entries(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.playlist.remove";
    let list = node_id(ID, p, "playlist")?;
    let a = p.get("positions").and_then(Value::as_array).ok_or_else(|| bad(ID, "`positions` must be a list"))?;
    let positions: Vec<usize> =
        a.iter().take(library::MAX_PLAYLIST_TRACKS).filter_map(Value::as_u64).map(|n| usize::try_from(n).unwrap_or(usize::MAX)).collect();
    let n = e.library.remove_entries(list, &positions).map_err(err(ID))?;
    save(e);
    Ok(json!({"removed": n}))
}

fn reorder(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.playlist.reorder";
    let list = node_id(ID, p, "playlist")?;
    e.library.reorder(list, index(ID, p, "from")?, index(ID, p, "to")?).map_err(err(ID))?;
    save(e);
    Ok(json!({}))
}

fn import_rekordbox(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.import_rekordbox";
    let path = str_param(p, "path").ok_or_else(|| bad(ID, "`path` required"))?;
    let bytes = crate::io::read_file(path)?;
    let export = library::rekordbox::parse(&bytes).map_err(err(ID))?;
    let r = e.library.import_rekordbox(export, now());
    save(e);
    Ok(json!({
        "folder": e.library.rekordbox_root,
        "folders": r.folders,
        "playlists": r.playlists,
        "entries": r.entries,
        "matched": r.matched,
        "added": r.added,
        "missing": r.missing,
        "skipped": r.skipped,
    }))
}

fn import(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "library.import";
    let id = str_param(p, "track").ok_or_else(|| bad(ID, "`track` required"))?;
    let t = e.library.track(id).ok_or_else(|| EngineError::NotFound(format!("no library track {id}")))?;
    let path = t.path.clone().ok_or_else(|| EngineError::NotFound(format!("“{}” has no audio file on this computer", t.title)))?;
    let mut args = json!({"path": path, "new_tracks": super::bool_or(p, "new_track", true)});
    if let Some(at) = p.get("at") {
        args["at"] = at.clone();
    } else {
        args["at"] = json!(e.session().edit.selection.start);
    }
    super::file::import_audio(e, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut e = Engine::default();
        let t = |id: &str, title: &str, bpm: u16, key: &str, genre: &str| Track {
            id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            bpm: Some(bpm),
            key: Some(key.into()),
            genre: genre.into(),
            seconds: 300,
            path: None,
            added: None,
            rb: None,
        };
        e.offer_library(
            "DJ",
            Ok(vec![
                t("a", "Siren", 120, "7A", "Afro House"),
                t("b", "Electric Bongo", 130, "11A", "Tech House"),
                t("c", "Yeki", 122, "4A", "Afro House"),
            ]),
        );
        assert!(e.execute("library.sync", &json!({"playlist": "DJ"})).is_ok());
        e
    }

    #[test]
    fn an_assistant_can_build_a_playlist() {
        let mut e = engine();
        let folder = e.execute("library.folder.new", &json!({"name": "la caña"})).ok().and_then(|v| v["id"].as_u64());
        let found =
            e.execute("library.search", &json!({"query": "afro 120-124", "columns": ["title", "artist", "bpm", "key", "genre"]})).unwrap_or_default();
        assert_eq!(found["matched"], 2);
        let ids: Vec<Value> = found["tracks"].as_array().cloned().unwrap_or_default().iter().map(|t| t["id"].clone()).collect();
        let list = e.execute("library.playlist.new", &json!({"name": "warm up 3.10", "parent": folder, "tracks": ids})).unwrap_or_default();
        let list = list["id"].as_u64();
        assert!(e.execute("library.playlist.reorder", &json!({"playlist": list, "from": 1, "to": 0})).is_ok());
        let in_list = e.execute("library.search", &json!({"playlist": list})).unwrap_or_default();
        let titles: Vec<&str> = in_list["tracks"].as_array().map(|a| a.iter().filter_map(|t| t["title"].as_str()).collect()).unwrap_or_default();
        assert_eq!(titles, ["Yeki", "Siren"]);
        let tree = e.execute("library.tree", &json!({})).unwrap_or_default();
        assert_eq!(tree["nodes"][1]["count"], 2);
        assert_eq!(tree["nodes"][1]["depth"], 1);
        assert!(!e.can_undo(), "library commands leave the session's undo alone");
    }

    #[test]
    fn hidden_columns_are_not_searched() {
        let mut e = engine();
        let r = e.execute("library.search", &json!({"query": "afro"})).unwrap_or_default();
        assert_eq!(r["matched"], 0, "genre is not a default column");
        let r = e.execute("library.search", &json!({"query": "genre:afro"})).unwrap_or_default();
        assert_eq!(r["matched"], 2);
    }

    #[test]
    fn hostile_params_are_errors() {
        let mut e = engine();
        for (id, p) in [
            ("library.search", json!({"columns": ["nope"]})),
            ("library.search", json!({"playlist": 99})),
            ("library.rename", json!({"id": "x", "name": "y"})),
            ("library.playlist.add", json!({"playlist": 99, "tracks": ["a"]})),
            ("library.playlist.reorder", json!({"playlist": 1, "from": -1, "to": 0})),
            ("library.import", json!({"track": "a"})),
            ("library.folder.new", json!({"name": ""})),
        ] {
            assert!(e.execute(id, &p).is_err(), "{id} {p}");
        }
        let r = e.execute("library.search", &json!({"limit": u64::MAX, "offset": u64::MAX})).unwrap_or_default();
        assert_eq!(r["tracks"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn a_rekordbox_export_brings_its_playlists_and_keys() {
        let dir = std::env::temp_dir().join(format!("soundcraft-rekordbox-cmd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let xml = dir.join("rekordbox.xml");
        let mut e = Engine::default();
        let music = |id: &str, title: &str, path: &str| Track {
            id: id.into(),
            title: title.into(),
            seconds: 300,
            path: Some(path.into()),
            bpm: Some(100),
            ..Track::default()
        };
        // The Music app gives a decomposed "ñ"; the export a composed one.
        e.offer_library("DJ", Ok(vec![music("m1", "Siren", "/M/Siren.mp3"), music("m2", "Cana", "/M/Can\u{0303}a.mp3")]));
        assert!(e.execute("library.sync", &json!({"playlist": "DJ"})).is_ok());
        let body = r#"<DJ_PLAYLISTS Version="1.0.0"><COLLECTION>
<TRACK TrackID="1" Name="Siren" Artist="Rancido" AverageBpm="120.00" Tonality="Dm" Rating="255" Location="file://localhost/M/Siren.mp3"><POSITION_MARK Start="1.5" Num="0"/></TRACK>
<TRACK TrackID="2" Name="Cana" AverageBpm="96.00" Tonality="4A" Location="file://localhost/M/Ca%C3%B1a.mp3"/>
<TRACK TrackID="3" Name="Only Here" Artist="X" AverageBpm="128.00" Location="file://localhost/Other/only.mp3"/>
<TRACK TrackID="4" Name="No File"/>
</COLLECTION><PLAYLISTS><NODE Type="0" Name="ROOT">
<NODE Type="0" Name="la caña"><NODE Type="1" Name="peak 3.10" KeyType="0"><TRACK Key="3"/><TRACK Key="1"/><TRACK Key="4"/></NODE></NODE>
</NODE></PLAYLISTS></DJ_PLAYLISTS>"#;
        let _ = std::fs::write(&xml, body);
        let r = e.execute("library.import_rekordbox", &json!({"path": xml.to_string_lossy()})).unwrap_or_default();
        assert_eq!(
            (r["folders"].as_u64(), r["playlists"].as_u64(), r["matched"].as_u64(), r["added"].as_u64(), r["missing"].as_u64()),
            (Some(1), Some(1), Some(2), Some(1), Some(1)),
            "{r}"
        );
        let siren = e.library.track("m1").cloned().unwrap_or_default();
        assert_eq!((siren.bpm(), siren.key(), siren.rating()), (Some(120), Some("7A"), 5));
        assert_eq!(e.library.track("m2").and_then(|t| t.key().map(str::to_string)).as_deref(), Some("4A"), "paths match across Unicode forms");
        let tree = e.execute("library.tree", &json!({})).unwrap_or_default();
        let names: Vec<&str> = tree["nodes"].as_array().map(|a| a.iter().filter_map(|n| n["name"].as_str()).collect()).unwrap_or_default();
        assert_eq!(names, ["rekordbox", "la caña", "peak 3.10"]);
        let list = tree["nodes"][2]["id"].as_u64();
        let rows = e.execute("library.search", &json!({"playlist": list})).unwrap_or_default();
        let titles: Vec<&str> = rows["tracks"].as_array().map(|a| a.iter().filter_map(|t| t["title"].as_str()).collect()).unwrap_or_default();
        assert_eq!(titles, ["Only Here", "Siren"], "rekordbox's order; the track without a file is left out");
        // Searching uses rekordbox's data.
        let hits = e.execute("library.search", &json!({"query": "rating:5 7a"})).unwrap_or_default();
        assert_eq!(hits["matched"], 1);
        // A new Music read keeps it, and importing again replaces the folder rather than adding one.
        e.offer_library("DJ", Ok(vec![music("m1", "Siren", "/M/Siren.mp3"), music("m3", "Only Here", "/Other/only.mp3")]));
        assert!(e.execute("library.sync", &json!({"playlist": "DJ"})).is_ok());
        assert_eq!(e.library.track("m1").and_then(|t| t.key().map(str::to_string)).as_deref(), Some("7A"), "a sync keeps rekordbox's key");
        let rows = e.execute("library.search", &json!({"playlist": list})).unwrap_or_default();
        assert_eq!(rows["tracks"][0]["id"], "m3", "a rekordbox-only track now in Music is the Music track in playlists");
        assert!(e.execute("library.import_rekordbox", &json!({"path": xml.to_string_lossy()})).is_ok());
        let tree = e.execute("library.tree", &json!({})).unwrap_or_default();
        assert_eq!(tree["nodes"].as_array().map(Vec::len), Some(3));
        let _ = std::fs::write(&xml, "<not rekordbox/>");
        assert!(e.execute("library.import_rekordbox", &json!({"path": xml.to_string_lossy()})).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sorts_by_any_column_and_finds_recent_additions() {
        let mut e = Engine::default();
        let day = 86_400;
        let t = |id: &str, bpm: Option<u16>, key: Option<&str>, days_ago: Option<u64>| Track {
            id: id.into(),
            title: id.to_uppercase(),
            bpm,
            key: key.map(str::to_string),
            added: days_ago.map(|d| now().saturating_sub(d * day)),
            ..Track::default()
        };
        e.offer_library(
            "DJ",
            Ok(vec![
                t("a", Some(128), Some("12A"), Some(40)),
                t("b", None, None, None),
                t("c", Some(94), Some("1B"), Some(2)),
                t("d", Some(120), Some("1A"), Some(10)),
            ]),
        );
        assert!(e.execute("library.sync", &json!({"playlist": "DJ"})).is_ok());
        let ids = |e: &mut Engine, p: Value| -> Vec<String> {
            e.execute("library.search", &p).unwrap_or_default()["tracks"]
                .as_array()
                .map(|a| a.iter().filter_map(|t| t["id"].as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        assert_eq!(ids(&mut e, json!({"sort": "bpm"})), ["c", "d", "a", "b"], "lowest first, no BPM last");
        assert_eq!(ids(&mut e, json!({"sort": "bpm", "descending": true})), ["a", "d", "c", "b"], "highest first, no BPM still last");
        assert_eq!(ids(&mut e, json!({"sort": "key"})), ["d", "c", "a", "b"], "wheel order 1A, 1B, … 12A");
        assert_eq!(ids(&mut e, json!({"sort": "added", "descending": true})), ["c", "d", "a", "b"], "newest first");
        assert_eq!(ids(&mut e, json!({"added_within_days": 30, "sort": "added", "descending": true})), ["c", "d"]);
        assert_eq!(ids(&mut e, json!({})), ["a", "b", "c", "d"], "no sort keeps the playlist's order");
        assert!(e.execute("library.search", &json!({"sort": "nope"})).is_err());
        assert!(e.execute("library.search", &json!({"added_within_days": u64::MAX})).is_ok());
    }

    #[test]
    fn changes_are_saved() {
        let dir = std::env::temp_dir().join(format!("soundcraft-library-cmd-{}", std::process::id()));
        let path = dir.join("DJ Library.json");
        let mut e = engine();
        assert!(e.open_library(path.clone()).is_ok());
        assert!(e.execute("library.playlist.new", &json!({"name": "salsa"})).is_ok());
        let mut again = Engine::default();
        assert!(again.open_library(path).is_ok());
        assert_eq!(again.library.nodes.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
