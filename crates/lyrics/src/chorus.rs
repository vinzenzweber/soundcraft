//! Lines from words, and the chorus from repeated lines plus repeated music.

use crate::{Line, Word};

/// Group words into lines: a new line after a pause longer than `max_gap` seconds or after
/// `max_words` words. Sung lyrics have no punctuation to split on.
pub fn lines(words: &[Word], max_gap: f32, max_words: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut count = 0;
    // A sentence ended: the next word starts a new line.
    let mut closed = false;
    for w in words {
        let text = w.text.trim();
        if text.is_empty() {
            continue;
        }
        // The model writes punctuation as its own token: it belongs to the word before.
        if text.chars().all(|c| !c.is_alphanumeric()) {
            if let Some(l) = out.last_mut().filter(|_| !closed) {
                l.text.push_str(text);
                l.end = l.end.max(w.end);
                closed = text.contains(['.', '?', '!']);
            }
            continue;
        }
        let join = !closed && out.last().is_some_and(|l| w.start - l.end <= max_gap && count < max_words.max(1));
        closed = false;
        match out.last_mut() {
            Some(l) if join => {
                l.text.push(' ');
                l.text.push_str(text);
                l.end = l.end.max(w.end);
                count += 1;
            }
            _ => {
                out.push(Line { start: w.start, end: w.end.max(w.start), text: text.to_string(), chorus: false });
                count = 1;
            }
        }
    }
    out
}

/// Lowercase words without accents, punctuation or ad-libs in parentheses, for comparing lines.
fn words_of(text: &str) -> Vec<String> {
    let mut depth = 0u32;
    let mut clean = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '(' | '[' => depth = depth.saturating_add(1),
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            c if c.is_alphanumeric() || c == '\'' => clean.extend(fold(c).to_lowercase()),
            _ => clean.push(' '),
        }
    }
    clean.split_whitespace().map(str::to_string).collect()
}

/// Strip the accents Spanish and English lyrics use (ñ stays).
fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'â' | 'ä' | 'Á' | 'À' | 'Â' | 'Ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'Ó' | 'Ò' | 'Ô' | 'Ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
        c => c,
    }
}

/// 2·LCS / (|a| + |b|) over words: 1 for the same line, 0 for nothing in common.
fn similarity(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // Lines are short (a dozen words), so the quadratic table is tiny.
    let mut prev = vec![0usize; b.len() + 1];
    for x in a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            let up = prev.get(j + 1).copied().unwrap_or(0);
            let left = cur.get(j).copied().unwrap_or(0);
            let diag = prev.get(j).copied().unwrap_or(0);
            if let Some(c) = cur.get_mut(j + 1) {
                *c = if x == y { diag + 1 } else { up.max(left) };
            }
        }
        prev = cur;
    }
    let lcs = prev.last().copied().unwrap_or(0);
    2.0 * lcs as f32 / (a.len() + b.len()) as f32
}

/// Chroma (12 pitch classes) of mono audio, about 8 frames a second: what a passage sounds like
/// harmonically, so a repeated chorus matches itself even with different words on top.
pub struct Chroma {
    pub fps: f32,
    pub frames: Vec<[f32; 12]>,
}

pub fn chroma(audio: &[f32], rate: u32) -> Chroma {
    const N: usize = 4096;
    let rate_f = rate.max(1) as f32;
    let hop = (rate_f / 8.0).round().max(1.0) as usize;
    let mut planner = rustfft::FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(N);
    let window: Vec<f32> = (0..N).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N as f32).cos()).collect();
    // Pitch class of every bin between 60 Hz and 4 kHz.
    let bins: Vec<(usize, usize)> = (1..N / 2)
        .filter_map(|k| {
            let f = k as f32 * rate_f / N as f32;
            if !(60.0..4000.0).contains(&f) {
                return None;
            }
            let pc = (12.0 * (f / 440.0).log2()).round().rem_euclid(12.0) as usize;
            Some((k, pc.min(11)))
        })
        .collect();
    let mut frames = Vec::new();
    let mut buf = vec![rustfft::num_complex::Complex32::new(0.0, 0.0); N];
    let mut at: usize = 0;
    while at.saturating_add(N) <= audio.len() {
        for (i, b) in buf.iter_mut().enumerate() {
            let s = audio.get(at + i).copied().unwrap_or(0.0);
            *b = rustfft::num_complex::Complex32::new(if s.is_finite() { s } else { 0.0 } * window.get(i).copied().unwrap_or(0.0), 0.0);
        }
        fft.process(&mut buf);
        let mut c = [0f32; 12];
        for &(k, pc) in &bins {
            if let (Some(v), Some(slot)) = (buf.get(k), c.get_mut(pc)) {
                *slot += v.norm();
            }
        }
        let norm = c.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            c.iter_mut().for_each(|v| *v /= norm);
        }
        frames.push(c);
        at += hop;
    }
    Chroma { fps: rate_f / hop as f32, frames }
}

impl Chroma {
    fn frame(&self, secs: f32) -> usize {
        (secs.max(0.0) * self.fps) as usize
    }

    /// Mean cosine similarity of the passage at `a` with the one at `b`, `len` frames long.
    fn similar(&self, a: usize, b: usize, len: usize) -> f32 {
        let mut sum = 0.0;
        let mut n = 0;
        for i in 0..len {
            let (Some(x), Some(y)) = (self.frames.get(a + i), self.frames.get(b + i)) else { break };
            sum += x.iter().zip(y).map(|(p, q)| p * q).sum::<f32>();
            n += 1;
        }
        if n < len / 2 { 0.0 } else { sum / n.max(1) as f32 }
    }
}

/// Mark the chorus lines. The chorus is the block of consecutive lines whose words repeat most
/// (2 to 8 lines, at least twice). With `music`, every other place where the music of that block
/// returns and someone sings is added too: transcription differs between repeats, the music much
/// less. Returns the chorus time ranges in seconds.
pub fn find_chorus(lines: &mut [Line], music: Option<&Chroma>) -> Vec<(f32, f32)> {
    lines.iter_mut().for_each(|l| l.chorus = false);
    let words: Vec<Vec<String>> = lines.iter().map(|l| words_of(&l.text)).collect();
    // Cluster lines that say (nearly) the same thing.
    let mut reps: Vec<usize> = Vec::new();
    let mut cid = Vec::with_capacity(lines.len());
    for (i, w) in words.iter().enumerate() {
        let best = reps.iter().enumerate().map(|(k, r)| (k, words.get(*r).map_or(0.0, |x| similarity(w, x)))).max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((k, s)) if s >= 0.6 => cid.push(k),
            _ => {
                cid.push(reps.len());
                reps.push(i);
            }
        }
    }
    let count = |c: usize| cid.iter().filter(|x| **x == c).count();
    let mut best: Option<(f32, usize, Vec<usize>)> = None;
    for n in 2..=8usize {
        for i in 0..cid.len().saturating_sub(n - 1) {
            let Some(pat) = cid.get(i..i + n) else { continue };
            if pat.iter().all(|c| *c == pat[0]) || pat.iter().any(|c| count(*c) < 2) {
                continue;
            }
            let mut occ = Vec::new();
            let mut j = 0;
            while j + n <= cid.len() {
                if cid.get(j..j + n) == Some(pat) {
                    occ.push(j);
                    j += n;
                } else {
                    j += 1;
                }
            }
            if occ.len() < 2 {
                continue;
            }
            let dur = occ.iter().filter_map(|o| Some(lines.get(o + n - 1)?.end - lines.get(*o)?.start)).sum::<f32>() / occ.len() as f32;
            let score = n as f32 * occ.len() as f32 * dur.min(40.0);
            if best.as_ref().is_none_or(|b| score > b.0) {
                best = Some((score, n, occ));
            }
        }
    }
    let Some((_, n, occ)) = best else { return Vec::new() };
    let mut ranges: Vec<(f32, f32)> = occ.iter().filter_map(|o| Some((lines.get(*o)?.start, lines.get(o + n - 1)?.end))).collect();
    if let (Some(m), Some(&(t0, t1))) = (music, ranges.first()) {
        let len = m.frame(t1 - t0).max(1);
        let a = m.frame(t0);
        // How alike the repeats the lyrics found are: other passages must match at least as well.
        let known = ranges.iter().skip(1).map(|r| m.similar(a, m.frame(r.0), len)).fold(f32::MAX, f32::min);
        let threshold = if known == f32::MAX { 0.95 } else { (known - 0.02).clamp(0.85, 0.97) };
        let step = (m.fps / 4.0).max(1.0) as usize;
        let mut found: Vec<(usize, f32)> = Vec::new();
        let mut b = 0;
        while b + len <= m.frames.len() {
            let s = m.similar(a, b, len);
            if s >= threshold {
                found.push((b, s));
            }
            b += step;
        }
        // Best match per neighbourhood, away from every range already known.
        found.sort_by(|x, y| y.1.total_cmp(&x.1));
        let dur = t1 - t0;
        for (b, _) in found {
            let start = b as f32 / m.fps;
            if ranges.iter().any(|r| (start - r.0).abs() < dur * 0.8) {
                continue;
            }
            // Only where someone sings.
            if lines.iter().any(|l| l.start < start + dur && l.end > start) {
                ranges.push((start, start + dur));
            }
        }
        ranges.sort_by(|x, y| x.0.total_cmp(&y.0));
    }
    for l in lines.iter_mut() {
        let mid = (l.start + l.end) / 2.0;
        l.chorus = ranges.iter().any(|r| mid >= r.0 && mid <= r.1);
    }
    // Report each range spanning the chorus lines inside it.
    ranges
        .iter()
        .filter_map(|r| {
            let inside: Vec<&Line> = lines.iter().filter(|l| l.chorus && (l.start + l.end) / 2.0 >= r.0 && (l.start + l.end) / 2.0 <= r.1).collect();
            Some((inside.first()?.start, inside.last()?.end))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(start: f32, text: &str) -> Word {
        Word { start, end: start + 0.3, text: text.into() }
    }

    fn line(start: f32, text: &str) -> Line {
        Line { start, end: start + 3.0, text: text.into(), chorus: false }
    }

    #[test]
    fn words_split_into_lines_at_pauses() {
        let ws = [w(0.0, "uno"), w(0.4, "dos"), w(2.0, "tres"), w(2.3, "cuatro")];
        let l = lines(&ws, 0.5, 12);
        assert_eq!(l.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), ["uno dos", "tres cuatro"]);
        assert_eq!((l[0].start, l[1].end), (0.0, 2.6));
        assert_eq!(lines(&ws, 10.0, 3).len(), 2);
        assert!(lines(&[], 0.5, 12).is_empty());
    }

    #[test]
    fn punctuation_joins_its_word_and_ends_sentences() {
        let ws = [w(0.0, "hola"), w(0.3, ","), w(0.4, "mami"), w(0.7, "."), w(0.8, "otra"), w(1.0, "vez"), w(1.1, "?")];
        let l = lines(&ws, 0.5, 12);
        assert_eq!(l.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(), ["hola, mami.", "otra vez?"]);
        // Punctuation before any word is dropped.
        assert_eq!(lines(&[w(0.0, "."), w(0.2, "uno")], 0.5, 12)[0].text, "uno");
    }

    #[test]
    fn comparing_ignores_case_accents_and_adlibs() {
        assert_eq!(words_of("Mamá, ¡DALE! (yeah) [eh]"), ["mama", "dale"]);
        assert_eq!(similarity(&words_of("la noche es nuestra"), &words_of("La noche es nuestra!")), 1.0);
        assert!(similarity(&words_of("la noche es nuestra"), &words_of("otra cosa diferente")) < 0.3);
    }

    #[test]
    fn the_most_repeated_block_is_the_chorus() {
        let mut ls = vec![
            line(0.0, "verse one starts here"),
            line(3.0, "something else entirely"),
            line(10.0, "dame tu cosita"),
            line(13.0, "baila conmigo hasta el final"),
            line(20.0, "second verse goes on"),
            line(23.0, "more words in the verse"),
            line(30.0, "dame tu cosita"),
            line(33.0, "baila conmigo hasta el fin"),
            line(40.0, "outro words"),
        ];
        let r = find_chorus(&mut ls, None);
        assert_eq!(r, vec![(10.0, 16.0), (30.0, 36.0)]);
        let marked: Vec<bool> = ls.iter().map(|l| l.chorus).collect();
        assert_eq!(marked, [false, false, true, true, false, false, true, true, false]);
    }

    #[test]
    fn no_repetition_means_no_chorus() {
        let mut ls = vec![line(0.0, "a b c"), line(3.0, "d e f"), line(6.0, "g h i")];
        assert!(find_chorus(&mut ls, None).is_empty());
        assert!(ls.iter().all(|l| !l.chorus));
        assert!(find_chorus(&mut [], None).is_empty());
    }

    #[test]
    fn music_finds_a_repeat_the_words_missed() {
        // Chorus music (an A minor chord) at 10 s, 30 s and 50 s; other music elsewhere.
        let rate = 16_000u32;
        let total = 60 * rate as usize;
        let audio: Vec<f32> = (0..total)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let chorus = [10.0f32, 30.0, 50.0].iter().any(|c| t >= *c && t < c + 6.0);
                let f: &[f32] = if chorus { &[220.0, 261.6, 329.6] } else { &[293.7, 370.0, 440.0] };
                f.iter().map(|f| (2.0 * std::f32::consts::PI * f * t).sin()).sum::<f32>() / 3.0
            })
            .collect();
        let m = chroma(&audio, rate);
        let mut ls = vec![
            line(0.0, "intro talking"),
            line(10.0, "dame tu cosita"),
            line(13.0, "baila conmigo"),
            line(20.0, "first verse about the night"),
            line(30.0, "dame tu cosita"),
            line(33.0, "baila conmigo"),
            line(40.0, "nothing here repeats at all"),
            // The third chorus was transcribed differently.
            line(50.0, "llame a tu casita"),
            line(53.0, "valla con amigo"),
        ];
        let r = find_chorus(&mut ls, Some(&m));
        assert_eq!(r.len(), 3, "{r:?}");
        assert!(ls[7].chorus && ls[8].chorus);
        assert!(!ls[6].chorus && !ls[0].chorus);
    }
}
