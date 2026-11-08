#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// Proving the GPU model `Send` (for the shared slot) walks wgpu's deeply nested types.
#![recursion_limit = "256"]
//! SoundCraft lyrics.
//!
//! Transcribes a sung vocal with NVIDIA's Parakeet TDT 0.6B v3 (25 European languages, language
//! detected per utterance) running locally on the GPU through Burn, groups the words into lines,
//! and finds the chorus: the block of lines that repeats most, extended to every place where the
//! music of that block comes back.
//!
//! The model is not bundled: [`install`] downloads it into a folder the caller names and checks
//! every file's size and SHA-256 before it is used.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

mod chorus;
#[cfg(not(target_arch = "wasm32"))]
mod native;

pub use chorus::{Chroma, chroma, find_chorus, lines};

/// One file of a model.
#[derive(Debug)]
pub struct ModelFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// A speech model SoundCraft can download and run.
#[derive(Debug)]
pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Download folder; each file is `base_url` + name.
    pub base_url: &'static str,
    pub files: &'static [ModelFile],
    /// Where the weights come from and under which terms, shown before a download.
    pub terms: &'static str,
}

impl Model {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }

    pub fn megabytes(&self) -> u64 {
        self.bytes().div_ceil(1_000_000)
    }

    /// The folder holding this model's files inside `dir`.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(self.id)
    }
}

pub static MODELS: [Model; 1] = [Model {
    id: "parakeet-tdt-0.6b-v3",
    label: "Parakeet v3",
    description: "25 languages including Spanish and English, detected automatically",
    base_url: "https://huggingface.co/altunenes/parakeet-rs/resolve/main/parakeet-tdt-0.6b-v3/",
    files: &[
        ModelFile { name: "encoder-model.onnx", bytes: 3_437_572, sha256: "8f72233edd8175da74950bbcec59f4fe8405455f6e038cdb2e530c54ab0d46ed" },
        ModelFile {
            name: "encoder-model.onnx.data",
            bytes: 2_476_376_064,
            sha256: "40c1f3a0455a376fe906eb6caab976231bf0420df1e4e70929b11fba1410ccbc",
        },
        ModelFile { name: "decoder_joint-model.onnx", bytes: 72_584_074, sha256: "9ce45c12b84c88366e259d51701eb6061bb40a8f3caee486fb7c3fe52ac131ac" },
        ModelFile { name: "vocab.txt", bytes: 93_939, sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d" },
    ],
    terms: "NVIDIA Parakeet TDT 0.6B v3, licensed CC-BY-4.0 (attribution: NVIDIA), in the ONNX export published for parakeet-rs.",
}];

/// The model with `id` (case-insensitive).
pub fn model(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id.eq_ignore_ascii_case(id.trim()))
}

/// The rate the model takes; the caller resamples to it.
pub const MODEL_RATE: u32 = 16_000;

/// The longest audio one call transcribes: 30 minutes at 16 kHz.
pub const MAX_SAMPLES: usize = 30 * 60 * MODEL_RATE as usize;

/// Why a transcription or download did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Unsupported,
    NotInstalled(&'static str),
    Cancelled,
    BadInput(String),
    Download(String),
    Model(String),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported => write!(f, "lyrics transcription is not available in this build"),
            Error::NotInstalled(id) => write!(f, "the `{id}` speech model is not installed (lyrics.install_model)"),
            Error::Cancelled => write!(f, "lyrics transcription was cancelled"),
            Error::BadInput(m) => write!(f, "{m}"),
            Error::Download(m) => write!(f, "model download failed: {m}"),
            Error::Model(m) => write!(f, "transcription failed: {m}"),
            Error::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

/// Whether every file of `model` is in `dir` with its catalog size.
pub fn installed(dir: &Path, model: &Model) -> bool {
    let d = model.path(dir);
    model.files.iter().all(|f| std::fs::metadata(d.join(f.name)).is_ok_and(|m| m.is_file() && m.len() == f.bytes))
}

pub fn supported() -> bool {
    cfg!(not(target_arch = "wasm32"))
}

/// Download `model` into `dir`, verifying each file. `progress(done, total)` over all files.
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

/// A transcribed word, in seconds from the start of the audio.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub start: f32,
    pub end: f32,
    pub text: String,
}

/// A line of lyrics, in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub start: f32,
    pub end: f32,
    pub text: String,
    pub chorus: bool,
}

/// Transcribe 16 kHz mono `audio` with the installed model in `dir`. `progress` gets 0..=1;
/// `cancel` stops between chunks. A panic in the model is caught and returned as an error.
pub fn transcribe(dir: &Path, model: &'static Model, audio: &[f32], progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<Vec<Word>, Error> {
    if audio.is_empty() {
        return Err(Error::BadInput("no audio to transcribe".into()));
    }
    if audio.len() > MAX_SAMPLES {
        return Err(Error::BadInput("lyrics transcription takes at most 30 minutes of audio at a time".into()));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::transcribe(dir, model, audio, progress, cancel)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (dir, model, progress, cancel);
        Err(Error::Unsupported)
    }
}

/// Where to cut long audio into chunks: near every `chunk` samples, at the quietest 100 ms
/// within the last `search` samples before that point, so a cut rarely splits a word.
pub fn chunk_bounds(audio: &[f32], chunk: usize, search: usize) -> Vec<(usize, usize)> {
    let n = audio.len();
    let chunk = chunk.max(1);
    let win = (MODEL_RATE as usize / 10).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while n.saturating_sub(start) > chunk {
        let target = start.saturating_add(chunk);
        let from = target.saturating_sub(search).max(start.saturating_add(win));
        let mut best = (f32::MAX, target);
        let mut at = from;
        while at.saturating_add(win) <= target {
            let e: f32 = audio.get(at..at.saturating_add(win)).map_or(f32::MAX, |w| w.iter().map(|v| v * v).sum());
            if e < best.0 {
                best = (e, at.saturating_add(win / 2));
            }
            at = at.saturating_add(win / 2);
        }
        out.push((start, best.1));
        start = best.1;
    }
    out.push((start, n));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        for m in &MODELS {
            for f in m.files {
                assert_eq!(f.sha256.len(), 64);
                assert!(f.bytes > 0);
            }
            assert!(model(m.id).is_some());
            assert!(m.megabytes() > 2_000);
        }
    }

    #[test]
    fn chunks_cover_everything_and_cut_in_silence() {
        let rate = MODEL_RATE as usize;
        // 10 s of tone with a silent gap from 4.0 to 4.3 s.
        let mut a: Vec<f32> = (0..10 * rate).map(|i| (i as f32 * 0.05).sin()).collect();
        for v in &mut a[4 * rate..4 * rate + rate * 3 / 10] {
            *v = 0.0;
        }
        let b = chunk_bounds(&a, 5 * rate, 2 * rate);
        assert_eq!(b.first().map(|x| x.0), Some(0));
        assert_eq!(b.last().map(|x| x.1), Some(a.len()));
        for w in b.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        let cut = b[0].1 as f32 / rate as f32;
        assert!((4.0..4.3).contains(&cut), "cut at {cut}");
        assert_eq!(chunk_bounds(&a[..100], 5 * rate, rate), vec![(0, 100)]);
    }

    #[test]
    fn missing_model_is_not_installed() {
        let dir = std::env::temp_dir().join("soundcraft-lyrics-test-empty");
        assert!(!installed(&dir, &MODELS[0]));
        let r = transcribe(&dir, &MODELS[0], &[0.0; 1600], &mut |_| {}, &AtomicBool::new(false));
        assert!(matches!(r, Err(Error::NotInstalled(_))), "{r:?}");
        assert!(matches!(transcribe(&dir, &MODELS[0], &[], &mut |_| {}, &AtomicBool::new(false)), Err(Error::BadInput(_))));
    }
}
