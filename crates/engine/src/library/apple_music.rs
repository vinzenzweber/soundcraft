//! Reading a playlist from the Music app on macOS, read-only, through its scripting interface
//! (`osascript`, JavaScript for Automation). Nothing is ever written to the Music library.

use super::{MAX_TRACKS, Track, search::wheel_key};

/// Longest output read from the Music app (a 100 000-track playlist is about 40 MB).
#[cfg(target_os = "macos")]
const MAX_OUTPUT_BYTES: usize = 128 << 20;

/// Reads every file track of a Music playlist. The playlist is the first user playlist with
/// that name. Takes a few seconds for thousands of tracks, so callers keep it off the UI thread.
#[cfg(target_os = "macos")]
pub fn read_playlist(name: &str) -> Result<Vec<Track>, String> {
    // One bulk read per property is far faster than one call per track.
    const SCRIPT: &str = r#"
function run(argv) {
  const music = Application('Music');
  const lists = music.userPlaylists.whose({name: argv[0]});
  if (lists.length === 0) return JSON.stringify({error: 'missing'});
  const t = lists[0].fileTracks;
  if (t.length === 0) return JSON.stringify({id: [], name: [], artist: [], bpm: [], genre: [], duration: [], comment: [], location: []});
  let location;
  try { location = t.location().map(l => l ? String(l) : ''); } catch (e) { location = []; }
  return JSON.stringify({id: t.persistentID(), name: t.name(), artist: t.artist(), bpm: t.bpm(), genre: t.genre(), duration: t.duration(), comment: t.comment(), location: location});
}"#;
    let out = std::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT, name])
        .output()
        .map_err(|e| format!("could not ask the Music app: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(if err.contains("-1743") {
            "SoundCraft is not allowed to read the Music app. Allow it in System Settings › Privacy & Security › Automation.".into()
        } else {
            format!("the Music app could not be read: {}", err.trim())
        });
    }
    if out.stdout.len() > MAX_OUTPUT_BYTES {
        return Err("the playlist is too large to read".into());
    }
    parse(&out.stdout, name)
}

#[cfg(not(target_os = "macos"))]
pub fn read_playlist(_name: &str) -> Result<Vec<Track>, String> {
    Err("reading the Music app is only available on macOS".into())
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct Columns {
    error: Option<String>,
    id: Vec<String>,
    name: Vec<serde_json::Value>,
    artist: Vec<serde_json::Value>,
    bpm: Vec<serde_json::Value>,
    genre: Vec<serde_json::Value>,
    duration: Vec<serde_json::Value>,
    comment: Vec<serde_json::Value>,
    location: Vec<serde_json::Value>,
}

fn text(v: Option<&serde_json::Value>) -> String {
    v.and_then(serde_json::Value::as_str).map(|s| s.trim().chars().take(super::MAX_TEXT_CHARS).collect()).unwrap_or_default()
}

/// The script's JSON as tracks. Each property is an array in track order.
pub(crate) fn parse(json: &[u8], name: &str) -> Result<Vec<Track>, String> {
    let c: Columns = serde_json::from_slice(json).map_err(|e| format!("unexpected answer from the Music app: {e}"))?;
    if c.error.is_some() {
        return Err(format!("there is no playlist named “{name}” in the Music app"));
    }
    let mut tracks = Vec::with_capacity(c.id.len().min(MAX_TRACKS));
    for (i, id) in c.id.iter().enumerate().take(MAX_TRACKS) {
        let bpm = c.bpm.get(i).and_then(serde_json::Value::as_f64).filter(|b| b.is_finite() && *b >= 1.0 && *b <= 999.0).map(|b| b.round() as u16);
        let seconds = c
            .duration
            .get(i)
            .and_then(serde_json::Value::as_f64)
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map_or(0, |d| d.min(f64::from(u32::MAX)).round() as u32);
        // Key-detection tools write the key first in the comment ("5A - Energy 8").
        let comment = text(c.comment.get(i));
        let key = comment.split(|ch: char| ch.is_whitespace() || ch == '-' || ch == ',').next().and_then(wheel_key);
        let path = text(c.location.get(i));
        tracks.push(Track {
            id: id.chars().take(64).collect(),
            title: text(c.name.get(i)),
            artist: text(c.artist.get(i)),
            bpm,
            key,
            genre: text(c.genre.get(i)),
            seconds,
            path: (!path.is_empty()).then_some(path),
        });
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_scripts_columns() {
        let json = br#"{"id":["A1","B2"],"name":["Siren (Original Mix)",null],"artist":["Rancido",""],"bpm":[120,0],"genre":["Afro House",""],"duration":[408.4,"x"],"comment":["7A - Energy 6","6"],"location":["/Music/siren.mp3",""]}"#;
        let t = parse(json, "DJ").unwrap_or_default();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].bpm, Some(120));
        assert_eq!(t[0].key.as_deref(), Some("7A"));
        assert_eq!(t[0].seconds, 408);
        assert_eq!(t[0].path.as_deref(), Some("/Music/siren.mp3"));
        assert_eq!((t[1].bpm, t[1].key.clone(), t[1].seconds, t[1].path.clone()), (None, None, 0, None), "a bare energy number is not a key");
    }

    #[test]
    fn missing_playlist_and_junk_are_errors() {
        assert!(parse(br#"{"error":"missing"}"#, "DJ").is_err_and(|e| e.contains("DJ")));
        assert!(parse(b"not json", "DJ").is_err());
        assert!(parse(br#"{"id":[]}"#, "DJ").is_ok_and(|t| t.is_empty()));
        assert!(parse(br#"{"id":["a"],"bpm":[1e300],"duration":[-5]}"#, "DJ").is_ok_and(|t| t[0].bpm.is_none() && t[0].seconds == 0));
    }
}
