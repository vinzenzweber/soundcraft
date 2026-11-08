//! Stem separation in the app: the Stem Separation window, background jobs with progress and
//! Cancel, and the job commands the control channel uses (`stems.start`, `jobs.list`,
//! `jobs.status`, `jobs.cancel`).
//!
//! A job renders its clips when it starts, runs the model on a worker thread, and hands the
//! result to the engine (`Engine::offer_stems`), then runs `stems.separate` with the same params so
//! the stems land as one undo step, exactly as a script calling the command would get them. Jobs
//! run one at a time (they share the GPU); later ones wait. A separation whose model is missing
//! downloads it first when asked to (`download: true`, which the window always sends).

use crate::SoundApp;
use crate::theme::{Tokens, bold};
use serde_json::{Value, json};
use soundcraft_stems::{MODELS, Stem};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Finished jobs kept for `jobs.status` after they end.
const KEEP_FINISHED: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }

    fn finished(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
}

enum Kind {
    Install {
        model: &'static str,
    },
    /// `stems.separate` params; `clips` is filled in when the job is created.
    Separate {
        params: Value,
    },
    InstallSpeech {
        model: &'static str,
    },
    /// `lyrics.transcribe` params; `clip` is filled in when the job is created.
    Lyrics {
        params: Value,
    },
}

/// What a worker thread sends back.
enum Outcome {
    Installed,
    #[cfg(not(target_arch = "wasm32"))]
    Separated(soundcraft_engine::cmd::StemsDone),
    #[cfg(not(target_arch = "wasm32"))]
    Transcribed(soundcraft_engine::cmd::LyricsDone),
    Cancelled,
    Failed(String),
}

struct Job {
    id: u64,
    kind: Kind,
    title: String,
    state: State,
    /// 0..=1 as `f32` bits, written by the worker.
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    rx: Option<std::sync::mpsc::Receiver<Outcome>>,
    /// The document the job was made for; a result for another one is dropped.
    epoch: u64,
    /// Jobs that wait for this one (a separation waiting for its model download).
    blocks: Vec<u64>,
    waits_for: Vec<u64>,
    result: Option<Value>,
    error: Option<String>,
    started: Option<web_time::Instant>,
    /// When progress first moved: the estimate leaves out loading the model.
    moving: Option<web_time::Instant>,
    seconds: Option<f32>,
}

impl Job {
    fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    fn status(&self) -> Value {
        let (kind, model) = match &self.kind {
            Kind::Install { model } => ("install_model", json!(model)),
            Kind::Separate { params } => ("separate", params.get("model").cloned().unwrap_or(json!(MODELS[0].id))),
            Kind::InstallSpeech { model } => ("install_lyrics_model", json!(model)),
            Kind::Lyrics { params } => ("lyrics", params.get("model").cloned().unwrap_or(json!(soundcraft_lyrics::MODELS[0].id))),
        };
        json!({
            "job": self.id,
            "kind": kind,
            "model": model,
            "title": self.title,
            "state": self.state.name(),
            "progress": (self.progress() * 1000.0).round() / 1000.0,
            "waits_for": self.waits_for,
            "seconds": self.seconds,
            "result": self.result,
            "error": self.error,
        })
    }
}

/// The Stem Separation window's choices, kept while the app runs.
pub struct StemsDialog {
    pub open: bool,
    model: usize,
    stems: [bool; 6],
    instrumental: bool,
    mute_original: bool,
}

impl Default for StemsDialog {
    fn default() -> Self {
        StemsDialog { open: false, model: 0, stems: [true, true, true, true, true, true], instrumental: false, mute_original: true }
    }
}

/// The Transcribe Lyrics window's choices.
pub struct LyricsDialog {
    pub open: bool,
    separate: bool,
}

impl Default for LyricsDialog {
    fn default() -> Self {
        LyricsDialog { open: false, separate: true }
    }
}

#[derive(Default)]
pub struct StemJobs {
    next: u64,
    jobs: Vec<Job>,
    pub dialog: StemsDialog,
    pub lyrics_dialog: LyricsDialog,
}

impl StemJobs {
    /// Whether a job is queued or running.
    pub fn busy(&self) -> bool {
        self.jobs.iter().any(|j| !j.state.finished())
    }
}

fn models_dir(app: &SoundApp) -> std::path::PathBuf {
    soundcraft_engine::cmd::models_dir(&app.engine)
}

fn push(app: &mut SoundApp, kind: Kind, title: String, waits_for: Vec<u64>) -> u64 {
    app.stem_jobs.next = app.stem_jobs.next.saturating_add(1);
    let id = app.stem_jobs.next;
    app.stem_jobs.jobs.push(Job {
        id,
        kind,
        title,
        state: State::Queued,
        progress: Arc::new(AtomicU32::new(0)),
        cancel: Arc::new(AtomicBool::new(false)),
        rx: None,
        epoch: app.doc_epoch,
        blocks: Vec::new(),
        waits_for: waits_for.clone(),
        result: None,
        error: None,
        started: None,
        moving: None,
        seconds: None,
    });
    for w in waits_for {
        if let Some(j) = app.stem_jobs.jobs.iter_mut().find(|j| j.id == w) {
            j.blocks.push(id);
        }
    }
    id
}

/// Queue a separation of `params` (the `stems.separate` params, plus `download?: bool`). The
/// clips are fixed now: the current selection unless `clips` names them. Returns the job id
/// (the separation's; a download it needs first runs as its own job).
pub fn start(app: &mut SoundApp, params: &Value) -> Result<Value, String> {
    if !soundcraft_stems::supported() {
        return Err("stem separation is not available in the browser".into());
    }
    let mut params = params.clone();
    let download = params.get("download").and_then(Value::as_bool).unwrap_or(false);
    if let Some(o) = params.as_object_mut() {
        o.remove("download");
    } else {
        params = json!({});
    }
    let model = match params.get("model").and_then(Value::as_str) {
        None => &MODELS[0],
        Some(m) => soundcraft_stems::model(m).ok_or_else(|| format!("unknown model `{m}`"))?,
    };
    let s = app.engine.session();
    if params.get("clips").is_none() {
        let clips: Vec<u64> = s.edit.selected_clips.iter().filter(|c| s.find_clip(**c).is_some_and(|(_, x)| x.is_audio())).map(|c| c.0).collect();
        params["clips"] = json!(clips);
    }
    // Check the params now (stems, clips, lengths) so a mistake is reported before any download.
    let job = soundcraft_engine::cmd::stems_job(&app.engine, &params).map_err(|e| e.to_string())?;
    let names = job.clip_names();
    drop(job);
    let title = match names.as_slice() {
        [one] => format!("Separating “{one}”"),
        many => format!("Separating {} clips", many.len()),
    };
    let waits_for = need_stem_model(app, model, download)?.into_iter().collect::<Vec<_>>();
    let id = push(app, Kind::Separate { params }, title, waits_for.clone());
    Ok(json!({"job": id, "waits_for": waits_for}))
}

/// The download job a job must wait for to have the stem `model`: none when it is installed, a
/// queued one, or a new one when `download`; an error otherwise.
fn need_stem_model(app: &mut SoundApp, model: &'static soundcraft_stems::Model, download: bool) -> Result<Option<u64>, String> {
    if soundcraft_stems::installed(&models_dir(app), model) {
        return Ok(None);
    }
    let queued =
        app.stem_jobs.jobs.iter().find(|j| !j.state.finished() && matches!(j.kind, Kind::Install { model: m } if m == model.id)).map(|j| j.id);
    match queued {
        Some(id) => Ok(Some(id)),
        None if download => Ok(Some(push(
            app,
            Kind::Install { model: model.id },
            format!("Downloading the {} model ({} MB)", model.label, model.megabytes()),
            Vec::new(),
        ))),
        None => Err(format!("the `{}` stem model is not installed: run stems.install_model, or pass \"download\": true", model.id)),
    }
}

/// Like [`need_stem_model`], for the speech model.
fn need_speech_model(app: &mut SoundApp, model: &'static soundcraft_lyrics::Model, download: bool) -> Result<Option<u64>, String> {
    if soundcraft_lyrics::installed(&models_dir(app), model) {
        return Ok(None);
    }
    let queued =
        app.stem_jobs.jobs.iter().find(|j| !j.state.finished() && matches!(j.kind, Kind::InstallSpeech { model: m } if m == model.id)).map(|j| j.id);
    match queued {
        Some(id) => Ok(Some(id)),
        None if download => Ok(Some(push(
            app,
            Kind::InstallSpeech { model: model.id },
            format!("Downloading the {} speech model ({:.1} GB)", model.label, model.megabytes() as f32 / 1000.0),
            Vec::new(),
        ))),
        None => Err(format!("the `{}` speech model is not installed: run lyrics.install_model, or pass \"download\": true", model.id)),
    }
}

/// Queue a lyrics transcription of `params` (the `lyrics.transcribe` params, plus
/// `download?: bool`). The clip is fixed now: the selected audio clip unless `clip` names one.
pub fn start_lyrics(app: &mut SoundApp, params: &Value) -> Result<Value, String> {
    if !soundcraft_lyrics::supported() {
        return Err("lyrics transcription is not available in the browser".into());
    }
    let mut params = params.clone();
    let download = params.get("download").and_then(Value::as_bool).unwrap_or(false);
    if let Some(o) = params.as_object_mut() {
        o.remove("download");
    } else {
        params = json!({});
    }
    let s = app.engine.session();
    if params.get("clip").is_none() {
        let clip = s.edit.selected_clips.iter().find(|c| s.find_clip(**c).is_some_and(|(_, x)| x.is_audio())).map(|c| c.0);
        params["clip"] = json!(clip);
    }
    let job = soundcraft_engine::cmd::lyrics_job(&app.engine, &params).map_err(|e| e.to_string())?;
    let (title, separates, speech) = (format!("Transcribing “{}”", job.clip_name()), job.separates(), job.model());
    drop(job);
    let mut waits_for = Vec::new();
    if separates {
        waits_for.extend(need_stem_model(app, &MODELS[0], download)?);
    }
    waits_for.extend(need_speech_model(app, speech, download)?);
    let id = push(app, Kind::Lyrics { params }, title, waits_for.clone());
    Ok(json!({"job": id, "waits_for": waits_for}))
}

/// Queue the speech model download. Returns the job id, or `installed: true`.
pub fn start_install_speech(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let model = match p.get("model").and_then(Value::as_str) {
        None => &soundcraft_lyrics::MODELS[0],
        Some(m) => soundcraft_lyrics::model(m).ok_or_else(|| format!("unknown model `{m}`"))?,
    };
    match need_speech_model(app, model, true)? {
        None => Ok(json!({"model": model.id, "installed": true})),
        Some(id) => Ok(json!({"job": id})),
    }
}

/// Queue a model download. Returns the job id, or `installed: true` when there is nothing to do.
pub fn start_install(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    if !soundcraft_stems::supported() {
        return Err("stem separation is not available in the browser".into());
    }
    let model = match p.get("model").and_then(Value::as_str) {
        None => &MODELS[0],
        Some(m) => soundcraft_stems::model(m).ok_or_else(|| format!("unknown model `{m}`"))?,
    };
    if soundcraft_stems::installed(&models_dir(app), model) {
        return Ok(json!({"model": model.id, "installed": true}));
    }
    let id = push(app, Kind::Install { model: model.id }, format!("Downloading the {} model ({} MB)", model.label, model.megabytes()), Vec::new());
    Ok(json!({"job": id}))
}

/// `jobs.list`, `jobs.status {job}`, `jobs.cancel {job?}` (no job: every unfinished one).
pub fn command(app: &mut SoundApp, id: &str, p: &Value) -> Result<Value, String> {
    let job = p.get("job").and_then(Value::as_u64);
    match id {
        "jobs.list" => Ok(json!({"jobs": app.stem_jobs.jobs.iter().map(Job::status).collect::<Vec<_>>()})),
        "jobs.status" => {
            let job = job.ok_or("`job` required")?;
            app.stem_jobs.jobs.iter().find(|j| j.id == job).map(Job::status).ok_or_else(|| format!("no job {job}"))
        }
        "jobs.cancel" => {
            let ids: Vec<u64> = match job {
                Some(j) => vec![j],
                None => app.stem_jobs.jobs.iter().filter(|j| !j.state.finished()).map(|j| j.id).collect(),
            };
            let mut cancelled = Vec::new();
            for j in ids {
                if cancel(app, j) {
                    cancelled.push(j);
                }
            }
            Ok(json!({"cancelled": cancelled}))
        }
        _ => Err(format!("unknown command `{id}`")),
    }
}

/// Cancel a job and anything waiting for it. True when it was still unfinished.
fn cancel(app: &mut SoundApp, id: u64) -> bool {
    let Some(j) = app.stem_jobs.jobs.iter_mut().find(|j| j.id == id) else { return false };
    if j.state.finished() {
        return false;
    }
    j.cancel.store(true, Ordering::Relaxed);
    let blocks = j.blocks.clone();
    if j.state == State::Queued {
        j.state = State::Cancelled;
    }
    for b in blocks {
        cancel(app, b);
    }
    true
}

/// Start the next queued job and collect finished ones. Called every frame from `logic`.
pub fn step(app: &mut SoundApp, ctx: &egui::Context) {
    collect(app);
    for j in app.stem_jobs.jobs.iter_mut().filter(|j| j.state == State::Running && j.moving.is_none()) {
        if j.progress() > 0.0 {
            j.moving = Some(web_time::Instant::now());
        }
    }
    let running = app.stem_jobs.jobs.iter().any(|j| j.state == State::Running);
    if !running {
        let next = app.stem_jobs.jobs.iter().position(|j| {
            j.state == State::Queued
                && j.waits_for.iter().all(|w| app.stem_jobs.jobs.iter().find(|x| x.id == *w).is_none_or(|x| x.state == State::Done))
        });
        if let Some(i) = next {
            spawn(app, i, ctx);
        }
    }
    if app.stem_jobs.busy() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    // Forget old finished jobs.
    let finished = app.stem_jobs.jobs.iter().filter(|j| j.state.finished()).count();
    if finished > KEEP_FINISHED {
        let mut drop = finished - KEEP_FINISHED;
        app.stem_jobs.jobs.retain(|j| {
            if drop > 0 && j.state.finished() {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

#[cfg(target_arch = "wasm32")]
fn spawn(app: &mut SoundApp, i: usize, _ctx: &egui::Context) {
    if let Some(j) = app.stem_jobs.jobs.get_mut(i) {
        j.state = State::Failed;
        j.error = Some("stem separation is not available in the browser".into());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn(app: &mut SoundApp, i: usize, ctx: &egui::Context) {
    let dir = models_dir(app);
    enum Work {
        Install(&'static soundcraft_stems::Model),
        Separate(soundcraft_engine::cmd::StemsJob),
        InstallSpeech(&'static soundcraft_lyrics::Model),
        Lyrics(soundcraft_engine::cmd::LyricsJob),
    }
    let prepared: Result<Work, String> = match app.stem_jobs.jobs.get(i).map(|j| &j.kind) {
        Some(Kind::Install { model }) => soundcraft_stems::model(model).map(Work::Install).ok_or_else(|| "unknown model".to_string()),
        Some(Kind::Separate { params }) => soundcraft_engine::cmd::stems_job(&app.engine, params).map(Work::Separate).map_err(|e| e.to_string()),
        Some(Kind::InstallSpeech { model }) => soundcraft_lyrics::model(model).map(Work::InstallSpeech).ok_or_else(|| "unknown model".to_string()),
        Some(Kind::Lyrics { params }) => soundcraft_engine::cmd::lyrics_job(&app.engine, params).map(Work::Lyrics).map_err(|e| e.to_string()),
        None => return,
    };
    let Some(j) = app.stem_jobs.jobs.get_mut(i) else { return };
    let job = match prepared {
        Ok(w) => w,
        Err(e) => {
            j.state = State::Failed;
            j.error = Some(e);
            return;
        }
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let progress = Arc::clone(&j.progress);
    let cancel = Arc::clone(&j.cancel);
    let ctx = ctx.clone();
    let work = move || {
        let mut report = |p: f32| {
            progress.store(p.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            ctx.request_repaint();
        };
        let outcome = match job {
            Work::Install(model) => {
                match soundcraft_stems::install(&dir, model, &mut |done, total| report(done as f32 / total.max(1) as f32), &cancel) {
                    Ok(_) => Outcome::Installed,
                    Err(soundcraft_stems::Error::Cancelled) => Outcome::Cancelled,
                    Err(e) => Outcome::Failed(e.to_string()),
                }
            }
            Work::Separate(job) => match job.run(&mut report, &cancel) {
                Ok(done) => Outcome::Separated(done),
                Err(_) if cancel.load(Ordering::Relaxed) => Outcome::Cancelled,
                Err(e) => Outcome::Failed(e.to_string()),
            },
            Work::InstallSpeech(model) => {
                match soundcraft_lyrics::install(&dir, model, &mut |done, total| report(done as f32 / total.max(1) as f32), &cancel) {
                    Ok(_) => Outcome::Installed,
                    Err(soundcraft_lyrics::Error::Cancelled) => Outcome::Cancelled,
                    Err(e) => Outcome::Failed(e.to_string()),
                }
            }
            Work::Lyrics(job) => match job.run(&mut report, &cancel) {
                Ok(done) => Outcome::Transcribed(done),
                Err(_) if cancel.load(Ordering::Relaxed) => Outcome::Cancelled,
                Err(e) => Outcome::Failed(e.to_string()),
            },
        };
        let _ = tx.send(outcome);
        ctx.request_repaint();
    };
    match std::thread::Builder::new().name("stems-and-lyrics".into()).spawn(work) {
        Ok(_) => {
            j.state = State::Running;
            j.rx = Some(rx);
            j.started = Some(web_time::Instant::now());
        }
        Err(e) => {
            j.state = State::Failed;
            j.error = Some(format!("could not start: {e}"));
        }
    }
}

/// Take finished results: place separations, start what waited for a download.
fn collect(app: &mut SoundApp) {
    let mut finished = Vec::new();
    for (i, j) in app.stem_jobs.jobs.iter_mut().enumerate() {
        let Some(rx) = &j.rx else { continue };
        let outcome = match rx.try_recv() {
            Ok(o) => o,
            Err(std::sync::mpsc::TryRecvError::Empty) => continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Outcome::Failed("the separation stopped unexpectedly".into()),
        };
        j.rx = None;
        j.seconds = j.started.map(|s| (s.elapsed().as_secs_f32() * 10.0).round() / 10.0);
        finished.push((i, outcome));
    }
    for (i, outcome) in finished {
        let epoch = app.stem_jobs.jobs.get(i).map_or(0, |j| j.epoch);
        let (state, result, error) = match outcome {
            Outcome::Installed => {
                // Confirm through the command scripts use: the files are in place, so it returns at once.
                let (cmd, model) = match app.stem_jobs.jobs.get(i).map(|j| &j.kind) {
                    Some(Kind::InstallSpeech { model }) => ("lyrics.install_model", *model),
                    Some(Kind::Install { model }) => ("stems.install_model", *model),
                    _ => ("stems.install_model", MODELS[0].id),
                };
                match app.run(cmd, json!({"model": model})) {
                    Ok(v) => (State::Done, Some(v), None),
                    Err(e) => (State::Failed, None, Some(e)),
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            Outcome::Transcribed(done) => {
                let params = match app.stem_jobs.jobs.get(i).map(|j| &j.kind) {
                    Some(Kind::Lyrics { params }) => params.clone(),
                    _ => json!({}),
                };
                if epoch != app.doc_epoch {
                    (State::Failed, None, Some("the session changed while the clip was transcribed".to_string()))
                } else {
                    app.engine.offer_lyrics(done);
                    let r = app.run("lyrics.transcribe", params);
                    app.engine.drop_offered_lyrics();
                    match r {
                        Ok(v) => (State::Done, Some(v), None),
                        Err(e) => (State::Failed, None, Some(e)),
                    }
                }
            }
            Outcome::Cancelled => (State::Cancelled, None, None),
            Outcome::Failed(e) => (State::Failed, None, Some(e)),
            #[cfg(not(target_arch = "wasm32"))]
            Outcome::Separated(done) => {
                let params = match app.stem_jobs.jobs.get(i).map(|j| &j.kind) {
                    Some(Kind::Separate { params }) => params.clone(),
                    _ => json!({}),
                };
                if epoch != app.doc_epoch {
                    (State::Failed, None, Some("the session changed while the clips were separated".to_string()))
                } else {
                    app.engine.offer_stems(done);
                    let r = app.run("stems.separate", params);
                    app.engine.drop_offered_stems();
                    match r {
                        Ok(v) => (State::Done, Some(v), None),
                        Err(e) => (State::Failed, None, Some(e)),
                    }
                }
            }
        };
        let Some(j) = app.stem_jobs.jobs.get_mut(i) else { continue };
        j.state = state;
        j.result = result;
        j.error.clone_from(&error);
        let secs = j.seconds.unwrap_or(0.0);
        app.ui.status = match (&j.kind, state) {
            (_, State::Failed) => error.unwrap_or_default(),
            (_, State::Cancelled) => format!("{} cancelled", j.title),
            (Kind::Install { model }, _) => {
                format!("Downloaded the {} stem separation model", soundcraft_stems::model(model).map_or(*model, |m| m.label))
            }
            (Kind::InstallSpeech { model }, _) => {
                format!("Downloaded the {} speech model", soundcraft_lyrics::model(model).map_or(*model, |m| m.label))
            }
            (Kind::Lyrics { .. }, _) => {
                let r = j.result.as_ref();
                let lines = r.and_then(|v| v["lines"].as_u64()).unwrap_or(0);
                let choruses = r.and_then(|v| v["choruses"].as_array()).map_or(0, Vec::len);
                let chorus = if choruses == 0 { "no chorus found".to_string() } else { format!("chorus found {choruses} times") };
                format!("{}: {lines} lines, {chorus} ({secs:.1} s)", j.title.replacen("Transcribing", "Transcribed", 1))
            }
            (Kind::Separate { .. }, _) => {
                let tracks = j
                    .result
                    .as_ref()
                    .and_then(|v| v["separated"].as_array())
                    .map_or(0, |a| a.iter().map(|c| c["tracks"].as_array().map_or(0, Vec::len)).sum());
                format!("{} into {tracks} stem tracks ({secs:.1} s)", j.title.replacen("Separating", "Separated", 1))
            }
        };
        // A download that failed or was cancelled takes its waiting separation with it.
        if state != State::Done {
            for b in j.blocks.clone() {
                cancel(app, b);
            }
        }
    }
}

/// The running and waiting jobs, bottom right, each with its progress and Cancel.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let active: Vec<(u64, String, State, f32, Option<f32>, bool)> = app
        .stem_jobs
        .jobs
        .iter()
        .filter(|j| !j.state.finished())
        .map(|j| {
            (
                j.id,
                j.title.clone(),
                j.state,
                j.progress(),
                j.moving.map(|s| s.elapsed().as_secs_f32()),
                matches!(j.kind, Kind::Install { .. } | Kind::InstallSpeech { .. }),
            )
        })
        .collect();
    if active.is_empty() {
        return;
    }
    let t = Tokens::current();
    let mut cancel_ids = Vec::new();
    egui::Area::new(egui::Id::unique("stem_jobs")).anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -72.0)).show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_width(300.0);
            for (k, (id, title, state, p, elapsed, download)) in active.iter().enumerate() {
                if k > 0 {
                    ui.separator();
                }
                ui.label(egui::RichText::new(title).font(bold(12.5)));
                if *state == State::Queued {
                    ui.label(egui::RichText::new("Waiting…").small().color(t.text_dim));
                } else {
                    ui.add(egui::ProgressBar::new(*p).desired_height(8.0));
                }
                ui.horizontal(|ui| {
                    let left = match (*state, elapsed) {
                        // Before the first chunk: loading the model, and on the very first run tuning the GPU.
                        (State::Running, _) if *p <= 0.0 && !download => "Preparing the model (the first run takes longer)…".to_string(),
                        (State::Running, Some(e)) if *p > 0.05 && *p < 1.0 => {
                            // Progress counts from when it first moved, so the estimate leaves out the start-up.
                            let rest = (e / p * (1.0 - p)).max(0.0);
                            format!(
                                "{:.0}%, about {}",
                                p * 100.0,
                                if rest < 60.0 { format!("{rest:.0} s left") } else { format!("{:.0} min left", rest / 60.0) }
                            )
                        }
                        (State::Running, _) if *p >= 1.0 && !download => "Finishing…".to_string(),
                        (State::Running, _) => format!("{:.0}%", p * 100.0),
                        _ => String::new(),
                    };
                    ui.label(egui::RichText::new(left).small().color(t.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Cancel").clicked() {
                            cancel_ids.push(*id);
                        }
                    });
                });
            }
        });
    });
    for id in cancel_ids {
        cancel(app, id);
    }
}

/// Seconds as m:ss.
fn duration(frames: i64, rate: u32) -> String {
    let s = (frames.max(0) as f64 / f64::from(rate.max(1))).round() as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// The Stem Separation window: model, stems, options, and Separate.
pub fn dialog(app: &mut SoundApp, ctx: &egui::Context) {
    if !app.stem_jobs.dialog.open {
        return;
    }
    let t = Tokens::current();
    let dir = models_dir(app);
    let s = app.engine.session();
    let rate = s.sample_rate.hz();
    let clips: Vec<(String, i64)> =
        s.edit.selected_clips.iter().filter_map(|c| s.find_clip(*c)).filter(|(_, c)| c.is_audio()).map(|(_, c)| (c.name.clone(), c.length)).collect();
    let mut open = true;
    let mut go = false;
    let mut close = false;
    let d = &mut app.stem_jobs.dialog;
    let model = MODELS.get(d.model).unwrap_or(&MODELS[0]);
    let installed = soundcraft_stems::installed(&dir, model);
    let center = ctx.content_rect().center();
    egui::Window::new("Stem Separation")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(380.0)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(center)
        .show(ctx, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        ui.label(egui::RichText::new("Splits the selected clips into stems on new tracks, in a folder under the clip's track. Runs on this computer; nothing is uploaded.").color(t.text_dim));
        match clips.as_slice() {
            [] => {
                ui.label(egui::RichText::new("Select one or more audio clips first.").color(t.accent));
            }
            [(name, len)] => {
                ui.label(format!("Clip: {name} ({})", duration(*len, rate)));
            }
            many => {
                let total: i64 = many.iter().map(|(_, l)| *l).sum();
                ui.label(format!("{} clips ({} in all)", many.len(), duration(total, rate)));
            }
        }
        egui::Grid::new("stems_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Model");
            ui.vertical(|ui| {
                craft_ui::ComboBox::new(egui::Id::unique("stems_model"), "").selected_text(model.label).show_ui(ui, |ui| {
                    for (i, m) in MODELS.iter().enumerate() {
                        let mark = if soundcraft_stems::installed(&dir, m) { "" } else { "  ⤓" };
                        ui.selectable_value(&mut d.model, i, format!("{}{mark}", m.label)).on_hover_text(m.description);
                    }
                });
                let state = if installed { "downloaded".to_string() } else { format!("{} MB download on first use", model.megabytes()) };
                ui.label(egui::RichText::new(format!("{} ({state})", model.description)).small().color(t.text_dim));
            });
            ui.end_row();
            ui.label("Stems");
            ui.horizontal_wrapped(|ui| {
                for (k, stem) in Stem::ALL.iter().enumerate() {
                    if model.stems.contains(stem)
                        && let Some(on) = d.stems.get_mut(k)
                    {
                        ui.checkbox(on, stem.label());
                    }
                }
            });
            ui.end_row();
            ui.label("");
            ui.vertical(|ui| {
                ui.checkbox(&mut d.instrumental, "Instrumental").on_hover_text("Adds a track with the clip minus its vocals. It keeps everything but the vocals exactly; the stems only add back up approximately.");
                ui.checkbox(&mut d.mute_original, "Mute the original clip").on_hover_text("The stems replace it in the mix. The clip stays, so you can unmute it to compare.");
            });
            ui.end_row();
        });
        if !installed {
            ui.collapsing(egui::RichText::new("About the model").small(), |ui| {
                ui.label(egui::RichText::new(model.terms).small().color(t.text_dim));
                ui.label(egui::RichText::new(format!("Saved in {}", dir.display())).small().color(t.text_dim));
            });
        }
        let chosen = Stem::ALL.iter().enumerate().any(|(k, s)| model.stems.contains(s) && d.stems.get(k).copied().unwrap_or(false));
        let can = !clips.is_empty() && (chosen || d.instrumental);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if installed { "Separate".to_string() } else { format!("Download {} MB & Separate", model.megabytes()) };
                let primary = egui::Button::new(egui::RichText::new(label).strong().color(egui::Color32::WHITE)).fill(t.accent);
                if ui.add_enabled(can, primary).clicked() {
                    go = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    });
    if go {
        let stems: Vec<&str> = Stem::ALL
            .iter()
            .enumerate()
            .filter(|(k, s)| model.stems.contains(s) && d.stems.get(*k).copied().unwrap_or(false))
            .map(|(_, s)| s.id())
            .collect();
        let params = json!({
            "model": model.id,
            "stems": stems,
            "instrumental": d.instrumental,
            "mute_original": d.mute_original,
            "download": true,
        });
        match start(app, &params) {
            Ok(_) => close = true,
            Err(e) => app.ui.status = e,
        }
    }
    if !open || close {
        app.stem_jobs.dialog.open = false;
    }
}

/// The Transcribe Lyrics window: what happens, the download it needs, and Transcribe.
pub fn lyrics_dialog(app: &mut SoundApp, ctx: &egui::Context) {
    if !app.stem_jobs.lyrics_dialog.open {
        return;
    }
    let t = Tokens::current();
    let dir = models_dir(app);
    let s = app.engine.session();
    let rate = s.sample_rate.hz();
    let clip = s.edit.selected_clips.iter().filter_map(|c| s.find_clip(*c)).find(|(_, c)| c.is_audio()).map(|(tr, c)| {
        let vocal = s.track(tr).is_some_and(|t| t.name.starts_with("Vocals - ") || t.name.eq_ignore_ascii_case("vocals"));
        (c.name.clone(), c.length, vocal)
    });
    let speech = &soundcraft_lyrics::MODELS[0];
    let speech_ok = soundcraft_lyrics::installed(&dir, speech);
    let stems_ok = soundcraft_stems::installed(&dir, &MODELS[0]);
    let mut highlight = s.lyrics.highlight_chorus;
    let had_highlight = highlight;
    let mut open = true;
    let mut go = false;
    let mut close = false;
    let d = &mut app.stem_jobs.lyrics_dialog;
    let separate = d.separate && !clip.as_ref().is_some_and(|c| c.2);
    let mut need: u64 = 0;
    if !speech_ok {
        need += speech.megabytes();
    }
    if separate && !stems_ok {
        need += MODELS[0].megabytes();
    }
    let center = ctx.content_rect().center();
    egui::Window::new("Transcribe Lyrics")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(380.0)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(center)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            ui.label(
                egui::RichText::new("Writes the clip's lyrics into the Lyrics ruler above the tracks and marks the chorus. Runs on this computer; nothing is uploaded.")
                    .color(t.text_dim),
            );
            match &clip {
                None => {
                    ui.label(egui::RichText::new("Select an audio clip first.").color(t.accent));
                }
                Some((name, len, _)) => {
                    ui.label(format!("Clip: {name} ({})", duration(*len, rate)));
                }
            }
            if clip.as_ref().is_some_and(|c| c.2) {
                ui.label(egui::RichText::new("This is a vocal stem, so it is transcribed as it is.").small().color(t.text_dim));
            } else {
                ui.checkbox(&mut d.separate, "Separate the vocals first")
                    .on_hover_text("Transcribes the vocal stem instead of the whole mix: far fewer mistakes with music behind the voice.");
            }
            ui.checkbox(&mut highlight, "Highlight the chorus").on_hover_text("Colours the chorus lines in the Lyrics ruler.");
            ui.label(egui::RichText::new(format!("{}: {}", speech.label, speech.description)).small().color(t.text_dim));
            if need > 0 {
                ui.collapsing(egui::RichText::new("About the models").small(), |ui| {
                    ui.label(egui::RichText::new(speech.terms).small().color(t.text_dim));
                    if separate && !stems_ok {
                        ui.label(egui::RichText::new(MODELS[0].terms).small().color(t.text_dim));
                    }
                    ui.label(egui::RichText::new(format!("Saved in {}", dir.display())).small().color(t.text_dim));
                });
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if need == 0 {
                        "Transcribe".to_string()
                    } else if need >= 1000 {
                        format!("Download {:.1} GB & Transcribe", need as f32 / 1000.0)
                    } else {
                        format!("Download {need} MB & Transcribe")
                    };
                    let primary = egui::Button::new(egui::RichText::new(label).strong().color(egui::Color32::WHITE)).fill(t.accent);
                    if ui.add_enabled(clip.is_some(), primary).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        });
    if highlight != had_highlight {
        let _ = app.run("lyrics.highlight_chorus", json!({"value": highlight}));
    }
    if go {
        match start_lyrics(app, &json!({"separate": separate, "download": true})) {
            Ok(_) => close = true,
            Err(e) => app.ui.status = e,
        }
    }
    if !open || close {
        app.stem_jobs.lyrics_dialog.open = false;
    }
}
