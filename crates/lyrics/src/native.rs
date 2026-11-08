//! The native backend: download with ureq, Parakeet on Burn's wgpu backend.

use crate::{Error, Model, Word};
use parakeet_rs::{ExecutionConfig, ExecutionProvider, ParakeetTDT, TimestampMode, Transcriber};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

/// The loaded model, kept between calls: loading 2.5 GB of weights costs seconds.
static LOADED: Mutex<Option<(String, ParakeetTDT)>> = Mutex::new(None);
static GPU_CONFIG: Once = Once::new();

/// About 90 s per chunk: progress, cancel and bounded GPU memory for long songs.
const CHUNK: usize = 90 * crate::MODEL_RATE as usize;
const SEARCH: usize = 8 * crate::MODEL_RATE as usize;

pub fn install(dir: &Path, model: &'static Model, progress: &mut dyn FnMut(u64, u64), cancel: &AtomicBool) -> Result<PathBuf, Error> {
    let dest = model.path(dir);
    std::fs::create_dir_all(&dest).map_err(|e| Error::Io(format!("{}: {e}", dest.display())))?;
    let total = model.bytes();
    let mut base = 0u64;
    for f in model.files {
        let path = dest.join(f.name);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == f.bytes) {
            base = base.saturating_add(f.bytes);
            progress(base, total);
            continue;
        }
        let part = dest.join(format!("{}.part", f.name));
        let url = format!("{}{}", model.base_url, f.name);
        let r = download(&url, f.bytes, f.sha256, &part, &mut |done| progress(base.saturating_add(done), total), cancel);
        if let Err(e) = r {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &path).map_err(|e| {
            let _ = std::fs::remove_file(&part);
            Error::Io(format!("{}: {e}", path.display()))
        })?;
        base = base.saturating_add(f.bytes);
    }
    Ok(dest)
}

fn download(url: &str, bytes: u64, sha256: &str, part: &Path, progress: &mut dyn FnMut(u64), cancel: &AtomicBool) -> Result<(), Error> {
    let resp = ureq::get(url).call().map_err(|e| Error::Download(e.to_string()))?;
    let mut body = resp.into_body().into_with_config().limit(bytes.saturating_add(1)).reader();
    let mut file = std::fs::File::create(part).map_err(|e| Error::Io(format!("{}: {e}", part.display())))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = body.read(&mut buf).map_err(|e| Error::Download(e.to_string()))?;
        let Some(chunk) = buf.get(..n) else { break };
        if chunk.is_empty() {
            break;
        }
        done = done.saturating_add(n as u64);
        if done > bytes {
            return Err(Error::Download(format!("{url} is larger than expected")));
        }
        hash.update(chunk);
        file.write_all(chunk).map_err(|e| Error::Io(format!("{}: {e}", part.display())))?;
        progress(done);
    }
    file.sync_all().map_err(|e| Error::Io(format!("{}: {e}", part.display())))?;
    if done != bytes {
        return Err(Error::Download(format!("{url} arrived incomplete ({done} of {bytes} bytes)")));
    }
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != sha256 {
        return Err(Error::Download(format!("{url} does not match its checksum")));
    }
    Ok(())
}

/// Keep GPU kernel tuning results next to the models instead of cubecl's default, which is a
/// `target` folder under the current directory.
fn configure_gpu(dir: &Path) {
    let cache = dir.join("gpu-cache-speech");
    GPU_CONFIG.call_once(|| {
        use cubecl::config::{CubeClRuntimeConfig, RuntimeConfig, cache::CacheConfig, environment::EnvironmentConfig};
        let config = CubeClRuntimeConfig {
            environment: EnvironmentConfig { path: CacheConfig::Directory(cache), ..Default::default() },
            ..Default::default()
        };
        // Fails when something already configured cubecl; then its settings stand.
        if !CubeClRuntimeConfig::try_set(config) {
            log::warn!("lyrics: GPU settings were already configured");
        }
    });
}

pub fn transcribe(dir: &Path, model: &'static Model, audio: &[f32], progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<Vec<Word>, Error> {
    if !crate::installed(dir, model) {
        return Err(Error::NotInstalled(model.id));
    }
    configure_gpu(dir);
    let mut slot = LOADED.lock().unwrap_or_else(|p| {
        let mut g = p.into_inner();
        *g = None;
        g
    });
    if slot.as_ref().is_none_or(|(id, _)| id != model.id) {
        *slot = None;
        let path = model.path(dir);
        let config = ExecutionConfig::new().with_execution_provider(ExecutionProvider::BurnWgpu);
        let loaded = catch_unwind(AssertUnwindSafe(|| ParakeetTDT::from_pretrained(&path, Some(config))))
            .map_err(|_| Error::Model("the speech model could not be loaded on this GPU".into()))?
            .map_err(|e| Error::Model(e.to_string()))?;
        *slot = Some((model.id.to_string(), loaded));
    }
    let Some((_, asr)) = slot.as_mut() else { return Err(Error::Model("the speech model is not loaded".into())) };
    let chunks = crate::chunk_bounds(audio, CHUNK, SEARCH);
    let total = chunks.len().max(1) as f32;
    let rate = crate::MODEL_RATE as f32;
    let mut words = Vec::new();
    progress(0.0);
    for (k, (a, b)) in chunks.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let Some(piece) = audio.get(*a..*b) else { continue };
        let piece: Vec<f32> = piece.iter().map(|v| if v.is_finite() { *v } else { 0.0 }).collect();
        let r = catch_unwind(AssertUnwindSafe(|| asr.transcribe_samples(piece, crate::MODEL_RATE, 1, Some(TimestampMode::Words))));
        let result = match r {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(Error::Model(e.to_string())),
            Err(_) => {
                *slot = None;
                return Err(Error::Model("the speech model stopped unexpectedly".into()));
            }
        };
        let offset = *a as f32 / rate;
        words.extend(result.tokens.into_iter().filter(|t| !t.text.trim().is_empty()).map(|t| Word {
            start: offset + t.start.max(0.0),
            end: offset + t.end.max(t.start).max(0.0),
            text: t.text.trim().to_string(),
        }));
        progress((k + 1) as f32 / total);
    }
    Ok(words)
}
