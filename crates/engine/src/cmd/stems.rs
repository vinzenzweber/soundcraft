//! Stem separation: split selected audio clips into vocals, drums, bass and other (or six stems)
//! on new tracks in a routing folder under the source track.
//!
//! The work has three parts so the app can keep drawing while the model runs:
//! [`stems_job`] renders the clips (fast, on the caller's thread), [`StemsJob::run`] separates
//! (slow, any thread), and `stems.separate` places the result as one undo step. The app offers a
//! finished result with [`Engine::offer_stems`] and then runs `stems.separate` with the same
//! params; called without an offer (CLI, MCP, scripts) the command does all three itself.

use super::{CommandSpec, bad, bool_or, clip_ids_param, str_param};
use crate::{Engine, EngineError, Result, cmd};
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_model::{ChannelFormat, Clip, ClipContent, ClipId, TrackId, TrackKind};
use soundcraft_stems::{MODELS, Model, Stem};
use soundcraft_time::Samples;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "stems.models",
            "Stem Separation Models",
            [],
            None,
            "{} — the models stem separation can use: id, label, size, installed, stems, terms; and the models folder",
            super::always,
            models
        ),
        cmd!(
            noundo "stems.install_model",
            "Download Stem Separation Model",
            [],
            None,
            "{model?: htdemucs | htdemucs_ft | htdemucs_6s} — downloads the model into the models folder and verifies its checksum; blocks until done (the app runs it in the background)",
            super::always,
            install_model
        ),
        cmd!(
            "stems.separate",
            "Stem Separation",
            [],
            None,
            "{clips?, model?: htdemucs (default) | htdemucs_ft | htdemucs_6s, stems?: [vocals, drums, bass, other, guitar, piano] (default: all the model makes), instrumental?: false (adds the clip minus its vocals), mute_original?: true} — separates each selected audio clip onto new stereo tracks in a routing folder below its track, aligned to the clip; the model must be installed (stems.install_model)",
            has_audio_clips,
            separate
        ),
    ]
}

/// Stem separation needs at least one selected audio clip.
fn has_audio_clips(e: &Engine) -> std::result::Result<(), String> {
    let s = e.session();
    if s.edit.selected_clips.iter().any(|id| s.find_clip(*id).is_some_and(|(_, c)| c.is_audio())) {
        Ok(())
    } else {
        Err("select audio clips first".into())
    }
}

/// The folder models are downloaded into: the app's settings folder, else
/// `SOUNDCRAFT_MODELS_DIR`, else a folder in the system temp directory.
pub fn models_dir(e: &Engine) -> PathBuf {
    if let Some(d) = &e.models_dir {
        return d.clone();
    }
    match std::env::var_os("SOUNDCRAFT_MODELS_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir().join("SoundCraft Models"),
    }
}

fn model_param(id: &str, p: &Value) -> Result<&'static Model> {
    match str_param(p, "model") {
        None => MODELS.first().ok_or_else(|| bad(id, "no models")),
        Some(m) => soundcraft_stems::model(m).ok_or_else(|| {
            let ids: Vec<&str> = MODELS.iter().map(|m| m.id).collect();
            bad(id, format!("unknown model `{m}` (one of {})", ids.join(", ")))
        }),
    }
}

fn stem_error(e: soundcraft_stems::Error) -> EngineError {
    match e {
        soundcraft_stems::Error::Unsupported => EngineError::Unsupported("stem separation in this build".into()),
        soundcraft_stems::Error::NotInstalled(id) => {
            EngineError::Disabled("stems.separate".into(), format!("the `{id}` model is not installed (stems.install_model)"))
        }
        other => EngineError::Io(other.to_string()),
    }
}

fn models(e: &mut Engine, _: &Value) -> Result<Value> {
    let dir = models_dir(e);
    let list: Vec<Value> = MODELS
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "label": m.label,
                "description": m.description,
                "megabytes": m.megabytes(),
                "installed": soundcraft_stems::installed(&dir, m),
                "stems": m.stems.iter().map(|s| s.id()).collect::<Vec<_>>(),
                "terms": m.terms,
            })
        })
        .collect();
    Ok(json!({"supported": soundcraft_stems::supported(), "models_dir": dir.to_string_lossy(), "models": list}))
}

fn install_model(e: &mut Engine, p: &Value) -> Result<Value> {
    let m = model_param("stems.install_model", p)?;
    let dir = models_dir(e);
    if soundcraft_stems::installed(&dir, m) {
        return Ok(json!({"model": m.id, "installed": true, "downloaded": false}));
    }
    let path = soundcraft_stems::install(&dir, m, &mut |_, _| {}, &AtomicBool::new(false)).map_err(stem_error)?;
    Ok(json!({"model": m.id, "installed": true, "downloaded": true, "path": path.to_string_lossy()}))
}

/// What `stems.separate` does with its params, before any audio is rendered.
struct Plan {
    model: &'static Model,
    stems: Vec<Stem>,
    instrumental: bool,
    mute_original: bool,
    clips: Vec<ClipId>,
    /// Matches a job to the command run that places it.
    key: String,
}

fn plan(e: &Engine, p: &Value) -> Result<Plan> {
    const ID: &str = "stems.separate";
    let model = model_param(ID, p)?;
    let stems = match p.get("stems") {
        None | Some(Value::Null) => model.stems.to_vec(),
        Some(Value::Array(a)) => {
            let mut v = Vec::new();
            for x in a {
                let name = x.as_str().ok_or_else(|| bad(ID, "`stems` lists names like \"vocals\""))?;
                let s = Stem::parse(name).ok_or_else(|| bad(ID, format!("unknown stem `{name}`")))?;
                if !model.stems.contains(&s) {
                    return Err(bad(ID, format!("the {} model has no {} stem", model.label, s.id())));
                }
                if !v.contains(&s) {
                    v.push(s);
                }
            }
            v
        }
        Some(_) => return Err(bad(ID, "`stems` must be a list")),
    };
    let instrumental = bool_or(p, "instrumental", false);
    if stems.is_empty() && !instrumental {
        return Err(bad(ID, "choose at least one stem"));
    }
    let clips: Vec<ClipId> = clip_ids_param(e, p).into_iter().filter(|id| e.session().find_clip(*id).is_some_and(|(_, c)| c.is_audio())).collect();
    if clips.is_empty() {
        return Err(bad(ID, "select audio clips first"));
    }
    let key = json!({
        "model": model.id,
        "stems": stems.iter().map(|s| s.id()).collect::<Vec<_>>(),
        "instrumental": instrumental,
        "clips": clips.iter().map(|c| c.0).collect::<Vec<_>>(),
    })
    .to_string();
    Ok(Plan { model, stems, instrumental, mute_original: bool_or(p, "mute_original", true), clips, key })
}

/// One clip's audio as it plays, without its fades and clip gain (the stems get those back).
struct ClipAudio {
    clip: ClipId,
    name: String,
    length: Samples,
    audio: Vec<Vec<f32>>,
}

/// A separation ready to run off the UI thread: the rendered clips and what to make of them.
pub struct StemsJob {
    pub key: String,
    model: &'static Model,
    stems: Vec<Stem>,
    instrumental: bool,
    rate: u32,
    dir: PathBuf,
    clips: Vec<ClipAudio>,
}

/// What a finished job hands back to `stems.separate`.
pub struct StemsDone {
    pub key: String,
    clips: Vec<ClipStems>,
}

struct ClipStems {
    clip: ClipId,
    length: Samples,
    /// (track label, stereo audio), in placing order.
    parts: Vec<(Option<Stem>, [Vec<f32>; 2])>,
}

impl StemsJob {
    /// The model this job runs.
    pub fn model(&self) -> &'static Model {
        self.model
    }

    /// The clip names, for progress notes.
    pub fn clip_names(&self) -> Vec<String> {
        self.clips.iter().map(|c| c.name.clone()).collect()
    }

    /// Whether the model is downloaded.
    pub fn model_installed(&self) -> bool {
        soundcraft_stems::installed(&self.dir, self.model)
    }

    /// The models folder this job reads from.
    pub fn models_dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Separate every clip. `progress` gets 0..=1 over all clips; `cancel` stops between chunks.
    pub fn run(&self, progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<StemsDone> {
        let n = self.clips.len().max(1) as f32;
        let mut out = Vec::new();
        for (i, c) in self.clips.iter().enumerate() {
            let base = i as f32 / n;
            let mut stems = self.stems.clone();
            if self.instrumental && !stems.contains(&Stem::Vocals) {
                stems.push(Stem::Vocals);
            }
            let req = soundcraft_stems::Request { model: self.model, audio: &c.audio, sample_rate: self.rate, stems: &stems };
            let sep = soundcraft_stems::separate(&self.dir, &req, &mut |p| progress(base + p / n), cancel).map_err(stem_error)?;
            let mut parts: Vec<(Option<Stem>, [Vec<f32>; 2])> = Vec::new();
            if self.instrumental {
                // Everything but the vocals, exactly: the clip minus the vocal stem. The four stems
                // only add back to the clip approximately; this loses nothing else.
                let vocals = sep.iter().find(|s| s.stem == Stem::Vocals).map(|s| &s.audio);
                if let Some(v) = vocals {
                    let ch = |k: usize| c.audio.get(k).or_else(|| c.audio.first());
                    let minus = |k: usize| -> Vec<f32> {
                        let src = ch(k).map_or(&[][..], Vec::as_slice);
                        v[k].iter().enumerate().map(|(j, x)| src.get(j).copied().unwrap_or(0.0) - x).collect()
                    };
                    parts.push((None, [minus(0), minus(1)]));
                }
            }
            for s in sep {
                if self.stems.contains(&s.stem) {
                    parts.push((Some(s.stem), s.audio));
                }
            }
            // Keep the dialog's order: requested stems first in their order, then the instrumental.
            parts.sort_by_key(|(s, _)| s.map_or(usize::MAX, |s| self.stems.iter().position(|x| *x == s).unwrap_or(usize::MAX - 1)));
            out.push(ClipStems { clip: c.clip, length: c.length, parts });
        }
        progress(1.0);
        Ok(StemsDone { key: self.key.clone(), clips: out })
    }
}

/// One clip as it plays, alone and without its fades, clip gain or mute.
pub(super) fn clean_clip_audio(s: &soundcraft_model::Session, track: TrackId, id: ClipId, clip: &Clip) -> Vec<Vec<f32>> {
    let mut solo = s.clone();
    if let Some(c) = solo.find_clip_mut(id) {
        c.fade_in = Default::default();
        c.fade_out = Default::default();
        c.gain_db = 0.0;
        c.gain_env.clear();
        c.muted = false;
    }
    if let Some(pl) = solo.track_mut(track).and_then(|t| t.playlist_mut()) {
        pl.clips.retain(|c| c.id == id);
    }
    soundcraft_mix::render_clips(&solo, track, clip.range())
}

/// Render the clips `stems.separate` would separate for `p`; [`StemsJob::run`] does the rest.
pub fn stems_job(e: &Engine, p: &Value) -> Result<StemsJob> {
    const ID: &str = "stems.separate";
    let plan = plan(e, p)?;
    let s = e.session();
    let mut budget = soundcraft_mix::MAX_RENDER_SAMPLES;
    let mut clips = Vec::new();
    for id in &plan.clips {
        let Some((track, clip)) = s.find_clip(*id).map(|(t, c)| (t, c.clone())) else { continue };
        let channels = s.track(track).map_or(1, |t| t.channels()).clamp(1, 2);
        let most = crate::io::render_limit(channels).min(soundcraft_stems::MAX_FRAMES);
        crate::io::refuse_longer(s, ID, &clip.name, "separate", clip.length, most)?;
        let frames = usize::try_from(clip.length).unwrap_or(0);
        budget = budget.checked_sub(frames.saturating_mul(channels)).ok_or_else(|| bad(ID, "too much audio to separate in one command"))?;
        let mut audio = clean_clip_audio(s, track, *id, &clip);
        audio.truncate(2);
        if audio.iter().all(|c| c.iter().all(|v| *v == 0.0)) {
            return Err(bad(ID, format!("`{}` is silent or its audio is offline", clip.name)));
        }
        clips.push(ClipAudio { clip: *id, name: clip.name.clone(), length: clip.length, audio });
    }
    Ok(StemsJob {
        key: plan.key,
        model: plan.model,
        stems: plan.stems,
        instrumental: plan.instrumental,
        rate: s.sample_rate.hz(),
        dir: models_dir(e),
        clips,
    })
}

impl Engine {
    /// Hands over a separation [`StemsJob::run`] finished off the UI thread: the next
    /// `stems.separate` with the same params places it instead of separating again.
    pub fn offer_stems(&mut self, done: StemsDone) {
        self.stems_offer = Some(done);
    }

    /// Drops an offered separation that was never placed.
    pub fn drop_offered_stems(&mut self) {
        self.stems_offer = None;
    }
}

/// A color per stem, so a folder of stems reads at a glance.
fn stem_color(s: Option<Stem>) -> [u8; 3] {
    match s {
        Some(Stem::Vocals) => [220, 90, 140],
        Some(Stem::Drums) => [230, 140, 50],
        Some(Stem::Bass) => [70, 120, 220],
        Some(Stem::Other) => [80, 170, 110],
        Some(Stem::Guitar) => [210, 190, 60],
        Some(Stem::Piano) => [150, 100, 210],
        None => [120, 160, 170],
    }
}

fn separate(e: &mut Engine, p: &Value) -> Result<Value> {
    let plan = plan(e, p)?;
    let done = match e.stems_offer.take() {
        Some(d) if d.key == plan.key => d,
        _ => {
            let job = stems_job(e, p)?;
            job.run(&mut |_| {}, &AtomicBool::new(false))?
        }
    };
    place(e, &plan, done)
}

/// Put each clip's stems on new tracks in a routing folder right below the clip's track.
fn place(e: &mut Engine, plan: &Plan, done: StemsDone) -> Result<Value> {
    let mut placed = Vec::new();
    let mut skipped = Vec::new();
    for cs in done.clips {
        let Some((src_track, clip)) = e.session().find_clip(cs.clip).map(|(t, c)| (t, c.clone())) else {
            skipped.push(json!({"clip": cs.clip.0, "reason": "the clip was deleted"}));
            continue;
        };
        if clip.length != cs.length {
            skipped.push(json!({"clip": cs.clip.0, "reason": "the clip changed length while it was separated"}));
            continue;
        }
        let s = e.session_mut();
        let mut tracks: Vec<TrackId> = Vec::new();
        let mut out_tracks = Vec::new();
        for (stem, audio) in cs.parts {
            let label = stem.map_or("Instrumental", Stem::label);
            let tid = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(&format!("{label} - {}", clip.name)));
            if let Some(t) = s.track_mut(tid) {
                t.color = stem_color(stem);
            }
            let frames = audio[0].len();
            let clip_name = format!("{}-{}", clip.name, label.to_ascii_lowercase());
            let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio.into() };
            let src = crate::io::add_source(s, &clip_name, buf, None, FileFormat::Wav);
            let mut nc: Clip = clip.clone();
            nc.id = s.new_clip_id();
            nc.name = clip_name;
            nc.content = ClipContent::Audio { source: src, offset: 0 };
            nc.length = Samples::try_from(frames).unwrap_or(nc.length);
            nc.stretch = 1.0;
            nc.muted = false;
            nc.color = None;
            nc.clamp_fades();
            let new_clip = nc.id;
            crate::edit::place_clip(s, tid, nc);
            tracks.push(tid);
            out_tracks.push(json!({"track": tid, "stem": stem.map_or("instrumental", Stem::id), "clip": new_clip}));
        }
        if tracks.is_empty() {
            continue;
        }
        // New tracks go right below the source track, in order.
        let below = s.track_index(src_track).map_or(s.tracks.len(), |i| i.saturating_add(1));
        let moved: Vec<_> = tracks.iter().filter_map(|t| s.track_index(*t).map(|i| s.tracks.remove(i))).collect();
        let at = below.min(s.tracks.len());
        for (k, t) in moved.into_iter().enumerate() {
            s.tracks.insert(at.saturating_add(k).min(s.tracks.len()), t);
        }
        let parent = s.track(src_track).and_then(|t| t.folder);
        let folder = super::track::move_to_folder(
            e,
            &json!({"tracks": tracks.iter().map(|t| t.0).collect::<Vec<_>>(), "name": format!("{} Stems", clip.name)}),
        )?;
        let folder_id = folder.get("folder").and_then(Value::as_u64).map(TrackId);
        let s = e.session_mut();
        if let (Some(f), Some(parent)) = (folder_id.and_then(|f| s.track_mut(f)), parent) {
            f.folder = Some(parent);
        }
        if plan.mute_original
            && let Some(c) = s.find_clip_mut(cs.clip)
        {
            c.muted = true;
        }
        placed.push(json!({"clip": cs.clip.0, "folder": folder_id, "tracks": out_tracks}));
    }
    if placed.is_empty() {
        let why = skipped.first().and_then(|s| s.get("reason")).and_then(Value::as_str).unwrap_or("nothing to place");
        return Err(bad("stems.separate", why.to_string()));
    }
    Ok(json!({"model": plan.model.id, "separated": placed, "skipped": skipped}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::Session;

    /// A session with one stereo track holding one clip of `frames` frames of a ramp.
    fn session(frames: usize) -> (Engine, TrackId, ClipId) {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Song"));
        let l: Vec<f32> = (0..frames).map(|i| (i % 100) as f32 / 100.0 - 0.5).collect();
        let r: Vec<f32> = l.iter().map(|v| -v).collect();
        let rate = s.sample_rate.hz();
        let src = crate::io::add_source(&mut s, "song", AudioBuffer { sample_rate: rate, channels: vec![l, r] }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(&mut s, t, Clip::audio(id, "Song", src, 0, 1000, frames as i64));
        s.edit.selected_clips = vec![id];
        (Engine::new(s), t, id)
    }

    /// A finished job as the model would return it: every stem a quarter of the clip.
    fn fake_done(e: &Engine, p: &Value) -> StemsDone {
        let job = stems_job(e, p).unwrap();
        let clips = job
            .clips
            .iter()
            .map(|c| {
                let q = |k: usize| c.audio[k.min(c.audio.len() - 1)].iter().map(|v| v / 4.0).collect::<Vec<_>>();
                let mut parts: Vec<(Option<Stem>, [Vec<f32>; 2])> = job.stems.iter().map(|s| (Some(*s), [q(0), q(1)])).collect();
                if job.instrumental {
                    let three = |k: usize| c.audio[k].iter().map(|v| v * 0.75).collect::<Vec<_>>();
                    parts.push((None, [three(0), three(1)]));
                }
                ClipStems { clip: c.clip, length: c.length, parts }
            })
            .collect();
        StemsDone { key: job.key, clips }
    }

    #[test]
    fn offered_result_lands_in_a_folder_below_the_track_as_one_undo_step() {
        let (mut e, t, clip) = session(4800);
        let p = json!({});
        let done = fake_done(&e, &p);
        e.offer_stems(done);
        let undo_before = e.can_undo();
        let r = e.execute("stems.separate", &p).unwrap();
        let s = e.session();
        let names: Vec<&str> = s.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Song", "Song Stems", "Vocals - Song", "Drums - Song", "Bass - Song", "Other - Song"], "{r}");
        let folder = s.tracks[1].id;
        assert!(s.tracks[1].is_folder());
        for tr in &s.tracks[2..] {
            assert_eq!(tr.folder, Some(folder));
            let c = &tr.clips()[0];
            assert_eq!((c.start, c.length), (1000, 4800), "{}", tr.name);
        }
        // The original keeps playing nothing: it is muted, not deleted.
        assert!(s.find_clip(clip).unwrap().1.muted);
        assert_eq!(s.track(t).unwrap().clips().len(), 1);
        assert!(!undo_before);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(e.session().tracks.len(), 1);
        assert!(!e.session().find_clip(clip).unwrap().1.muted);
    }

    #[test]
    fn chosen_stems_and_instrumental_keep_their_order() {
        let (mut e, _, _) = session(2000);
        let p = json!({"stems": ["bass", "vocals"], "instrumental": true, "mute_original": false});
        let done = fake_done(&e, &p);
        e.offer_stems(done);
        e.execute("stems.separate", &p).unwrap();
        let names: Vec<String> = e.session().tracks.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, ["Song", "Song Stems", "Bass - Song", "Vocals - Song", "Instrumental - Song"]);
        assert!(!e.session().tracks[0].clips()[0].muted);
    }

    #[test]
    fn a_stale_offer_is_not_placed_for_other_params() {
        let (mut e, _, _) = session(2000);
        let done = fake_done(&e, &json!({"stems": ["vocals"]}));
        e.offer_stems(done);
        // Different stems: the offer does not match, so the command would run the model.
        let dir = std::env::temp_dir().join("soundcraft-stems-none");
        e.models_dir = Some(dir);
        let err = e.execute("stems.separate", &json!({"stems": ["drums"]})).unwrap_err().to_string();
        assert!(err.contains("not installed"), "{err}");
        assert_eq!(e.session().tracks.len(), 1);
    }

    #[test]
    fn a_clip_that_changed_length_is_skipped() {
        let (mut e, _, clip) = session(3000);
        let p = json!({});
        let done = fake_done(&e, &p);
        if let Some(c) = e.session_mut().find_clip_mut(clip) {
            c.length = 1500;
        }
        e.offer_stems(done);
        let err = e.execute("stems.separate", &p).unwrap_err().to_string();
        assert!(err.contains("changed length"), "{err}");
        assert_eq!(e.session().tracks.len(), 1);
    }

    #[test]
    fn bad_params_are_refused() {
        let (mut e, _, _) = session(1000);
        for (p, want) in [
            (json!({"model": "nope"}), "unknown model"),
            (json!({"stems": ["kazoo"]}), "unknown stem"),
            (json!({"stems": ["piano"]}), "has no piano"),
            (json!({"stems": "vocals"}), "must be a list"),
            (json!({"stems": []}), "at least one stem"),
            (json!({"clips": [999999]}), "select audio clips"),
        ] {
            let err = e.execute("stems.separate", &p).unwrap_err().to_string();
            assert!(err.contains(want), "{p}: {err}");
        }
    }

    #[test]
    fn models_lists_the_catalog() {
        let mut e = Engine::new(Session::default());
        e.models_dir = Some(std::env::temp_dir().join("soundcraft-stems-none"));
        let v = e.execute("stems.models", &json!({})).unwrap();
        assert_eq!(v["models"].as_array().map(Vec::len), Some(3));
        assert_eq!(v["models"][0]["id"], "htdemucs");
        assert_eq!(v["models"][0]["installed"], false);
    }
}
