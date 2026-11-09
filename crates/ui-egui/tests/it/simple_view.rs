//! View › Simple View switches to a plainer Edit window and back again.
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp, menus};

type Snapshot = (bool, bool, Vec<String>, Vec<f32>, &'static str, String);

fn snapshot(app: &SoundApp) -> Snapshot {
    let s = app.engine.session();
    (
        app.ui.show_tracks_list,
        app.ui.show_clip_list,
        s.edit.rulers.clone(),
        s.tracks.iter().map(|t| t.height.points()).collect(),
        s.edit.tool.id(),
        format!("{:?}", s.edit.edit_mode),
    )
}

#[test]
fn simple_view_hides_the_extras_and_switching_it_off_puts_them_back() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.run("edit.mode", json!({"mode": "grid"})).unwrap();
    app.run("edit.tool", json!({"tool": "grabber"})).unwrap();
    app.run("view.ruler", json!({"ruler": "tempo", "visible": true})).unwrap();
    fn has(nodes: &[menus::MenuNode], path: &str) -> bool {
        nodes.iter().any(|n| n.path == path || has(&n.children, path))
    }
    assert!(has(&menus::tree(), "View > Simple View"), "the View menu offers it");
    let before = snapshot(&app);
    assert!(before.0 && before.1, "the demo shows both side lists");
    assert_eq!(menus::item_state(&app, "View > Simple View", Some("view.simple")), (true, false), "enabled, unchecked");

    assert_eq!(app.run("view.simple", json!({})).unwrap(), json!({"value": true}));
    let simple = snapshot(&app);
    assert!(!simple.0 && !simple.1, "the side lists are hidden");
    assert_eq!(simple.2, vec!["min_secs".to_string(), "markers".to_string()]);
    assert!(simple.3.iter().all(|h| *h == soundcraft_model::TrackHeight::Large.points()), "{:?}", simple.3);
    assert_eq!((simple.4, simple.5.as_str()), ("smart", "Slip"));
    assert_eq!(menus::item_state(&app, "View > Simple View", Some("view.simple")), (true, true), "checked while on");
    assert_eq!(app.run("view.simple", json!({"value": true})).unwrap(), json!({"value": true}), "switching on twice keeps the saved view");

    assert_eq!(app.run("view.simple", json!({})).unwrap(), json!({"value": false}));
    assert_eq!(snapshot(&app), before, "everything is back as it was");
}
