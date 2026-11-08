//! Runs the real model. Needs the weights and a GPU, so it is ignored by default:
//! `SOUNDCRAFT_STEMS_MODELS=<folder with htdemucs.safetensors> cargo test -p soundcraft-stems -- --ignored`

use soundcraft_stems::{MODELS, Request, Stem, separate};
use std::sync::atomic::{AtomicBool, Ordering};

fn models_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("SOUNDCRAFT_STEMS_MODELS").map(Into::into)
}

/// A kick-like thump every half second over a sustained chord: something for every stem.
fn test_signal(rate: u32, seconds: f32) -> Vec<Vec<f32>> {
    let n = (rate as f32 * seconds) as usize;
    let sr = rate as f32;
    let mut l = vec![0.0f32; n];
    for (i, v) in l.iter_mut().enumerate() {
        let t = i as f32 / sr;
        let beat = t % 0.5;
        let kick = (-(beat * 30.0)).exp() * (2.0 * std::f32::consts::PI * 55.0 * beat).sin();
        let chord = [220.0, 277.18, 329.63].iter().map(|f| (2.0 * std::f32::consts::PI * f * t).sin()).sum::<f32>() / 3.0;
        *v = 0.5 * kick + 0.2 * chord;
    }
    let r = l.clone();
    vec![l, r]
}

#[test]
#[ignore = "needs model weights and a GPU"]
fn separates_with_exact_length_and_sums_back() {
    let Some(dir) = models_dir() else { return };
    let rate = 48_000;
    let audio = test_signal(rate, 12.0);
    let req = Request { model: &MODELS[0], audio: &audio, sample_rate: rate, stems: &[] };
    let mut last = 0.0;
    let stems = separate(&dir, &req, &mut |p| last = p, &AtomicBool::new(false)).unwrap();
    assert_eq!(stems.len(), 4);
    assert_eq!(last, 1.0);
    for s in &stems {
        assert_eq!(s.audio[0].len(), audio[0].len(), "{:?}", s.stem);
        assert_eq!(s.audio[1].len(), audio[0].len(), "{:?}", s.stem);
    }
    // The stems should add back to roughly the input (HTDemucs does not enforce it exactly).
    let err: f64 = (0..audio[0].len())
        .map(|i| {
            let sum: f32 = stems.iter().map(|s| s.audio[0][i]).sum();
            f64::from(sum - audio[0][i]).powi(2)
        })
        .sum();
    let sig: f64 = audio[0].iter().map(|v| f64::from(*v).powi(2)).sum();
    let snr = 10.0 * (sig / err.max(1e-12)).log10();
    assert!(snr > 10.0, "stems sum back at only {snr:.1} dB");
    // Only the asked-for stems come back, in the asked-for order.
    let two = separate(&dir, &Request { stems: &[Stem::Vocals, Stem::Bass], ..req }, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(two.iter().map(|s| s.stem).collect::<Vec<_>>(), vec![Stem::Vocals, Stem::Bass]);
}

#[test]
#[ignore = "needs model weights and a GPU"]
fn cancel_stops_between_chunks() {
    let Some(dir) = models_dir() else { return };
    let audio = test_signal(44_100, 30.0);
    let cancel = AtomicBool::new(false);
    let req = Request { model: &MODELS[0], audio: &audio, sample_rate: 44_100, stems: &[] };
    let r = separate(
        &dir,
        &req,
        &mut |p| {
            if p > 0.0 {
                cancel.store(true, Ordering::Relaxed)
            }
        },
        &cancel,
    );
    assert!(matches!(r, Err(soundcraft_stems::Error::Cancelled)), "{:?}", r.err());
}
