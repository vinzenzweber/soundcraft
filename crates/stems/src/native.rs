//! The native backend: download with ureq, HTDemucs on Burn's wgpu backend.

use crate::{Error, Model, Separated, Stem};
use burn::backend::wgpu::{Wgpu, WgpuDevice};
use demucs_core::listener::{ForwardEvent, ForwardListener};
use demucs_core::model::metadata::StemId;
use demucs_core::{Demucs, ModelOptions};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

type B = Wgpu;

/// The loaded model, kept between calls: loading and GPU setup cost more than a short clip.
struct Loaded {
    key: String,
    model: Demucs<B>,
}

static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);
static GPU_CONFIG: Once = Once::new();

pub fn install(dir: &Path, model: &'static Model, progress: &mut dyn FnMut(u64, u64), cancel: &AtomicBool) -> Result<PathBuf, Error> {
    std::fs::create_dir_all(dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
    let dest = model.path(dir);
    let part = dir.join(format!("{}.part", model.file));
    let r = download(model, &part, progress, cancel);
    if let Err(e) = r {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        Error::Io(format!("{}: {e}", dest.display()))
    })?;
    Ok(dest)
}

/// Stream `model.url` into `part`, hashing as it goes; fails unless size and SHA-256 match.
fn download(model: &Model, part: &Path, progress: &mut dyn FnMut(u64, u64), cancel: &AtomicBool) -> Result<(), Error> {
    let resp = ureq::get(model.url).call().map_err(|e| Error::Download(e.to_string()))?;
    // One byte over the catalog size is enough to know the file is wrong.
    let mut body = resp.into_body().into_with_config().limit(model.bytes.saturating_add(1)).reader();
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
        if done > model.bytes {
            return Err(Error::Download(format!("{} is larger than expected", model.file)));
        }
        hash.update(chunk);
        file.write_all(chunk).map_err(|e| Error::Io(format!("{}: {e}", part.display())))?;
        progress(done, model.bytes);
    }
    file.sync_all().map_err(|e| Error::Io(format!("{}: {e}", part.display())))?;
    if done != model.bytes {
        return Err(Error::Download(format!("{} arrived incomplete ({done} of {} bytes)", model.file, model.bytes)));
    }
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != model.sha256 {
        return Err(Error::Download(format!("{} does not match its checksum", model.file)));
    }
    Ok(())
}

fn stem_id(s: Stem) -> StemId {
    match s {
        Stem::Vocals => StemId::Vocals,
        Stem::Drums => StemId::Drums,
        Stem::Bass => StemId::Bass,
        Stem::Other => StemId::Other,
        Stem::Guitar => StemId::Guitar,
        Stem::Piano => StemId::Piano,
    }
}

fn from_stem_id(s: StemId) -> Stem {
    match s {
        StemId::Vocals => Stem::Vocals,
        StemId::Drums => Stem::Drums,
        StemId::Bass => Stem::Bass,
        StemId::Other => Stem::Other,
        StemId::Guitar => Stem::Guitar,
        StemId::Piano => Stem::Piano,
    }
}

/// Reports chunk progress and passes the cancel flag to the model.
struct Listener<'a> {
    progress: &'a mut dyn FnMut(f32),
    cancel: &'a AtomicBool,
    /// Sub-models per chunk: the fine-tuned model runs one per stem.
    per_chunk: usize,
    chunk: usize,
    chunks: usize,
    in_chunk: usize,
}

impl ForwardListener for Listener<'_> {
    fn on_event(&mut self, event: ForwardEvent) {
        match event {
            ForwardEvent::ChunkStarted { index, total } => {
                self.chunk = index;
                self.chunks = total.max(1);
                self.in_chunk = 0;
            }
            ForwardEvent::StemDone { .. } => self.in_chunk = self.in_chunk.saturating_add(1).min(self.per_chunk),
            ForwardEvent::ChunkDone { index, total } => {
                self.chunk = index.saturating_add(1);
                self.chunks = total.max(1);
                self.in_chunk = 0;
            }
            _ => return,
        }
        let within = self.in_chunk as f32 / self.per_chunk.max(1) as f32;
        (self.progress)(((self.chunk as f32 + within) / self.chunks as f32).clamp(0.0, 1.0));
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Keep GPU kernel tuning results next to the models instead of cubecl's default, which is a
/// `target` folder under the current directory.
fn configure_gpu(dir: &Path) {
    let cache = dir.join("gpu-cache");
    GPU_CONFIG.call_once(|| {
        use cubecl::config::{GlobalConfig, autotune::AutotuneConfig, cache::CacheConfig};
        let config = GlobalConfig { autotune: AutotuneConfig { cache: CacheConfig::File(cache), ..Default::default() }, ..Default::default() };
        // `set` panics when something already configured cubecl; then its settings stand.
        if catch_unwind(|| GlobalConfig::set(config)).is_err() {
            log::warn!("stems: GPU settings were already configured");
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub fn separate(
    dir: &Path,
    model: &'static Model,
    left: &[f32],
    right: &[f32],
    rate: u32,
    wanted: &[Stem],
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<Vec<Separated>, Error> {
    if !crate::installed(dir, model) {
        return Err(Error::NotInstalled(model.id));
    }
    configure_gpu(dir);
    let fine_tuned = model.id == "htdemucs_ft";
    let key = if fine_tuned { format!("{}:{}", model.id, wanted.iter().map(|s| s.id()).collect::<Vec<_>>().join(",")) } else { model.id.to_string() };
    // A panic left the lock poisoned: the model may be half-built, so load it again.
    let mut slot = LOADED.lock().unwrap_or_else(|p| {
        let mut g = p.into_inner();
        *g = None;
        g
    });
    if slot.as_ref().is_none_or(|l| l.key != key) {
        *slot = None;
        let bytes = std::fs::read(model.path(dir)).map_err(|e| Error::Io(format!("{}: {e}", model.path(dir).display())))?;
        let opts = if fine_tuned {
            ModelOptions::FineTuned(wanted.iter().map(|s| stem_id(*s)).collect())
        } else if model.id == "htdemucs_6s" {
            ModelOptions::SixStem
        } else {
            ModelOptions::FourStem
        };
        let loaded = catch_unwind(AssertUnwindSafe(|| Demucs::<B>::from_bytes(opts, &bytes, WgpuDevice::default())))
            .map_err(|_| Error::Model("the model could not be loaded on this GPU".into()))?
            .map_err(|e| Error::Model(e.to_string()))?;
        *slot = Some(Loaded { key, model: loaded });
    }
    let Some(loaded) = slot.as_ref() else { return Err(Error::Model("the model is not loaded".into())) };
    let per_chunk = if fine_tuned { wanted.len().max(1) } else { 1 };
    let mut listener = Listener { progress, cancel, per_chunk, chunk: 0, chunks: 1, in_chunk: 0 };
    let run = catch_unwind(AssertUnwindSafe(|| pollster::block_on(loaded.model.separate_with_listener(left, right, rate, &mut listener))));
    let stems = match run {
        Ok(Ok(s)) => s,
        Ok(Err(demucs_core::DemucsError::Cancelled)) => return Err(Error::Cancelled),
        Ok(Err(e)) => return Err(Error::Model(e.to_string())),
        Err(_) => {
            *slot = None;
            return Err(Error::Model("the model stopped unexpectedly".into()));
        }
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let n = left.len();
    let mut out = Vec::new();
    for want in wanted {
        let Some(s) = stems.iter().find(|s| from_stem_id(s.id) == *want) else { continue };
        // Resampling can leave the length a frame off; the caller places stems sample-exact.
        let fit = |x: &[f32]| {
            let mut v: Vec<f32> = x.iter().take(n).map(|v| if v.is_finite() { *v } else { 0.0 }).collect();
            v.resize(n, 0.0);
            v
        };
        out.push(Separated { stem: *want, audio: [fit(&s.left), fit(&s.right)] });
    }
    if out.len() != wanted.len() {
        return Err(Error::Model("the model returned fewer stems than requested".into()));
    }
    (listener.progress)(1.0);
    Ok(out)
}
