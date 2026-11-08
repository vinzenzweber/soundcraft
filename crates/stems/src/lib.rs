#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// Proving the GPU model `Send` (for the shared slot) walks wgpu's deeply nested types.
#![recursion_limit = "256"]
//! SoundCraft stem separation.
//!
//! Splits stereo or mono audio into stems (vocals, drums, bass, other, and guitar and piano with
//! the six-stem model) with HTDemucs v4 running locally on the GPU (Metal, Vulkan or DirectX
//! through wgpu). Nothing is uploaded.
//!
//! The model weights are not bundled. [`install`] downloads one model into a folder the caller
//! names, checks its size and SHA-256 against this catalog, and only then moves it in place.
//! [`separate`] loads an installed model (kept loaded for the next call), runs it chunk by chunk
//! with progress and cancel, and returns planar stems at the input's sample rate and length.
//!
//! Audio is planar `f32`: `audio[channel][frame]`.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[cfg(not(target_arch = "wasm32"))]
mod native;

/// One separated part of the mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stem {
    Vocals,
    Drums,
    Bass,
    Other,
    Guitar,
    Piano,
}

impl Stem {
    pub const ALL: [Stem; 6] = [Stem::Vocals, Stem::Drums, Stem::Bass, Stem::Other, Stem::Guitar, Stem::Piano];

    /// The id used in commands: `vocals`, `drums`, ...
    pub fn id(self) -> &'static str {
        match self {
            Stem::Vocals => "vocals",
            Stem::Drums => "drums",
            Stem::Bass => "bass",
            Stem::Other => "other",
            Stem::Guitar => "guitar",
            Stem::Piano => "piano",
        }
    }

    /// The name shown on tracks and in the dialog.
    pub fn label(self) -> &'static str {
        match self {
            Stem::Vocals => "Vocals",
            Stem::Drums => "Drums",
            Stem::Bass => "Bass",
            Stem::Other => "Other",
            Stem::Guitar => "Guitar",
            Stem::Piano => "Piano",
        }
    }

    /// Parses an id, ignoring ASCII case.
    pub fn parse(s: &str) -> Option<Stem> {
        Stem::ALL.into_iter().find(|x| x.id().eq_ignore_ascii_case(s.trim()))
    }
}

/// A model SoundCraft can download and run.
#[derive(Debug)]
pub struct Model {
    /// `htdemucs`, `htdemucs_ft` or `htdemucs_6s`.
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// File name inside the models folder.
    pub file: &'static str,
    /// Exact size of the file in bytes.
    pub bytes: u64,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: &'static str,
    pub url: &'static str,
    /// The stems this model produces.
    pub stems: &'static [Stem],
    /// Where the weights come from and under which terms, shown before a download.
    pub terms: &'static str,
}

impl Model {
    /// Size in whole megabytes, for labels.
    pub fn megabytes(&self) -> u64 {
        self.bytes.div_ceil(1_000_000)
    }

    /// Where this model's file sits inside `dir`.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(self.file)
    }
}

const TERMS: &str = "HTDemucs weights by Meta AI Research (Défossez et al.), in the safetensors conversion published by the demucs-rs project. \
The Demucs code is MIT-licensed; its authors describe the pretrained weights as released for research, so check the terms before commercial use.";

/// The models in the order the dialog offers them; the first is the default.
pub static MODELS: [Model; 3] = [
    Model {
        id: "htdemucs",
        label: "Standard",
        description: "Fast, good quality: vocals, drums, bass, other",
        file: "htdemucs.safetensors",
        bytes: 84_030_696,
        sha256: "8193504cdfb3943adaf039b8acb524a46e87ebf232c383ac7a32c80a6578423e",
        url: "https://huggingface.co/set-soft/audio_separation/resolve/main/Demucs/htdemucs.safetensors",
        stems: &[Stem::Vocals, Stem::Drums, Stem::Bass, Stem::Other],
        terms: TERMS,
    },
    Model {
        id: "htdemucs_ft",
        label: "Fine-tuned",
        description: "Best quality, about 4x slower: vocals, drums, bass, other",
        file: "htdemucs_ft.safetensors",
        bytes: 336_125_008,
        sha256: "255c2650d26537ce4887c9c4cf08c6d4896fad2fecc0b78dc5b875b117bcc575",
        url: "https://huggingface.co/set-soft/audio_separation/resolve/main/Demucs/htdemucs_ft.safetensors",
        stems: &[Stem::Vocals, Stem::Drums, Stem::Bass, Stem::Other],
        terms: TERMS,
    },
    Model {
        id: "htdemucs_6s",
        label: "Six stems",
        description: "Adds guitar and piano (experimental; piano is weak)",
        file: "htdemucs_6s.safetensors",
        bytes: 54_890_960,
        sha256: "f56fe666f0bbf3a645764856ed90f8c5dd8cf4430b1d9649b94e29ec45aa9057",
        url: "https://huggingface.co/set-soft/audio_separation/resolve/main/Demucs/htdemucs_6s.safetensors",
        stems: &[Stem::Vocals, Stem::Drums, Stem::Bass, Stem::Other, Stem::Guitar, Stem::Piano],
        terms: TERMS,
    },
];

/// The model with `id` (case-insensitive).
pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id.eq_ignore_ascii_case(id.trim()))
}

/// The model's sample rate; other rates are converted on the way in and out.
pub const MODEL_RATE: u32 = 44_100;

/// The longest audio one call separates: 20 minutes at 48 kHz. The model keeps every stem of the
/// whole input in memory, so longer input is refused instead of risking the machine's memory.
pub const MAX_FRAMES: usize = 20 * 60 * 48_000;

/// Why a separation or download did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// This build cannot run the model (the web build).
    Unsupported,
    /// The model is not in the models folder; run [`install`] first.
    NotInstalled(&'static str),
    /// The caller set the cancel flag.
    Cancelled,
    /// Input the model cannot take (empty, too long, too many channels, not finite).
    BadInput(String),
    /// The download failed or the file did not match the catalog.
    Download(String),
    /// Loading or running the model failed.
    Model(String),
    /// Reading or writing the models folder failed.
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported => write!(f, "stem separation is not available in this build"),
            Error::NotInstalled(id) => write!(f, "the `{id}` model is not installed (stems.install_model)"),
            Error::Cancelled => write!(f, "stem separation was cancelled"),
            Error::BadInput(m) => write!(f, "{m}"),
            Error::Download(m) => write!(f, "model download failed: {m}"),
            Error::Model(m) => write!(f, "stem separation failed: {m}"),
            Error::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

/// Whether the file for `model` is in `dir` with the catalog size. The full hash is checked when
/// it is installed; this cheap check catches a missing or truncated file.
pub fn installed(dir: &Path, model: &Model) -> bool {
    std::fs::metadata(model.path(dir)).is_ok_and(|m| m.is_file() && m.len() == model.bytes)
}

/// Whether this build can run the model at all.
pub fn supported() -> bool {
    cfg!(not(target_arch = "wasm32"))
}

/// Download `model` into `dir`, verify it and move it in place. `progress(done, total)` is called
/// as bytes arrive; setting `cancel` stops the download and removes the partial file.
pub fn install(dir: &Path, model: &'static Model, progress: &mut dyn FnMut(u64, u64), cancel: &AtomicBool) -> Result<PathBuf, Error> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::install(dir, model, progress, cancel)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (dir, model, progress, cancel);
        Err(Error::Unsupported)
    }
}

/// What to separate.
pub struct Request<'a> {
    pub model: &'static Model,
    /// Planar audio, one or two channels (more are mixed down to stereo by the caller).
    pub audio: &'a [Vec<f32>],
    pub sample_rate: u32,
    /// The stems to return, in this order; empty = all of the model's stems.
    pub stems: &'a [Stem],
}

/// One separated stem: stereo, the input's rate and length.
pub struct Separated {
    pub stem: Stem,
    pub audio: [Vec<f32>; 2],
}

/// Separate `req.audio` with the installed model in `dir`. `progress` gets 0..=1; `cancel` stops
/// between chunks. A panic inside the model is caught and returned as [`Error::Model`].
pub fn separate(dir: &Path, req: &Request<'_>, progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<Vec<Separated>, Error> {
    let wanted = wanted_stems(req.model, req.stems)?;
    let (left, right) = stereo(req.audio)?;
    if req.sample_rate == 0 || req.sample_rate > 384_000 {
        return Err(Error::BadInput(format!("unsupported sample rate {} Hz", req.sample_rate)));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::separate(dir, req.model, &left, &right, req.sample_rate, &wanted, progress, cancel)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (dir, left, right, wanted, progress, cancel);
        Err(Error::Unsupported)
    }
}

/// The requested stems, checked against what the model makes; empty means all of them.
fn wanted_stems(model: &Model, stems: &[Stem]) -> Result<Vec<Stem>, Error> {
    if stems.is_empty() {
        return Ok(model.stems.to_vec());
    }
    let mut out = Vec::new();
    for s in stems {
        if !model.stems.contains(s) {
            return Err(Error::BadInput(format!("the {} model has no {} stem", model.label, s.id())));
        }
        if !out.contains(s) {
            out.push(*s);
        }
    }
    Ok(out)
}

/// The two channels the model takes: mono is duplicated; NaN and infinities become silence.
fn stereo(audio: &[Vec<f32>]) -> Result<(Vec<f32>, Vec<f32>), Error> {
    let (l, r) = match audio {
        [m] => (m, m),
        [l, r] => (l, r),
        [] => return Err(Error::BadInput("no audio to separate".into())),
        _ => return Err(Error::BadInput(format!("{} channels: stem separation takes mono or stereo audio", audio.len()))),
    };
    let n = l.len().min(r.len());
    if n == 0 {
        return Err(Error::BadInput("no audio to separate".into()));
    }
    if n > MAX_FRAMES {
        return Err(Error::BadInput("stem separation takes at most 20 minutes of audio at a time".into()));
    }
    let clean = |x: &[f32]| x.iter().take(n).map(|v| if v.is_finite() { *v } else { 0.0 }).collect();
    Ok((clean(l), clean(r)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        for m in &MODELS {
            assert_eq!(m.sha256.len(), 64, "{}", m.id);
            assert!(m.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{}", m.id);
            assert!(m.url.ends_with(m.file), "{}", m.id);
            assert!(m.bytes > 1_000_000);
            assert!(model(m.id).is_some_and(|x| std::ptr::eq(x, m)));
        }
        assert_eq!(model(" HTDEMUCS ").map(|m| m.id), Some("htdemucs"));
        assert!(model("nope").is_none());
    }

    #[test]
    fn stems_parse_and_check() {
        assert_eq!(Stem::parse("Vocals"), Some(Stem::Vocals));
        assert_eq!(Stem::parse("kazoo"), None);
        let std = &MODELS[0];
        assert_eq!(wanted_stems(std, &[]).map(|v| v.len()), Ok(4));
        assert_eq!(wanted_stems(std, &[Stem::Vocals, Stem::Vocals]), Ok(vec![Stem::Vocals]));
        assert!(matches!(wanted_stems(std, &[Stem::Piano]), Err(Error::BadInput(_))));
    }

    #[test]
    fn hostile_input_is_refused_or_cleaned() {
        assert!(matches!(stereo(&[]), Err(Error::BadInput(_))));
        assert!(matches!(stereo(&[vec![], vec![]]), Err(Error::BadInput(_))));
        assert!(matches!(stereo(&[vec![0.0; 4], vec![0.0; 4], vec![0.0; 4]]), Err(Error::BadInput(_))));
        let (l, r) = stereo(&[vec![f32::NAN, 1.0, f32::INFINITY]]).unwrap();
        assert_eq!(l, vec![0.0, 1.0, 0.0]);
        assert_eq!(l, r);
        // Uneven channels are cut to the shorter one.
        let (l, r) = stereo(&[vec![0.5; 5], vec![0.25; 3]]).unwrap();
        assert_eq!((l.len(), r.len()), (3, 3));
    }

    #[test]
    fn missing_model_is_not_installed() {
        let dir = std::env::temp_dir().join("soundcraft-stems-test-empty");
        assert!(!installed(&dir, &MODELS[0]));
        let req = Request { model: &MODELS[0], audio: &[vec![0.0; 1000]], sample_rate: 48_000, stems: &[] };
        let r = separate(&dir, &req, &mut |_| {}, &AtomicBool::new(false));
        assert!(matches!(r, Err(Error::NotInstalled("htdemucs"))), "{:?}", r.err());
    }
}
