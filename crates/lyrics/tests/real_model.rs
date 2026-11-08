//! Runs the real speech model. Needs the weights and a GPU, so it is ignored by default:
//! `SOUNDCRAFT_LYRICS_MODELS=<models folder> SOUNDCRAFT_LYRICS_VOCALS=<16 kHz mono vocal wav>
//! SOUNDCRAFT_LYRICS_MIX=<16 kHz mono mix wav> cargo test -p soundcraft-lyrics -- --ignored`

use soundcraft_lyrics::{MODELS, chroma, find_chorus, lines, transcribe};
use std::sync::atomic::AtomicBool;

fn read(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).unwrap();
    r.samples::<i16>().map(|s| f32::from(s.unwrap()) / 32768.0).collect()
}

#[test]
#[ignore = "needs model weights, a GPU and a song"]
fn transcribes_a_song_and_finds_its_chorus() {
    let (Ok(dir), Ok(vocals), Ok(mix)) =
        (std::env::var("SOUNDCRAFT_LYRICS_MODELS"), std::env::var("SOUNDCRAFT_LYRICS_VOCALS"), std::env::var("SOUNDCRAFT_LYRICS_MIX"))
    else {
        return;
    };
    let v = read(&vocals);
    let t0 = std::time::Instant::now();
    let mut last = 0.0;
    let words = transcribe(std::path::Path::new(&dir), &MODELS[0], &v, &mut |p| last = p, &AtomicBool::new(false)).unwrap();
    let secs = t0.elapsed().as_secs_f32();
    assert_eq!(last, 1.0);
    assert!(words.len() > 100, "{} words", words.len());
    assert!(words.windows(2).all(|w| w[0].start <= w[1].start + 0.01));
    let mut ls = lines(&words, 0.5, 12);
    let m = chroma(&read(&mix), 16_000);
    let ranges = find_chorus(&mut ls, Some(&m));
    eprintln!("{} words, {} lines in {secs:.1} s; chorus ranges {ranges:?}", words.len(), ls.len());
    assert!(ranges.len() >= 2);
}
