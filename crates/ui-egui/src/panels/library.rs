//! The DJ Library panel: folders and playlists on the left, the selected playlist's tracks on the
//! right in the user's order, and one search field over every shown column.
//!
//! Everything it changes goes through the `library.*` commands, so an assistant connected over
//! MCP or the control channel can do the same things.

use super::{Align2, Color32, Rect, Sense, SoundApp, Stroke, Tokens, Ui, bold, json, mono, pos2, regular, vec2};
use egui::text::{LayoutJob, TextFormat};
use soundcraft_engine::library::search::{Column, Query, format_time};
use soundcraft_engine::library::{self, NodeKind, Track};
use std::collections::HashSet;
use std::sync::Arc;

const ROW_H: f32 = 20.0;
const TREE_W: f32 = 230.0;
const BAR_H: f32 = 32.0;

/// A library track being dragged (onto the timeline or a playlist).
#[derive(Clone, Debug)]
pub struct TrackDrag {
    pub id: String,
    pub title: String,
}

/// One result row: the track and its position in the listed playlist.
#[derive(Clone)]
struct Row {
    position: usize,
    track: Track,
    lists: String,
}

#[derive(Clone, PartialEq)]
struct Key {
    rev: u64,
    tracks: usize,
    playlist: Option<u64>,
    recent: bool,
    query: String,
    columns: Vec<Column>,
    sort: Option<(Column, bool)>,
}

/// The tree's "Added in the last 30 days" entry, selected like a playlist.
pub const RECENT: u64 = u64::MAX;
/// How far back "Added in the last 30 days" looks.
const RECENT_DAYS: u64 = 30;

fn now_secs() -> u64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The sort the user picked by clicking a column title, if any.
fn user_sort(app: &SoundApp) -> Option<(Column, bool)> {
    app.ui.library_sort.as_deref().and_then(Column::parse).map(|c| (c, app.ui.library_sort_desc))
}

/// What the panel remembers between frames.
#[derive(Default)]
pub struct LibraryPanel {
    query: String,
    /// The listed folder or playlist; `None` lists every track.
    selected: Option<u64>,
    whole_library: bool,
    collapsed: HashSet<u64>,
    renaming: Option<(u64, String)>,
    selected_row: Option<usize>,
    cache: Option<(Key, Arc<Vec<Row>>, Vec<String>, usize)>,
    #[cfg(not(target_arch = "wasm32"))]
    sync: Option<std::sync::mpsc::Receiver<(String, Result<Vec<Track>, String>)>>,
    tried_first_sync: bool,
    status: String,
}

impl LibraryPanel {
    fn syncing(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        return self.sync.is_some();
        #[cfg(target_arch = "wasm32")]
        false
    }
}

/// The shown columns, from the saved UI state (unknown names are skipped).
pub fn shown_columns(app: &SoundApp) -> Vec<Column> {
    let cols: Vec<Column> = app.ui.library_columns.iter().filter_map(|c| Column::parse(c)).collect();
    if cols.is_empty() { Column::DEFAULT.to_vec() } else { cols }
}

/// Start reading the source playlist from the Music app on a worker thread.
fn start_sync(app: &mut SoundApp, ctx: &egui::Context) {
    if app.library.syncing() {
        return;
    }
    let source = app.engine.library.source().to_string();
    app.library.status = format!("Reading “{source}” from Music…");
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("library-sync".into()).spawn(move || {
            let read = library::apple_music::read_playlist(&source);
            let _ = tx.send((source, read));
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => app.library.sync = Some(rx),
            Err(e) => app.library.status = format!("Sync could not start: {e}"),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = ctx;
        let r = app.run("library.sync", json!({}));
        finish_sync(app, r);
    }
}

fn finish_sync(app: &mut SoundApp, r: Result<serde_json::Value, String>) {
    app.library.status = match r {
        Ok(v) => {
            let added = v["added"].as_u64().unwrap_or(0);
            if added > 0 { format!("{added} new") } else { String::new() }
        }
        Err(e) => e,
    };
}

#[cfg(not(target_arch = "wasm32"))]
fn poll_sync(app: &mut SoundApp) {
    let Some(rx) = &app.library.sync else { return };
    match rx.try_recv() {
        Ok((source, read)) => {
            app.library.sync = None;
            app.engine.offer_library(&source, read);
            let r = app.run("library.sync", json!({"playlist": source}));
            finish_sync(app, r);
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            app.library.sync = None;
            app.library.status = "Sync stopped unexpectedly".into();
        }
    }
}

/// "synced 3 min ago".
fn synced_label(at: Option<u64>) -> String {
    let Some(at) = at else { return "not synced yet".into() };
    let now = now_secs();
    let mins = now.saturating_sub(at) / 60;
    match mins {
        0 => "synced just now".into(),
        1..=59 => format!("synced {mins} min ago"),
        60..=2879 => format!("synced {} h ago", mins / 60),
        _ => format!("synced {} days ago", mins / 1440),
    }
}

/// The DJ Library dock panel.
pub fn dj_library(app: &mut SoundApp, ui: &mut Ui) {
    #[cfg(not(target_arch = "wasm32"))]
    poll_sync(app);
    if !app.library.tried_first_sync {
        app.library.tried_first_sync = true;
        if app.engine.library.synced_at.is_none() {
            start_sync(app, ui.ctx());
        }
    }
    let t = Tokens::current();
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, t.panel_bg);
    let tree = Rect::from_min_max(full.min, pos2((full.min.x + TREE_W).min(full.max.x), full.max.y));
    let right = Rect::from_min_max(pos2(tree.max.x + 1.0, full.min.y), full.max);
    ui.painter().line_segment([pos2(tree.max.x + 0.5, full.min.y), pos2(tree.max.x + 0.5, full.max.y)], Stroke::new(1.0, t.border));
    let mut tree_ui = ui.new_child(egui::UiBuilder::new().max_rect(tree).id_salt("library-tree"));
    tree_ui.set_clip_rect(tree);
    tree_panel(app, &mut tree_ui);
    if right.width() > 40.0 {
        let mut list_ui = ui.new_child(egui::UiBuilder::new().max_rect(right).id_salt("library-list"));
        list_ui.set_clip_rect(right);
        tracks_panel(app, &mut list_ui);
    }
}

fn small_button(ui: &mut Ui, text: &str, tip: &str) -> egui::Response {
    ui.add(egui::Button::new(egui::RichText::new(text).font(regular(11.0))).min_size(vec2(0.0, 18.0))).on_hover_text(tip)
}

fn tree_panel(app: &mut SoundApp, ui: &mut Ui) {
    let t = Tokens::current();
    ui.spacing_mut().item_spacing = vec2(4.0, 0.0);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(egui::RichText::new("PLAYLISTS").font(bold(11.5)).color(t.header_text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(6.0);
            let parent = app.library.selected.filter(|id| app.engine.library.node(*id).is_some_and(|n| n.kind == NodeKind::Folder));
            if small_button(ui, "+ Playlist", "New playlist (inside the selected folder)").clicked() {
                new_node(app, "library.playlist.new", "new playlist", parent);
            }
            if small_button(ui, "+ Folder", "New folder (inside the selected folder)").clicked() {
                new_node(app, "library.folder.new", "new folder", parent);
            }
        });
    });
    ui.add_space(4.0);
    let nodes: Vec<(u64, String, NodeKind, usize, usize, bool)> = {
        let l = &app.engine.library;
        let mut hidden: HashSet<u64> = HashSet::new();
        l.nodes
            .iter()
            .filter_map(|n| {
                if n.parent.is_some_and(|p| hidden.contains(&p) || app.library.collapsed.contains(&p)) {
                    hidden.insert(n.id);
                    return None;
                }
                let count = l.listing(Some(n.id)).map_or(0, |v| v.len());
                Some((n.id, n.name.clone(), n.kind, l.depth(n.id), count, app.library.collapsed.contains(&n.id)))
            })
            .collect()
    };
    let total = app.engine.library.tracks.len();
    egui::ScrollArea::vertical().id_salt("library-tree-scroll").auto_shrink([false, false]).max_height(ui.available_height() - 50.0).show(ui, |ui| {
        tree_row(app, ui, None, "All tracks", None, 0, total, false);
        let since = now_secs().saturating_sub(RECENT_DAYS * 86_400);
        let recent = app.engine.library.tracks.iter().filter(|t| t.added().is_some_and(|a| a >= since)).count();
        tree_row(app, ui, Some(RECENT), "Added in the last 30 days", None, 0, recent, false);
        for (id, name, kind, depth, count, collapsed) in nodes {
            tree_row(app, ui, Some(id), &name, Some(kind), depth, count, collapsed);
        }
    });
    // Source and sync status along the bottom.
    let r = Rect::from_min_max(pos2(ui.max_rect().min.x, ui.max_rect().max.y - 24.0), ui.max_rect().max);
    ui.painter().rect_filled(r, 0.0, t.panel_bg2);
    let syncing = app.library.syncing();
    let dot = if syncing {
        t.solo
    } else if app.engine.library.synced_at.is_some() {
        t.mode_on
    } else {
        t.text_dim
    };
    ui.painter().circle_filled(pos2(r.min.x + 10.0, r.center().y), 3.5, dot);
    let source = app.engine.library.source().to_string();
    let label = if syncing { "reading…".to_string() } else { synced_label(app.engine.library.synced_at) };
    ui.painter().with_clip_rect(Rect::from_min_max(pos2(r.min.x + 18.0, r.min.y), pos2(r.max.x - 54.0, r.max.y))).text(
        pos2(r.min.x + 18.0, r.center().y),
        Align2::LEFT_CENTER,
        format!("{source} · {label}"),
        regular(10.5),
        t.text_dim,
    );
    let br = Rect::from_min_size(pos2(r.max.x - 48.0, r.min.y + 3.0), vec2(42.0, 18.0));
    let resp = ui
        .put(br, egui::Button::new(egui::RichText::new("Sync").font(regular(11.0))))
        .on_hover_text("Read the source playlist from the Music app again (read-only). Your playlists keep their tracks.");
    if resp.clicked() && !syncing {
        start_sync(app, ui.ctx());
    }
    // Above the source line: bring in a rekordbox export.
    let ir = Rect::from_min_size(pos2(r.min.x + 6.0, r.min.y - 24.0), vec2(r.width() - 12.0, 20.0));
    let imported = app.engine.library.rekordbox_at.is_some();
    let label = if imported { "Import rekordbox XML again…" } else { "Import from rekordbox…" };
    if ui
        .put(ir, egui::Button::new(egui::RichText::new(label).font(regular(11.0))))
        .on_hover_text(
            "In rekordbox: File › Export Collection in xml format, then pick that file here. Its folders and playlists \
             land in a \"rekordbox\" folder (replaced on each import), and its key, BPM, rating, colour, comments and cues \
             are kept with each track. rekordbox is never changed.",
        )
        .clicked()
    {
        import_rekordbox(app);
    }
}

/// Picks a rekordbox XML export and imports it.
fn import_rekordbox(app: &mut SoundApp) {
    let Some(pick) = &app.services.pick_open else {
        app.library.status = "Choosing a file is not available here; use the library.import_rekordbox command".into();
        return;
    };
    let Some(path) = pick("library.import_rekordbox", &["xml"]) else { return };
    let path = match crate::dialogs::unicode(&path) {
        Ok(p) => p,
        Err(e) => {
            app.library.status = e.to_string();
            return;
        }
    };
    app.library.status = match app.run("library.import_rekordbox", json!({"path": path})) {
        Ok(v) => {
            if let Some(root) = v["folder"].as_u64() {
                app.library.collapsed.remove(&root);
            }
            format!(
                "rekordbox: {} playlists in {} folders · {} tracks matched · {} only in rekordbox{}",
                v["playlists"],
                v["folders"],
                v["matched"],
                v["added"],
                match v["missing"].as_u64() {
                    Some(0) | None => String::new(),
                    Some(n) => format!(" · {n} without a file left out"),
                }
            )
        }
        Err(e) => e,
    };
}

fn new_node(app: &mut SoundApp, command: &str, name: &str, parent: Option<u64>) {
    match app.run(command, json!({"name": name, "parent": parent})) {
        Ok(v) => {
            if let Some(id) = v["id"].as_u64() {
                if let Some(p) = parent {
                    app.library.collapsed.remove(&p);
                }
                app.library.renaming = Some((id, name.to_string()));
                if command == "library.playlist.new" {
                    app.library.selected = Some(id);
                }
            }
        }
        Err(e) => app.library.status = e,
    }
}

#[allow(clippy::too_many_arguments)]
fn tree_row(app: &mut SoundApp, ui: &mut Ui, id: Option<u64>, name: &str, kind: Option<NodeKind>, depth: usize, count: usize, collapsed: bool) {
    let t = Tokens::current();
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let selected = app.library.selected == id;
    // The recent list is not a folder or playlist: no renaming, menu or drops.
    let node_id = id.filter(|i| *i != RECENT);
    let drop_hover = node_id.is_some() && kind == Some(NodeKind::Playlist) && resp.dnd_hover_payload::<TrackDrag>().is_some();
    if selected {
        ui.painter().rect_filled(r, 0.0, t.row_selected);
    } else if drop_hover {
        ui.painter().rect_stroke(r.shrink(1.0), 2.0, Stroke::new(1.5, t.accent), egui::StrokeKind::Inside);
    } else if resp.hovered() {
        ui.painter().rect_filled(r, 0.0, t.panel_bg2);
    }
    let x = r.min.x + 10.0 + depth.min(library::MAX_DEPTH) as f32 * 14.0;
    let fg = if selected { Color32::WHITE } else { t.text };
    let icon = pos2(x + 5.0, r.center().y);
    match kind {
        Some(NodeKind::Folder) => {
            // A disclosure triangle and a folder outline, drawn in code.
            let tri = if collapsed {
                vec![pos2(icon.x - 3.0, icon.y - 4.0), pos2(icon.x + 3.0, icon.y), pos2(icon.x - 3.0, icon.y + 4.0)]
            } else {
                vec![pos2(icon.x - 4.0, icon.y - 2.5), pos2(icon.x + 4.0, icon.y - 2.5), pos2(icon.x, icon.y + 3.0)]
            };
            ui.painter().add(egui::Shape::convex_polygon(tri, t.text_dim, Stroke::NONE));
            let f = Rect::from_min_size(pos2(x + 13.0, r.center().y - 4.5), vec2(13.0, 9.0));
            ui.painter().rect_stroke(f, 1.0, Stroke::new(1.2, t.header_text), egui::StrokeKind::Inside);
            ui.painter().line_segment([pos2(f.min.x, f.min.y), pos2(f.min.x + 5.0, f.min.y - 2.0)], Stroke::new(1.2, t.header_text));
        }
        _ => {
            // A playlist: three lines.
            for (i, w) in [12.0, 12.0, 8.0].into_iter().enumerate() {
                let y = r.center().y - 4.0 + i as f32 * 4.0;
                ui.painter().line_segment([pos2(x + 13.0, y), pos2(x + 13.0 + w, y)], Stroke::new(1.2, if selected { fg } else { t.text_dim }));
            }
        }
    }
    let text_x = x + 32.0;
    if let Some(node) = node_id
        && app.library.renaming.as_ref().is_some_and(|(rid, _)| *rid == node)
    {
        let er = Rect::from_min_max(pos2(text_x - 2.0, r.min.y + 1.0), pos2(r.max.x - 6.0, r.max.y - 1.0));
        let mut text = app.library.renaming.as_ref().map(|(_, s)| s.clone()).unwrap_or_default();
        let edit = ui.put(er, egui::TextEdit::singleline(&mut text).font(regular(11.5)).margin(vec2(3.0, 1.0)));
        if !edit.has_focus() && !edit.lost_focus() {
            edit.request_focus();
        }
        let done = edit.lost_focus();
        app.library.renaming = Some((node, text.clone()));
        if done {
            app.library.renaming = None;
            if !ui.input(|i| i.key_pressed(egui::Key::Escape))
                && let Err(e) = app.run("library.rename", json!({"id": node, "name": text}))
            {
                app.library.status = e;
            }
        }
        return;
    }
    let font = if kind == Some(NodeKind::Folder) { bold(11.5) } else { regular(11.5) };
    let clip = Rect::from_min_max(pos2(text_x, r.min.y), pos2(r.max.x - 44.0, r.max.y));
    ui.painter().with_clip_rect(clip).text(pos2(text_x, r.center().y), Align2::LEFT_CENTER, name, font, fg);
    if kind != Some(NodeKind::Folder) || count > 0 {
        ui.painter().text(
            pos2(r.max.x - 8.0, r.center().y),
            Align2::RIGHT_CENTER,
            count.to_string(),
            mono(10.5),
            if selected { fg } else { t.header_text },
        );
    }
    if let Some(node) = node_id
        && let Some(drag) = resp.dnd_release_payload::<TrackDrag>()
        && kind == Some(NodeKind::Playlist)
    {
        match app.run("library.playlist.add", json!({"playlist": node, "tracks": [drag.id]})) {
            Ok(_) => app.library.status = format!("Added “{}” to {name}", drag.title),
            Err(e) => app.library.status = e,
        }
    }
    if resp.clicked() {
        if kind == Some(NodeKind::Folder) && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| p.x < x + 12.0) {
            if let Some(node) = node_id
                && !app.library.collapsed.remove(&node)
            {
                app.library.collapsed.insert(node);
            }
        } else {
            app.library.selected = id;
            app.library.selected_row = None;
            app.library.whole_library = false;
        }
    }
    if resp.double_clicked()
        && let Some(node) = node_id
    {
        if kind == Some(NodeKind::Folder) {
            if !app.library.collapsed.remove(&node) {
                app.library.collapsed.insert(node);
            }
        } else {
            app.library.renaming = Some((node, name.to_string()));
        }
    }
    if let Some(node) = node_id {
        resp.context_menu(|ui| {
            if ui.button("Rename").clicked() {
                app.library.renaming = Some((node, name.to_string()));
                ui.close();
            }
            if kind == Some(NodeKind::Folder) {
                if ui.button("New Playlist Inside").clicked() {
                    new_node(app, "library.playlist.new", "new playlist", Some(node));
                    ui.close();
                }
                if ui.button("New Folder Inside").clicked() {
                    new_node(app, "library.folder.new", "new folder", Some(node));
                    ui.close();
                }
            }
            if app.engine.library.node(node).is_some_and(|n| n.parent.is_some()) && ui.button("Move to Top Level").clicked() {
                if let Err(e) = app.run("library.move", json!({"id": node})) {
                    app.library.status = e;
                }
                ui.close();
            }
            let folders: Vec<(u64, String)> = app
                .engine
                .library
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Folder && n.id != node && n.parent != Some(node))
                .map(|n| (n.id, n.name.clone()))
                .collect();
            if !folders.is_empty() {
                ui.menu_button("Move Into", |ui| {
                    for (f, fname) in folders {
                        if ui.button(fname).clicked() {
                            if let Err(e) = app.run("library.move", json!({"id": node, "parent": f})) {
                                app.library.status = e;
                            }
                            ui.close();
                        }
                    }
                });
            }
            ui.separator();
            let what = if kind == Some(NodeKind::Folder) { "Delete Folder and Its Playlists" } else { "Delete Playlist" };
            if ui.button(what).on_hover_text("Only SoundCraft's playlist is deleted; the Music app is never changed").clicked() {
                if app.run("library.delete", json!({"id": node})).is_ok() && app.library.selected == Some(node) {
                    app.library.selected = None;
                }
                ui.close();
            }
        });
    }
}

/// The rows for the current playlist/scope, query and columns, cached until something changes.
fn rows(app: &mut SoundApp, columns: &[Column]) -> (Arc<Vec<Row>>, Vec<String>, usize) {
    let recent = !app.library.whole_library && app.library.selected == Some(RECENT);
    let playlist = if app.library.whole_library || recent { None } else { app.library.selected };
    // The recent list shows the newest first until a column is clicked.
    let sort = user_sort(app).or(recent.then_some((Column::Added, true)));
    let l = &app.engine.library;
    let key = Key { rev: l.rev, tracks: l.tracks.len(), playlist, recent, query: app.library.query.clone(), columns: columns.to_vec(), sort };
    if let Some((k, rows, understood, total)) = &app.library.cache
        && *k == key
    {
        return (rows.clone(), understood.clone(), *total);
    }
    let query = Query::parse(&app.library.query);
    let since = recent.then(|| now_secs().saturating_sub(RECENT_DAYS * 86_400));
    let total = if recent {
        l.find(None, &Query::default(), columns, since, None).map_or(0, |v| v.len())
    } else {
        l.listing(playlist).map_or(0, |v| v.len())
    };
    let lists = if playlist.is_none() && (!query.is_empty() || recent) { Some(l.memberships()) } else { None };
    let rows: Vec<Row> = l
        .find(playlist, &query, columns, since, sort)
        .unwrap_or_default()
        .into_iter()
        .map(|(position, t)| Row {
            position,
            lists: lists.as_ref().and_then(|m| m.get(t.id.as_str())).map(|v| v.join(", ")).unwrap_or_default(),
            track: t.clone(),
        })
        .collect();
    let rows = Arc::new(rows);
    let understood = query.describe();
    app.library.cache = Some((key, rows.clone(), understood.clone(), total));
    (rows, understood, total)
}

/// `text` with the first match of any of `words` on a highlight.
fn highlighted(text: &str, words: &[String], font: egui::FontId, color: Color32, width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    let lower = text.to_lowercase();
    // Lowercasing can change byte lengths; only highlight when it does not.
    let hit = (lower.len() == text.len())
        .then(|| words.iter().filter(|w| !w.is_empty()).filter_map(|w| lower.find(w.as_str()).map(|i| (i, i + w.len()))).min())
        .flatten()
        .filter(|(a, b)| text.is_char_boundary(*a) && text.is_char_boundary(*b));
    let plain = TextFormat { font_id: font.clone(), color, ..Default::default() };
    match hit {
        Some((a, b)) => {
            job.append(text.get(..a).unwrap_or(""), 0.0, plain.clone());
            job.append(
                text.get(a..b).unwrap_or(""),
                0.0,
                TextFormat { font_id: font, color: Color32::WHITE, background: Color32::from_rgb(110, 92, 20), ..Default::default() },
            );
            job.append(text.get(b..).unwrap_or(""), 0.0, plain);
        }
        None => job.append(text, 0.0, plain),
    }
    job
}

/// Column widths for `w` points: fixed numbers, and the rest shared by title, artist and genre.
fn widths(columns: &[Column], lists: bool, w: f32) -> (f32, Vec<(Column, f32)>, f32) {
    let num = 40.0;
    let fixed = |c: Column| match c {
        Column::Bpm => Some(52.0),
        Column::Key => Some(46.0),
        Column::Time => Some(54.0),
        Column::Rating => Some(66.0),
        Column::Added => Some(78.0),
        _ => None,
    };
    let flex = |c: Column| match c {
        Column::Title => 3.0,
        Column::Artist => 2.0,
        _ => 1.4,
    };
    let lists_w = if lists { 150.0 } else { 0.0 };
    let used: f32 = num + lists_w + columns.iter().filter_map(|c| fixed(*c)).sum::<f32>();
    let parts: f32 = columns.iter().filter(|c| fixed(**c).is_none()).map(|c| flex(*c)).sum();
    let spare = (w - used).max(0.0);
    let cols = columns.iter().map(|c| (*c, fixed(*c).unwrap_or_else(|| if parts > 0.0 { spare * flex(*c) / parts } else { 0.0 }))).collect();
    (num, cols, lists_w)
}

/// The column titles: click one to sort lowest first, again for highest first, and a third time
/// (or on #) for the playlist's own order. Returns where the titles end.
fn sort_header(app: &mut SoundApp, ui: &mut Ui, head: Rect, num_w: f32, cols: &[(Column, f32)]) -> f32 {
    let t = Tokens::current();
    let mut cx = head.min.x + num_w;
    let sorted = user_sort(app).or((app.library.selected == Some(RECENT) && !app.library.whole_library).then_some((Column::Added, true)));
    let num_rect = Rect::from_min_max(head.min, pos2(head.min.x + num_w, head.max.y));
    let num_resp = ui.interact(num_rect, ui.scope_id().with("library-sort-order"), Sense::click()).on_hover_text("Back to the playlist's own order");
    ui.painter().text(
        pos2(head.min.x + 10.0, head.center().y),
        Align2::LEFT_CENTER,
        "#",
        bold(11.0),
        if sorted.is_none() { t.accent } else { t.header_text },
    );
    if num_resp.clicked() {
        app.ui.library_sort = None;
    }
    for (c, w) in cols {
        let cell = Rect::from_min_max(pos2(cx, head.min.y), pos2(cx + w, head.max.y));
        let here = sorted.filter(|(s, _)| s == c).map(|(_, d)| d);
        let right = matches!(c, Column::Bpm | Column::Time);
        let px = if right { cx + w - 22.0 } else { cx };
        let colour = if here.is_some() { t.accent } else { t.header_text };
        let g = ui.painter().layout_no_wrap(c.label().to_string(), bold(11.0), colour);
        let gw = g.size().x;
        let gx = if right { px - gw } else { px };
        ui.painter().with_clip_rect(cell).galley(pos2(gx, head.center().y - g.size().y * 0.5), g, colour);
        if let Some(descending) = here {
            // A small triangle drawn in code: up for lowest first, down for highest first.
            let ax = (gx + gw + 8.0).min(cell.max.x - 6.0);
            let ay = head.center().y;
            let pts = if descending {
                vec![pos2(ax - 4.0, ay - 2.5), pos2(ax + 4.0, ay - 2.5), pos2(ax, ay + 3.0)]
            } else {
                vec![pos2(ax - 4.0, ay + 2.5), pos2(ax + 4.0, ay + 2.5), pos2(ax, ay - 3.0)]
            };
            ui.painter().with_clip_rect(cell).add(egui::Shape::convex_polygon(pts, t.accent, Stroke::NONE));
        }
        let resp = ui
            .interact(cell, ui.scope_id().with(("library-sort", c.id())), Sense::click())
            .on_hover_text("Sort: lowest first, then highest first, then your order");
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            // Lowest first, then highest first, then back to the playlist's order.
            match here {
                None => {
                    app.ui.library_sort = Some(c.id().to_string());
                    app.ui.library_sort_desc = false;
                }
                Some(false) => {
                    app.ui.library_sort = Some(c.id().to_string());
                    app.ui.library_sort_desc = true;
                }
                Some(true) => {
                    app.ui.library_sort = None;
                    app.ui.library_sort_desc = false;
                }
            }
            app.library.selected_row = None;
        }
        cx += w;
    }
    cx
}

fn tracks_panel(app: &mut SoundApp, ui: &mut Ui) {
    let t = Tokens::current();
    let columns = shown_columns(app);
    let full = ui.max_rect();
    // Search bar.
    let bar = Rect::from_min_size(full.min, vec2(full.width(), BAR_H));
    ui.painter().rect_filled(bar, 0.0, t.panel_bg2);
    let mut bar_ui =
        ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(8.0, 5.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    bar_ui.spacing_mut().item_spacing = vec2(4.0, 0.0);
    let search_w = (bar.width() * 0.42).clamp(160.0, 520.0);
    let id = egui::Id::unique("library-search");
    let edit = bar_ui.add(
        egui::TextEdit::singleline(&mut app.library.query)
            .id(id)
            .hint_text("Search title, artist, BPM, key…  e.g. afro 120-124 8A")
            .desired_width(search_w)
            .font(regular(12.0)),
    );
    if edit.changed() {
        app.library.selected_row = None;
    }
    if edit.has_focus() && bar_ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.library.query.clear();
    }
    if !app.library.query.is_empty() && bar_ui.add(egui::Button::new("×").small()).on_hover_text("Clear the search").clicked() {
        app.library.query.clear();
    }
    bar_ui.add_space(6.0);
    for (whole, label) in [(false, "This playlist"), (true, "Whole library")] {
        let on = app.library.whole_library == whole;
        if crate::widgets::text_toggle(&mut bar_ui, vec2(if whole { 92.0 } else { 84.0 }, 20.0), label, on, t.mode_on, "Where the search looks")
            .clicked()
        {
            app.library.whole_library = whole;
        }
    }
    bar_ui.add_space(8.0);
    let shown_names: Vec<&str> = columns.iter().map(|c| if *c == Column::Title { "Title" } else { c.label() }).collect();
    bar_ui
        .menu_button(egui::RichText::new(format!("Columns: {}", shown_names.join(", "))).font(regular(11.0)), |ui| {
            ui.label(egui::RichText::new("Shown columns are searched").font(regular(10.5)).color(t.text_dim));
            for c in Column::ALL {
                let mut on = columns.contains(&c);
                if ui.checkbox(&mut on, c.label()).changed() {
                    let mut next: Vec<Column> = Column::ALL.into_iter().filter(|x| if *x == c { on } else { columns.contains(x) }).collect();
                    if next.is_empty() {
                        next.push(Column::Title);
                    }
                    app.ui.library_columns = next.iter().map(|c| c.id().to_string()).collect();
                }
            }
        })
        .response
        .on_hover_text("Choose the columns; a hidden column is not searched unless you name it (genre:afro)");
    // What the search understood.
    let (rows, understood, total) = rows(app, &columns);
    let info = Rect::from_min_size(pos2(full.min.x, bar.max.y), vec2(full.width(), 22.0));
    ui.painter().rect_filled(info, 0.0, t.panel_bg);
    ui.painter().line_segment([pos2(info.min.x, info.max.y), pos2(info.max.x, info.max.y)], Stroke::new(1.0, t.border));
    let place = if !app.library.whole_library && app.library.selected == Some(RECENT) {
        "the last 30 days".to_string()
    } else if app.library.whole_library || app.library.selected.is_none() {
        "the whole library".to_string()
    } else {
        app.library.selected.and_then(|id| app.engine.library.node(id)).map_or_else(|| "the whole library".into(), |n| n.name.clone())
    };
    let summary = if app.library.query.trim().is_empty() {
        format!("{total} tracks in {place}")
    } else {
        format!("{} of {total} match in {place}", rows.len())
    };
    let mut x = info.min.x + 10.0;
    let g = ui.painter().layout_no_wrap(summary, bold(11.0), t.text);
    let gw = g.size().x;
    ui.painter().galley(pos2(x, info.center().y - g.size().y * 0.5), g, t.text);
    x += gw + 10.0;
    for chip in &understood {
        let g = ui.painter().layout_no_wrap(chip.clone(), regular(10.5), t.text);
        let cr = Rect::from_min_size(pos2(x, info.min.y + 3.0), vec2(g.size().x + 10.0, info.height() - 6.0));
        if cr.max.x > info.max.x - 200.0 {
            break;
        }
        ui.painter().rect_filled(cr, 2.0, t.button);
        ui.painter().galley(pos2(cr.min.x + 5.0, cr.center().y - g.size().y * 0.5), g, t.text);
        x = cr.max.x + 4.0;
    }
    let status =
        if app.library.status.is_empty() { "Drag a track onto the timeline, or onto a playlist".to_string() } else { app.library.status.clone() };
    ui.painter().with_clip_rect(info).text(pos2(info.max.x - 10.0, info.center().y), Align2::RIGHT_CENTER, status, regular(10.5), t.text_dim);
    // Column titles.
    let in_playlist = !app.library.whole_library && app.library.selected.is_some_and(|s| s != RECENT);
    let show_lists = !in_playlist && !app.library.query.trim().is_empty();
    let head = Rect::from_min_size(pos2(full.min.x, info.max.y), vec2(full.width(), 20.0));
    ui.painter().rect_filled(head, 0.0, t.ruler_bg);
    let (num_w, cols, lists_w) = widths(&columns, show_lists, head.width() - 12.0);
    let cx = sort_header(app, ui, head, num_w, &cols);
    if show_lists {
        ui.painter().text(pos2(cx, head.center().y), Align2::LEFT_CENTER, "In playlist", bold(11.0), t.text_dim);
    }
    let _ = lists_w;
    // Rows.
    let body = Rect::from_min_max(pos2(full.min.x, head.max.y), full.max);
    let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body).id_salt("library-rows"));
    body_ui.set_clip_rect(body);
    body_ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
    let words = Query::parse(&app.library.query).highlight_words();
    if rows.is_empty() {
        let msg = if app.engine.library.tracks.is_empty() {
            if app.library.syncing() { "Reading your Music playlist…" } else { "No tracks yet. Press Sync to read your Music playlist." }
        } else if app.library.query.trim().is_empty() {
            "This playlist is empty. Drag tracks onto it from All tracks."
        } else {
            "No tracks match. Try fewer words, or search the whole library."
        };
        body_ui.painter().text(body.min + vec2(14.0, 18.0), Align2::LEFT_TOP, msg, regular(12.0), t.text_dim);
        return;
    }
    let playlist = if in_playlist { app.library.selected } else { None };
    egui::ScrollArea::vertical().id_salt(("library-rows-scroll", playlist, app.library.whole_library)).auto_shrink([false, false]).show_rows(
        &mut body_ui,
        ROW_H,
        rows.len(),
        |ui, shown| {
            for i in shown {
                let Some(row) = rows.get(i) else { continue };
                track_row(app, ui, i, row, &cols, num_w, show_lists, &words, playlist);
            }
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn track_row(
    app: &mut SoundApp,
    ui: &mut Ui,
    i: usize,
    row: &Row,
    cols: &[(Column, f32)],
    num_w: f32,
    show_lists: bool,
    words: &[String],
    playlist: Option<u64>,
) {
    let t = Tokens::current();
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click_and_drag());
    let selected = app.library.selected_row == Some(i);
    let bg = if selected {
        t.row_selected
    } else if resp.hovered() {
        t.button
    } else if i % 2 == 1 {
        t.playlist_alt
    } else {
        t.playlist_bg
    };
    ui.painter().rect_filled(r, 0.0, bg);
    if let Some([cr, cg, cb]) = row.track.colour() {
        ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(3.0, r.height())), 0.0, Color32::from_rgb(cr, cg, cb));
    }
    let p = ui.painter().with_clip_rect(r.intersect(ui.clip_rect()));
    let fg = if selected { Color32::WHITE } else { t.text };
    p.text(pos2(r.min.x + 10.0, r.center().y), Align2::LEFT_CENTER, (i + 1).to_string(), mono(10.0), t.text_dim);
    let mut x = r.min.x + num_w;
    for (c, w) in cols {
        let cell = Rect::from_min_max(pos2(x, r.min.y), pos2(x + w - 8.0, r.max.y));
        match c {
            Column::Title | Column::Artist | Column::Genre | Column::Comment => {
                let text = c.text(&row.track);
                let color = if *c == Column::Title {
                    fg
                } else if *c == Column::Artist {
                    if selected { fg } else { t.header_text }
                } else {
                    t.text_dim
                };
                let job = highlighted(&text, words, regular(11.5), color, cell.width());
                let g = ui.painter().layout_job(job);
                p.with_clip_rect(cell).galley(pos2(cell.min.x, cell.center().y - g.size().y * 0.5), g, color);
            }
            Column::Bpm | Column::Time => {
                let text = if *c == Column::Bpm { row.track.bpm().map_or("—".into(), |b| b.to_string()) } else { format_time(row.track.seconds) };
                p.text(pos2(x + w - 10.0, r.center().y), Align2::RIGHT_CENTER, text, mono(11.0), if *c == Column::Bpm { fg } else { t.text_dim });
            }
            Column::Added => {
                p.text(pos2(x, r.center().y), Align2::LEFT_CENTER, c.text(&row.track), mono(11.0), t.text_dim);
            }
            Column::Rating => {
                // Five dots, the rated ones lit.
                let stars = row.track.rating();
                for s in 0..5u8 {
                    let c = pos2(x + 5.0 + f32::from(s) * 11.0, r.center().y);
                    if s < stars {
                        p.circle_filled(c, 3.5, t.solo);
                    } else {
                        p.circle_stroke(c, 3.0, Stroke::new(1.0, t.border_light));
                    }
                }
            }
            Column::Key => {
                let (text, color) = match row.track.key() {
                    Some(k) => (k.to_string(), t.counter_text),
                    None => ("—".into(), t.text_dim),
                };
                p.text(pos2(x, r.center().y), Align2::LEFT_CENTER, text, mono(11.0), color);
            }
        }
        x += w;
    }
    if show_lists {
        p.text(pos2(x, r.center().y), Align2::LEFT_CENTER, &row.lists, regular(10.5), t.text_dim);
    }
    if resp.clicked() {
        app.library.selected_row = Some(i);
    }
    resp.dnd_set_drag_payload(TrackDrag { id: row.track.id.clone(), title: row.track.title.clone() });
    if resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        if let Some(pos) = ui.ctx().pointer_latest_pos() {
            egui::Area::new(egui::Id::unique("library-drag-ghost"))
                .order(egui::Order::Tooltip)
                .fixed_pos(pos + vec2(14.0, 6.0))
                .interactable(false)
                .show(ui.ctx(), |ui| {
                    egui::Frame::NONE.fill(t.panel_bg2).stroke(Stroke::new(1.0, t.accent)).inner_margin(egui::Margin::symmetric(8, 4)).show(
                        ui,
                        |ui| {
                            ui.label(egui::RichText::new(&row.track.title).font(regular(11.5)).color(t.text));
                        },
                    );
                });
        }
    }
    if resp.double_clicked() {
        add_to_session(app, &row.track, None);
    }
    let id = row.track.id.clone();
    let position = row.position;
    resp.context_menu(|ui| {
        if ui.button("Add to Session at Insertion Point").clicked() {
            add_to_session(app, &row.track, None);
            ui.close();
        }
        let lists: Vec<(u64, String)> =
            app.engine.library.nodes.iter().filter(|n| n.kind == NodeKind::Playlist).map(|n| (n.id, n.name.clone())).collect();
        ui.menu_button("Add to Playlist", |ui| {
            if lists.is_empty() {
                ui.label("No playlists yet");
            }
            for (lid, name) in lists {
                if ui.button(name).clicked() {
                    if let Err(e) = app.run("library.playlist.add", json!({"playlist": lid, "tracks": [id]})) {
                        app.library.status = e;
                    }
                    ui.close();
                }
            }
        });
        if let Some(list) = playlist {
            ui.separator();
            if ui.button("Move Up").clicked() {
                let _ = app.run("library.playlist.reorder", json!({"playlist": list, "from": position, "to": position.saturating_sub(1)}));
                ui.close();
            }
            if ui.button("Move Down").clicked() {
                let _ = app.run("library.playlist.reorder", json!({"playlist": list, "from": position, "to": position.saturating_add(1)}));
                ui.close();
            }
            if ui.button("Remove from Playlist").clicked() {
                let _ = app.run("library.playlist.remove", json!({"playlist": list, "positions": [position]}));
                app.library.selected_row = None;
                ui.close();
            }
        }
    });
}

/// Imports a library track into the session at `at` (default: the insertion point).
pub fn add_to_session(app: &mut SoundApp, track: &Track, at: Option<i64>) {
    let mut p = json!({"track": track.id});
    if let Some(at) = at {
        p["at"] = json!(at);
    }
    match app.run("library.import", p) {
        Ok(_) => app.library.status = format!("Added “{}” to the session", track.title),
        Err(e) => app.library.status = e,
    }
}

/// Imports a library track dropped on the timeline at the drop point.
pub fn drop_on_timeline(app: &mut SoundApp, drag: &TrackDrag, at: i64) {
    let track = app.engine.library.track(&drag.id).cloned();
    match track {
        Some(t) => add_to_session(app, &t, Some(at.max(0))),
        None => app.library.status = "That track is no longer in the library".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_fill_the_row() {
        let (num, cols, lists) = widths(&Column::DEFAULT, false, 800.0);
        let sum: f32 = num + lists + cols.iter().map(|(_, w)| w).sum::<f32>();
        assert!((sum - 800.0).abs() < 0.5);
        let (_, cols, _) = widths(&Column::ALL, true, 10.0);
        assert!(cols.iter().all(|(_, w)| *w >= 0.0), "a narrow panel never gets negative widths");
    }

    #[test]
    fn highlight_handles_any_text() {
        for text in ["", "Siren", "ÉLAN élan", "İstanbul", "ß"] {
            let job = highlighted(text, &["la".into(), "".into(), "s".into()], regular(11.0), Color32::WHITE, 100.0);
            assert_eq!(job.text, text);
        }
    }
}
