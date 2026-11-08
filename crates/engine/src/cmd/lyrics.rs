//! Lyrics: transcribe a clip's vocal, show it in the Lyrics ruler, find and select the chorus.
//!
//! Like stem separation, the work has three parts so the app can keep drawing: [`lyrics_job`]
//! renders the clip, [`LyricsJob::run`] separates the vocal (unless the clip already is one),
//! transcribes it and finds the chorus, and `lyrics.transcribe` stores the result as one undo
//! step. The app offers a finished result with [`Engine::offer_lyrics`] first.

use super::{CommandSpec, bad, bool_or, clip_ids_param, str_param};
use crate::{Engine, EngineError, Result, cmd};
use serde_json::{Value, json};
use soundcraft_model::{ClipId, LyricLine, Lyrics};
use soundcraft_time::Samples;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

/// Chorus lines further apart than this are separate occurrences.
const OCCURRENCE_GAP_SECS: f64 = 2.0;
/// Occurrences closer than this form one section (a chorus sung twice in a row).
const SECTION_GAP_SECS: f64 = 8.0;

fn occurrences(e: &Engine) -> Vec<(Samples, Samples)> {
    e.session().lyrics.chorus_ranges((OCCURRENCE_GAP_SECS * e.session().sample_rate.as_f64()) as Samples)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "lyrics.models",
            "Lyrics Models",
            [],
            None,
            "{} — the speech models lyrics transcription can use: id, label, size, installed, terms; and the models folder",
            super::always,
            models
        ),
        cmd!(
            noundo "lyrics.install_model",
            "Download Lyrics Model",
            [],
            None,
            "{model?} — downloads the speech model and verifies every file's checksum; blocks until done (the app runs it in the background)",
            super::always,
            install_model
        ),
        cmd!(
            "lyrics.transcribe",
            "Transcribe Lyrics",
            [],
            None,
            "{clip?: id (default: the selected audio clip), separate?: bool (separate the vocal first; default true unless the clip is on a Vocals track), model?} — transcribes the clip's vocal into the Lyrics ruler and marks the chorus; needs the speech model (lyrics.install_model) and, to separate, the htdemucs stem model",
            has_audio_clip,
            transcribe
        ),
        cmd!(
            query "lyrics.inspect",
            "Lyrics",
            [],
            None,
            "{} — the lyrics: lines (start, end, seconds, text, chorus), choruses (each chorus occurrence) and sections (occurrences closer than 8 s merged)",
            super::always,
            inspect
        ),
        cmd!("lyrics.clear", "Clear Lyrics", [], None, "{} — removes the lyrics", has_lyrics, clear),
        cmd!(
            "lyrics.highlight_chorus",
            "Highlight Chorus",
            [],
            None,
            "{value?: bool} — highlights the chorus lines in the Lyrics ruler (toggles when omitted; on by default)",
            super::always,
            highlight
        ),
        cmd!(
            "lyrics.copy",
            "Copy Lyrics",
            [],
            None,
            "{start, end, to} — copies the lyric lines that start in start..end to `to` (keeping their chorus marks), e.g. after pasting the chorus as an intro",
            has_lyrics,
            copy
        ),
        cmd!(
            noundo "lyrics.select_chorus",
            "Select Chorus",
            [],
            None,
            "{index?: 0, section?: false, tracks?: [ids|names] (default: every audio track)} — selects a chorus occurrence (or a merged section) on the tracks, ready to copy",
            has_lyrics,
            select_chorus
        ),
    ]
}

fn has_audio_clip(e: &Engine) -> std::result::Result<(), String> {
    let s = e.session();
    if s.edit.selected_clips.iter().any(|id| s.find_clip(*id).is_some_and(|(_, c)| c.is_audio())) {
        Ok(())
    } else {
        Err("select an audio clip first".into())
    }
}

fn has_lyrics(e: &Engine) -> std::result::Result<(), String> {
    if e.session().lyrics.is_empty() { Err("the session has no lyrics".into()) } else { Ok(()) }
}

fn model_param(id: &str, p: &Value) -> Result<&'static soundcraft_lyrics::Model> {
    match str_param(p, "model") {
        None => soundcraft_lyrics::MODELS.first().ok_or_else(|| bad(id, "no models")),
        Some(m) => soundcraft_lyrics::model(m).ok_or_else(|| bad(id, format!("unknown model `{m}`"))),
    }
}

fn lyric_error(e: soundcraft_lyrics::Error) -> EngineError {
    match e {
        soundcraft_lyrics::Error::Unsupported => EngineError::Unsupported("lyrics transcription in this build".into()),
        soundcraft_lyrics::Error::NotInstalled(id) => {
            EngineError::Disabled("lyrics.transcribe".into(), format!("the `{id}` speech model is not installed (lyrics.install_model)"))
        }
        other => EngineError::Io(other.to_string()),
    }
}

fn models(e: &mut Engine, _: &Value) -> Result<Value> {
    let dir = super::models_dir(e);
    let list: Vec<Value> = soundcraft_lyrics::MODELS
        .iter()
        .map(|m| {
            json!({"id": m.id, "label": m.label, "description": m.description, "megabytes": m.megabytes(), "installed": soundcraft_lyrics::installed(&dir, m), "terms": m.terms})
        })
        .collect();
    Ok(json!({"supported": soundcraft_lyrics::supported(), "models_dir": dir.to_string_lossy(), "models": list}))
}

fn install_model(e: &mut Engine, p: &Value) -> Result<Value> {
    let m = model_param("lyrics.install_model", p)?;
    let dir = super::models_dir(e);
    if soundcraft_lyrics::installed(&dir, m) {
        return Ok(json!({"model": m.id, "installed": true, "downloaded": false}));
    }
    soundcraft_lyrics::install(&dir, m, &mut |_, _| {}, &AtomicBool::new(false)).map_err(lyric_error)?;
    Ok(json!({"model": m.id, "installed": true, "downloaded": true}))
}

/// A transcription ready to run off the UI thread.
pub struct LyricsJob {
    pub key: String,
    model: &'static soundcraft_lyrics::Model,
    separate: bool,
    clip: ClipId,
    name: String,
    length: Samples,
    rate: u32,
    dir: PathBuf,
    audio: Vec<Vec<f32>>,
}

/// What a finished job hands back to `lyrics.transcribe`.
pub struct LyricsDone {
    pub key: String,
    clip: ClipId,
    length: Samples,
    lines: Vec<soundcraft_lyrics::Line>,
    model: &'static str,
}

impl LyricsJob {
    pub fn clip_name(&self) -> &str {
        &self.name
    }

    /// Whether this job separates the vocal first (and so needs the stem model).
    pub fn separates(&self) -> bool {
        self.separate
    }

    pub fn model(&self) -> &'static soundcraft_lyrics::Model {
        self.model
    }

    /// Separate the vocal, transcribe it and find the chorus. `progress` gets 0..=1.
    pub fn run(&self, progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<LyricsDone> {
        let (sep_share, mono) = if self.separate {
            let stem_model = soundcraft_stems::MODELS.first().ok_or_else(|| bad("lyrics.transcribe", "no stem model"))?;
            let req =
                soundcraft_stems::Request { model: stem_model, audio: &self.audio, sample_rate: self.rate, stems: &[soundcraft_stems::Stem::Vocals] };
            let sep = soundcraft_stems::separate(&self.dir, &req, &mut |p| progress(p * 0.5), cancel).map_err(|e| match e {
                soundcraft_stems::Error::NotInstalled(id) => EngineError::Disabled(
                    "lyrics.transcribe".into(),
                    format!("separating the vocal needs the `{id}` stem model (stems.install_model)"),
                ),
                other => EngineError::Io(other.to_string()),
            })?;
            let v = sep.into_iter().next().ok_or_else(|| bad("lyrics.transcribe", "no vocal stem"))?;
            (0.5, v.audio[0].iter().zip(&v.audio[1]).map(|(l, r)| (l + r) * 0.5).collect::<Vec<f32>>())
        } else {
            let n = self.audio.iter().map(Vec::len).min().unwrap_or(0);
            let ch = self.audio.len().max(1) as f32;
            (0.0, (0..n).map(|i| self.audio.iter().map(|c| c.get(i).copied().unwrap_or(0.0)).sum::<f32>() / ch).collect())
        };
        let speech = soundcraft_dsp::offline::resample(&[mono], self.rate, soundcraft_lyrics::MODEL_RATE).into_iter().next().unwrap_or_default();
        let words = soundcraft_lyrics::transcribe(&self.dir, self.model, &speech, &mut |p| progress(sep_share + p * (0.95 - sep_share)), cancel)
            .map_err(lyric_error)?;
        let mut lines = soundcraft_lyrics::lines(&words, 0.5, 12);
        // The vocal's melody tells a chorus apart; the backing loop often repeats all song long.
        let melody = soundcraft_lyrics::chroma(&speech, soundcraft_lyrics::MODEL_RATE);
        soundcraft_lyrics::find_chorus(&mut lines, Some(&melody));
        progress(1.0);
        Ok(LyricsDone { key: self.key.clone(), clip: self.clip, length: self.length, lines, model: self.model.id })
    }
}

struct Plan {
    model: &'static soundcraft_lyrics::Model,
    separate: bool,
    clip: ClipId,
    key: String,
}

fn plan(e: &Engine, p: &Value) -> Result<Plan> {
    const ID: &str = "lyrics.transcribe";
    let model = model_param(ID, p)?;
    let s = e.session();
    let clip = match p.get("clip").and_then(Value::as_u64) {
        Some(c) => ClipId(c),
        None => clip_ids_param(e, p)
            .into_iter()
            .find(|id| s.find_clip(*id).is_some_and(|(_, c)| c.is_audio()))
            .ok_or_else(|| bad(ID, "select an audio clip first"))?,
    };
    let Some((track, c)) = s.find_clip(clip) else { return Err(bad(ID, format!("no clip {}", clip.0))) };
    if !c.is_audio() {
        return Err(bad(ID, "the clip is not audio"));
    }
    // A stem from Stem Separation is already a vocal.
    let vocal_track = s.track(track).is_some_and(|t| t.name.starts_with("Vocals - ") || t.name.eq_ignore_ascii_case("vocals"));
    let separate = bool_or(p, "separate", !vocal_track);
    let key = json!({"model": model.id, "clip": clip.0, "separate": separate}).to_string();
    Ok(Plan { model, separate, clip, key })
}

/// Render the clip `lyrics.transcribe` would transcribe for `p`; [`LyricsJob::run`] does the rest.
pub fn lyrics_job(e: &Engine, p: &Value) -> Result<LyricsJob> {
    const ID: &str = "lyrics.transcribe";
    let plan = plan(e, p)?;
    let s = e.session();
    let Some((track, clip)) = s.find_clip(plan.clip).map(|(t, c)| (t, c.clone())) else { return Err(bad(ID, "the clip is gone")) };
    let most = (soundcraft_lyrics::MAX_SAMPLES as f64 * s.sample_rate.as_f64() / f64::from(soundcraft_lyrics::MODEL_RATE)) as usize;
    crate::io::refuse_longer(s, ID, &clip.name, "transcribe", clip.length, most.min(crate::io::render_limit(2)))?;
    let mut audio = super::stems::clean_clip_audio(s, track, plan.clip, &clip);
    audio.truncate(2);
    if audio.iter().all(|c| c.iter().all(|v| *v == 0.0)) {
        return Err(bad(ID, format!("`{}` is silent or its audio is offline", clip.name)));
    }
    Ok(LyricsJob {
        key: plan.key,
        model: plan.model,
        separate: plan.separate,
        clip: plan.clip,
        name: clip.name.clone(),
        length: clip.length,
        rate: s.sample_rate.hz(),
        dir: super::models_dir(e),
        audio,
    })
}

impl Engine {
    /// Hands over a transcription [`LyricsJob::run`] finished off the UI thread.
    pub fn offer_lyrics(&mut self, done: LyricsDone) {
        self.lyrics_offer = Some(done);
    }

    pub fn drop_offered_lyrics(&mut self) {
        self.lyrics_offer = None;
    }
}

fn transcribe(e: &mut Engine, p: &Value) -> Result<Value> {
    let plan = plan(e, p)?;
    let done = match e.lyrics_offer.take() {
        Some(d) if d.key == plan.key => d,
        _ => {
            let job = lyrics_job(e, p)?;
            job.run(&mut |_| {}, &AtomicBool::new(false))?
        }
    };
    let Some((_, clip)) = e.session().find_clip(done.clip).map(|(t, c)| (t, c.clone())) else {
        return Err(bad("lyrics.transcribe", "the clip was deleted"));
    };
    if clip.length != done.length {
        return Err(bad("lyrics.transcribe", "the clip changed length while it was transcribed"));
    }
    let s = e.session_mut();
    let rate = s.sample_rate.as_f64();
    let at = |secs: f32| clip.start.saturating_add((f64::from(secs) * rate).round() as Samples);
    let lines: Vec<LyricLine> = done
        .lines
        .iter()
        .take(soundcraft_model::MAX_LYRIC_LINES)
        .map(|l| LyricLine {
            start: at(l.start),
            end: at(l.end),
            text: l.text.chars().take(soundcraft_model::MAX_LYRIC_CHARS).collect(),
            chorus: l.chorus,
        })
        .collect();
    let highlight = s.lyrics.highlight_chorus;
    s.lyrics = Lyrics { lines, highlight_chorus: highlight, clip: Some(done.clip), language: "auto".into(), model: done.model.into() };
    if !s.edit.rulers.iter().any(|r| r == "lyrics") {
        s.edit.rulers.push("lyrics".into());
        super::view_more::sort_rulers(&mut s.edit.rulers);
    }
    let v = summary(e);
    Ok(v)
}

/// Seconds for a session position.
fn secs(e: &Engine, at: Samples) -> f64 {
    (at as f64 / e.session().sample_rate.as_f64() * 1000.0).round() / 1000.0
}

/// Chorus occurrences closer than [`SECTION_GAP_SECS`] merged into sections.
fn sections(e: &Engine) -> Vec<(Samples, Samples)> {
    let gap = (SECTION_GAP_SECS * e.session().sample_rate.as_f64()) as Samples;
    let mut out: Vec<(Samples, Samples)> = Vec::new();
    for (a, b) in occurrences(e) {
        match out.last_mut() {
            Some(last) if a.saturating_sub(last.1) <= gap => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

fn ranges_json(e: &Engine, r: &[(Samples, Samples)]) -> Vec<Value> {
    r.iter().map(|(a, b)| json!({"start": a, "end": b, "start_secs": secs(e, *a), "end_secs": secs(e, *b), "length_secs": secs(e, b - a)})).collect()
}

fn summary(e: &Engine) -> Value {
    let l = &e.session().lyrics;
    json!({
        "lines": l.lines.len(),
        "words": l.lines.iter().map(|x| x.text.split_whitespace().count()).sum::<usize>(),
        "chorus_lines": l.lines.iter().filter(|x| x.chorus).count(),
        "choruses": ranges_json(e, &occurrences(e)),
        "sections": ranges_json(e, &sections(e)),
        "model": l.model,
    })
}

fn inspect(e: &mut Engine, _: &Value) -> Result<Value> {
    let l = &e.session().lyrics;
    let lines: Vec<Value> = l
        .lines
        .iter()
        .map(|x| json!({"start": x.start, "end": x.end, "start_secs": secs(e, x.start), "end_secs": secs(e, x.end), "text": x.text, "chorus": x.chorus}))
        .collect();
    let mut v = summary(e);
    v["lines"] = json!(lines);
    v["highlight_chorus"] = json!(l.highlight_chorus);
    v["clip"] = json!(l.clip.map(|c| c.0));
    Ok(v)
}

fn clear(e: &mut Engine, _: &Value) -> Result<Value> {
    let s = e.session_mut();
    let n = s.lyrics.lines.len();
    let highlight = s.lyrics.highlight_chorus;
    s.lyrics = Lyrics { highlight_chorus: highlight, ..Lyrics::default() };
    Ok(json!({"cleared": n}))
}

fn highlight(e: &mut Engine, p: &Value) -> Result<Value> {
    let s = e.session_mut();
    let v = p.get("value").and_then(Value::as_bool).unwrap_or(!s.lyrics.highlight_chorus);
    s.lyrics.highlight_chorus = v;
    Ok(json!({"value": v}))
}

fn copy(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "lyrics.copy";
    let pos = |k: &str| p.get(k).and_then(Value::as_i64).ok_or_else(|| bad(ID, format!("`{k}` (samples) required")));
    let (a, b, to) = (pos("start")?, pos("end")?, pos("to")?);
    if b <= a || to < 0 {
        return Err(bad(ID, "need start < end and to >= 0"));
    }
    let s = e.session_mut();
    let by = to.saturating_sub(a);
    let copies: Vec<LyricLine> = s
        .lyrics
        .lines
        .iter()
        .filter(|l| l.start >= a && l.start < b)
        .map(|l| LyricLine { start: l.start.saturating_add(by), end: l.end.min(b).saturating_add(by), text: l.text.clone(), chorus: l.chorus })
        .collect();
    if copies.is_empty() {
        return Err(bad(ID, "no lyric lines start in that range"));
    }
    if s.lyrics.lines.len().saturating_add(copies.len()) > soundcraft_model::MAX_LYRIC_LINES {
        return Err(bad(ID, "too many lyric lines"));
    }
    let n = copies.len();
    s.lyrics.lines.extend(copies);
    s.lyrics.lines.sort_by_key(|l| l.start);
    Ok(json!({"copied": n}))
}

fn select_chorus(e: &mut Engine, p: &Value) -> Result<Value> {
    const ID: &str = "lyrics.select_chorus";
    let list = if bool_or(p, "section", false) { sections(e) } else { occurrences(e) };
    if list.is_empty() {
        return Err(bad(ID, "no chorus was found in the lyrics"));
    }
    let i = p.get("index").and_then(Value::as_u64).unwrap_or(0);
    let (a, b) = usize::try_from(i)
        .ok()
        .and_then(|i| list.get(i).copied())
        .ok_or_else(|| bad(ID, format!("there are {} choruses (index 0..{})", list.len(), list.len() - 1)))?;
    let tracks: Vec<Value> = if p.get("tracks").is_some() {
        p.get("tracks").and_then(Value::as_array).cloned().unwrap_or_default()
    } else {
        e.session().tracks.iter().filter(|t| t.kind == soundcraft_model::TrackKind::Audio).map(|t| json!(t.id.0)).collect()
    };
    e.execute("edit.select", &json!({"tracks": tracks, "start": a, "end": b}))?;
    Ok(json!({"index": i, "count": list.len(), "start": a, "end": b, "start_secs": secs(e, a), "end_secs": secs(e, b), "tracks": tracks.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};
    use soundcraft_model::{ChannelFormat, Clip, Session, TrackKind};

    fn session() -> (Engine, ClipId) {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Song"));
        let rate = s.sample_rate.hz();
        let frames = rate as usize * 60;
        let src =
            crate::io::add_source(&mut s, "song", AudioBuffer { sample_rate: rate, channels: vec![vec![0.1; frames]; 2] }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(&mut s, t, Clip::audio(id, "Song", src, 0, rate as i64 * 2, frames as i64));
        s.edit.selected_clips = vec![id];
        (Engine::new(s), id)
    }

    fn line(start: f32, text: &str, chorus: bool) -> soundcraft_lyrics::Line {
        soundcraft_lyrics::Line { start, end: start + 3.0, text: text.into(), chorus }
    }

    fn offer(e: &mut Engine, p: &Value) {
        let plan = plan(e, p).unwrap();
        let length = e.session().find_clip(plan.clip).unwrap().1.length;
        e.offer_lyrics(LyricsDone {
            key: plan.key,
            clip: plan.clip,
            length,
            model: "parakeet-tdt-0.6b-v3",
            lines: vec![
                line(1.0, "uno", false),
                line(10.0, "dame", true),
                line(13.0, "baila", true),
                line(30.0, "dame", true),
                line(45.0, "fin", false),
            ],
        });
    }

    #[test]
    fn offered_lyrics_land_on_the_timeline_with_the_ruler_shown() {
        let (mut e, _) = session();
        let p = json!({});
        offer(&mut e, &p);
        let r = e.execute("lyrics.transcribe", &p).unwrap();
        assert_eq!(r["lines"], 5);
        assert_eq!(r["choruses"].as_array().map(Vec::len), Some(2));
        // 10 s into a clip that starts at 2 s.
        let rate = e.session().sample_rate.hz() as i64;
        assert_eq!(e.session().lyrics.lines[1].start, 12 * rate);
        assert!(e.session().edit.rulers.iter().any(|r| r == "lyrics"));
        assert!(e.session().lyrics.highlight_chorus);
        // 10-16 s and 30-33 s are 14 s apart: two sections.
        let v = e.execute("lyrics.inspect", &json!({})).unwrap();
        assert_eq!(v["sections"].as_array().map(Vec::len), Some(2));
        assert_eq!(v["lines"][1]["chorus"], true);
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(e.session().lyrics.is_empty());
    }

    #[test]
    fn select_chorus_selects_every_audio_track() {
        let (mut e, _) = session();
        offer(&mut e, &json!({}));
        e.execute("lyrics.transcribe", &json!({})).unwrap();
        let v = e.execute("lyrics.select_chorus", &json!({"index": 1})).unwrap();
        let rate = e.session().sample_rate.hz() as i64;
        assert_eq!((v["start"].as_i64(), v["end"].as_i64()), (Some(32 * rate), Some(35 * rate)));
        let ed = &e.session().edit;
        assert_eq!((ed.selection.start, ed.selection.end), (32 * rate, 35 * rate));
        assert_eq!(ed.selected_tracks.len(), 1);
        assert!(e.execute("lyrics.select_chorus", &json!({"index": 9})).unwrap_err().to_string().contains("there are 2"));
    }

    #[test]
    fn inserting_time_moves_the_lyrics() {
        let (mut e, _) = session();
        offer(&mut e, &json!({}));
        e.execute("lyrics.transcribe", &json!({})).unwrap();
        let before = e.session().lyrics.lines[0].start;
        let rate = e.session().sample_rate.hz() as i64;
        e.execute("event.insert_time", &json!({"start": 0, "length": 10 * rate})).unwrap();
        assert_eq!(e.session().lyrics.lines[0].start, before + 10 * rate);
    }

    #[test]
    fn copy_puts_the_chorus_lines_at_the_intro() {
        let (mut e, _) = session();
        offer(&mut e, &json!({}));
        e.execute("lyrics.transcribe", &json!({})).unwrap();
        let rate = e.session().sample_rate.hz() as i64;
        // The chorus lines at 12-18 s (clip starts at 2 s) to 0 s.
        let v = e.execute("lyrics.copy", &json!({"start": 12 * rate, "end": 18 * rate, "to": 0})).unwrap();
        assert_eq!(v["copied"], 2);
        let l = &e.session().lyrics.lines;
        assert_eq!((l[0].start, l[0].chorus, l[1].start), (0, true, 3 * rate));
        assert!(e.execute("lyrics.copy", &json!({"start": 5, "end": 1, "to": 0})).is_err());
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(e.session().lyrics.lines.len(), 5);
    }

    #[test]
    fn highlight_toggles_and_clear_keeps_it() {
        let (mut e, _) = session();
        offer(&mut e, &json!({}));
        e.execute("lyrics.transcribe", &json!({})).unwrap();
        assert_eq!(e.execute("lyrics.highlight_chorus", &json!({})).unwrap()["value"], false);
        e.execute("lyrics.clear", &json!({})).unwrap();
        assert!(e.session().lyrics.is_empty());
        assert!(!e.session().lyrics.highlight_chorus);
    }

    #[test]
    fn a_vocals_track_is_not_separated_again() {
        let (mut e, clip) = session();
        e.session_mut().tracks[0].name = "Vocals - Song".into();
        assert!(!plan(&e, &json!({})).unwrap().separate);
        assert!(plan(&e, &json!({"separate": true})).unwrap().separate);
        assert!(plan(&e, &json!({"clip": clip.0 + 99})).is_err());
    }
}
