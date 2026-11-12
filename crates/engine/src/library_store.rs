//! The DJ library's file in the app's settings folder. Like `autosave`, this reads and writes a
//! fixed file of the app's own that no command names, so it does not go through the checked
//! `io` helpers (see `file_access`'s guard test).

use crate::library::{Library, MAX_FILE_BYTES};
use std::path::Path;

/// The saved library at `path`; `Ok(None)` when there is none yet.
pub fn load(path: &Path) -> Result<Option<Library>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if meta.len() > MAX_FILE_BYTES {
        return Err("the library file is too large".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Library::from_bytes(&bytes).map(Some)
}

/// Writes the library beside `path` and moves it into place, so a crash never leaves half a file.
pub fn save(lib: &Library, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec(lib).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Keeps a file that could not be read aside as `….json.damaged`, so the next save cannot
/// overwrite the user's playlists.
pub fn set_aside(path: &Path) {
    let _ = std::fs::rename(path, path.with_extension("json.damaged"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::NodeKind;

    #[test]
    fn saves_loads_and_sets_damaged_files_aside() {
        let dir = std::env::temp_dir().join(format!("soundcraft-library-store-{}", std::process::id()));
        let path = dir.join("DJ Library.json");
        assert_eq!(load(&path), Ok(None));
        let mut l = Library::default();
        let f = l.add(NodeKind::Folder, "la caña", None).unwrap_or(0);
        l.add(NodeKind::Playlist, "peak 3.10", Some(f)).unwrap_or(0);
        assert!(save(&l, &path).is_ok());
        assert_eq!(load(&path), Ok(Some(l)));
        let _ = std::fs::write(&path, b"not json");
        assert!(load(&path).is_err());
        set_aside(&path);
        assert!(path.with_extension("json.damaged").exists());
        assert_eq!(load(&path), Ok(None));
        let _ = std::fs::remove_dir_all(dir);
    }
}
