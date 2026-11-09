//! View › Simple View: one switch to a plainer Edit window for editing a single song, and back.
//! It hides the side lists and every time ruler but Min:Secs and Markers (the Lyrics ruler stays as
//! it is), makes the tracks Large, and picks the Smart tool in Slip mode. Switching it off puts back
//! exactly what it changed.

use crate::SoundApp;
use serde_json::{Value, json};
use soundcraft_model::EditMode;

/// The time rulers `view.ruler` knows, in its order.
const RULERS: [&str; 10] = ["bars_beats", "min_secs", "timecode", "feet_frames", "samples", "tempo", "meter", "markers", "key", "chords"];
/// The rulers Simple View keeps.
const KEPT: [&str; 2] = ["min_secs", "markers"];

/// What Simple View changed, to put back when it is switched off.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    tracks_list: bool,
    clip_list: bool,
    rulers: Vec<String>,
    /// Each track's height in points.
    heights: Vec<(soundcraft_model::TrackId, f32)>,
    tool: &'static str,
    mode: &'static str,
}

fn mode_id(m: EditMode) -> &'static str {
    match m {
        EditMode::Shuffle => "shuffle",
        EditMode::Slip => "slip",
        EditMode::Spot => "spot",
        EditMode::Grid => "grid",
        EditMode::GridRelative => "relative_grid",
    }
}

/// `view.simple`: `{value?: bool}` switches Simple View on or off (toggles without `value`).
pub fn toggle(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let on = p.get("value").and_then(Value::as_bool).unwrap_or(app.ui.simple_view.is_none());
    match (on, app.ui.simple_view.take()) {
        (true, None) => app.ui.simple_view = Some(simplify(app)),
        (false, Some(saved)) => restore(app, &saved),
        (_, kept) => app.ui.simple_view = kept,
    }
    Ok(json!({"value": app.ui.simple_view.is_some()}))
}

fn simplify(app: &mut SoundApp) -> Saved {
    let s = app.engine.session();
    let saved = Saved {
        tracks_list: app.ui.show_tracks_list,
        clip_list: app.ui.show_clip_list,
        rulers: s.edit.rulers.clone(),
        heights: s.tracks.iter().map(|t| (t.id, t.height.points())).collect(),
        tool: s.edit.tool.id(),
        mode: mode_id(s.edit.edit_mode),
    };
    let tracks: Vec<u64> = s.tracks.iter().map(|t| t.id.0).collect();
    app.ui.show_tracks_list = false;
    app.ui.show_clip_list = false;
    for r in RULERS {
        let _ = app.run("view.ruler", json!({"ruler": r, "visible": KEPT.contains(&r)}));
    }
    if !tracks.is_empty() {
        let _ = app.run("track.height", json!({"tracks": tracks, "height": "large"}));
    }
    let _ = app.run("edit.tool", json!({"tool": "smart"}));
    let _ = app.run("edit.mode", json!({"mode": "slip"}));
    saved
}

fn restore(app: &mut SoundApp, saved: &Saved) {
    app.ui.show_tracks_list = saved.tracks_list;
    app.ui.show_clip_list = saved.clip_list;
    for r in RULERS {
        let _ = app.run("view.ruler", json!({"ruler": r, "visible": saved.rulers.iter().any(|x| x == r)}));
    }
    for (track, points) in &saved.heights {
        // Tracks deleted meanwhile are skipped; new ones keep their height.
        if app.engine.session().track(*track).is_some() {
            let _ = app.run("track.height", json!({"tracks": [track.0], "height": points}));
        }
    }
    let _ = app.run("edit.tool", json!({"tool": saved.tool}));
    let _ = app.run("edit.mode", json!({"mode": saved.mode}));
}
