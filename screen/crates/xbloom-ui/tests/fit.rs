//! The controls you must always be able to reach stay inside the window, big enough to tap, at
//! every supported screen size, landscape and portrait: the brew buttons, Stop and its
//! confirmation, the PIN keypads, and the button that finishes setup.

use i_slint_backend_testing::{AccessibleRole, ElementQuery};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use xbloom_ui::{AppState, AppWindow, BrewInfo, Layout, Overlay, Page, PourInfo, RecipeCard};

const SIZES: [(u32, u32); 12] = [
    (480, 272),
    (480, 320),
    (640, 400),
    (800, 480),
    (1024, 600),
    (1280, 800),
    (1920, 1080),
    (480, 480),
    (720, 720),
    (320, 480),
    (480, 800),
    (720, 1280),
];

fn strings(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>()))
}

fn recipe() -> RecipeCard {
    let pour = |label: &str, ml: i32| PourInfo {
        label: label.into(),
        ml,
        temp: 93,
        pattern: "spiral".into(),
        pause: 10,
        agitation: false,
        share: ml as f32 / 240.0,
    };
    RecipeCard {
        id: "r1".into(),
        name: "A recipe with a fairly long name".into(),
        subtitle: "Omni · Pour-over".into(),
        kind: "filter".into(),
        dose: 16,
        ratio: "1:15".into(),
        ratio_value: 15.0,
        grind: "62".into(),
        water: 240,
        time: "~2:30".into(),
        temp: "93°".into(),
        note: "Clean and balanced.".into(),
        tint: slint::Color::from_rgb_u8(200, 116, 58),
        pours: ModelRc::new(VecModel::from(vec![pour("Bloom", 45), pour("Pour 1", 100), pour("Pour 2", 95)])),
        ..Default::default()
    }
}

/// Lay out at `w` × `h` (no rendering), then check every button labelled with each of `labels`.
fn reachable(ui: &AppWindow, (w, h): (u32, u32), state: &str, labels: &[&str]) {
    // Runs the window's size-change handlers, which feed the layout classes.
    slint::platform::update_timers_and_animations();
    for label in labels {
        let wanted = label.to_string();
        let found = ElementQuery::from_root(ui)
            .match_descendants()
            .match_accessible_role(AccessibleRole::Button)
            .match_predicate(move |e| e.accessible_label().is_some_and(|l| l == wanted.as_str()))
            .find_all();
        assert!(!found.is_empty(), "{w}×{h} {state}: no “{label}”");
        for e in found {
            let (p, s) = (e.absolute_position(), e.size());
            assert!(s.width >= 40.0 && s.height >= 36.0, "{w}×{h} {state}: “{label}” is only {s:?}");
            assert!(
                p.x >= -0.5 && p.y >= -0.5 && p.x + s.width <= w as f32 + 0.5 && p.y + s.height <= h as f32 + 0.5,
                "{w}×{h} {state}: “{label}” at {p:?}, size {s:?}, is off screen"
            );
        }
    }
}

#[test]
fn essential_controls_fit_every_screen() {
    i_slint_backend_testing::init_no_event_loop();
    for size in SIZES {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::PhysicalSize::new(size.0, size.1));
        let g = ui.global::<AppState>();
        g.set_recipes(ModelRc::new(VecModel::from(vec![recipe()])));
        g.set_current(recipe());
        g.set_allow_remote_start(true);
        g.set_simulator(true);

        g.set_page(Page::Recipe);
        reachable(&ui, size, "recipe", &["Hold to brew", "Load only"]);

        g.set_brew(BrewInfo {
            active: true,
            stage: "brewing".into(),
            message: "Brewing…".into(),
            recipe: "A recipe".into(),
            water: 130.0,
            target: 240.0,
            progress: 0.5,
            elapsed: "1:20".into(),
            stage_index: 1,
            stages: strings(&["Bloom", "Pour 1", "Pour 2"]),
            ..Default::default()
        });
        g.set_page(Page::Brewing);
        reachable(&ui, size, "brewing", &["Stop"]);
        g.set_overlay(Overlay::CancelBrew);
        reachable(&ui, size, "stop dialog", &["Keep brewing", "Stop brew"]);
        g.set_overlay(Overlay::None);

        let mut done = g.get_brew();
        done.finished = true;
        done.success = true;
        g.set_brew(done);
        reachable(&ui, size, "brew done", &["Done", "Brew again"]);

        g.set_overlay(Overlay::Pin);
        reachable(&ui, size, "PIN dialog", &["1", "0", "Delete", "OK"]);
        g.set_overlay(Overlay::None);

        g.set_lock_mode(2);
        g.set_locked(true);
        reachable(&ui, size, "lock screen", &["1", "0", "OK"]);
        g.set_locked(false);

        g.set_needs_setup(true);
        g.set_setup_step(1);
        reachable(&ui, size, "setup PIN", &["1", "0", "OK"]);
        g.set_setup_step(3);
        g.set_admin_hint(
            "Scan to join the Wi-Fi “xBloom-Screen-1A2B” (password k7mx2pq9tz), then open the address above.".into(),
        );
        reachable(&ui, size, "setup done", &["Start brewing"]);
    }
}

#[test]
fn interface_size_keeps_the_panel_size_and_its_minimum_layout() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let layout = |ui: &AppWindow| {
        slint::platform::update_timers_and_animations();
        let l = ui.global::<Layout>();
        (l.get_width(), l.get_height())
    };

    // 150 % on a 1920 × 1080 panel lays out like 1280 × 720, in the same pixels.
    ui.window().set_size(slint::PhysicalSize::new(1920, 1080));
    let max = xbloom_ui::bind::apply_interface_size(&ui, 150);
    assert_eq!(layout(&ui), (1280.0, 720.0));
    assert_eq!(ui.window().size(), slint::PhysicalSize::new(1920, 1080));
    assert_eq!(max, 200);

    // 200 % on 800 × 480 stops at 480 px wide, the smallest layout.
    ui.window().set_size(slint::PhysicalSize::new(800, 480));
    let max = xbloom_ui::bind::apply_interface_size(&ui, 200);
    assert_eq!(layout(&ui), (480.0, 288.0));
    assert_eq!(max, 160);

    // And back to 100 %.
    xbloom_ui::bind::apply_interface_size(&ui, 100);
    assert_eq!(layout(&ui), (800.0, 480.0));
}
