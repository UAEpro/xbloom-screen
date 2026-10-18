//! Turning app data into what the screen shows: recipe cards, chart paths, time strings.
//!
//! Pure functions (no Slint window needed), so they're unit-tested on the host.

use slint::{Color, ModelRc, SharedString, VecModel};
use xbloom_app::history::HistoryEntry;
use xbloom_app::recipes::StoredRecipe;
use xbloom_app::schedule::Schedule;
use xbloom_app::util::LocalTime;
use xbloom_core::brew::{stage_label, Sample};
use xbloom_core::Recipe;

use crate::clock;
use crate::{HistoryRow, PourInfo, RecipeCard, TimerRow};

/// Card colours, cycled when a recipe has none of its own.
pub const TINTS: [&str; 6] = ["#c8743a", "#b5523b", "#8a6a3d", "#3f8f6b", "#3a7cb8", "#7a5cc0"];

/// Parse `#rrggbb`.
pub fn parse_hex(hex: &str) -> Option<Color> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// `#rrggbb` for a colour.
pub fn to_hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

/// "512 KB", "9.6 MB", "28.4 GB".
pub fn bytes(n: u64) -> String {
    let n = n as f64;
    if n >= 1e9 {
        format!("{:.1} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else {
        format!("{:.0} KB", n / 1e3)
    }
}

/// `m:ss`.
pub fn mmss(secs: f32) -> String {
    let s = secs.max(0.0).round() as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// "1:15", "1:15.5".
pub fn ratio_text(r: f64) -> String {
    if (r - r.round()).abs() < 0.05 {
        format!("1:{}", r.round() as i64)
    } else {
        format!("1:{r:.1}")
    }
}

/// Recipe kind for the card icon and the Coffee / Tea filter: `tea`, `no-grind`, `iced` or
/// `filter`.
pub fn kind(r: &Recipe) -> &'static str {
    if xbloom_app::recipes::is_tea(r) {
        "tea"
    } else if r.no_grind() {
        "no-grind"
    } else if r.ice_g.is_some() || r.hot_water_ml.is_some() || r.name.to_lowercase().contains("iced") {
        "iced"
    } else {
        "filter"
    }
}

/// Estimated brew time (grind + pours + pauses) when the recipe doesn't state one.
pub fn estimate_seconds(r: &Recipe) -> f32 {
    let grind = if r.no_grind() { 4.0 } else { 22.0 };
    let pours: f32 = r.pours.iter().map(|p| p.ml as f32 / p.flow_ml_s.max(0.1) as f32 + p.pause_s as f32).sum();
    grind + pours + 20.0 // + drawdown
}

/// A library recipe as a home-screen card.
pub fn recipe_card(s: &StoredRecipe, index: usize) -> RecipeCard {
    let r = &s.recipe;
    let total = r.total_water_ml().max(1) as f32;
    let pours: Vec<PourInfo> = r
        .pours
        .iter()
        .enumerate()
        .map(|(i, p)| PourInfo {
            label: stage_label(r, i).into(),
            ml: p.ml,
            temp: p.temp_c,
            pattern: SharedString::from(p.pattern.as_str()),
            pause: p.pause_s,
            agitation: p.agitation,
            share: p.ml as f32 / total,
        })
        .collect();
    let mut subtitle: Vec<String> = Vec::new();
    if let Some(d) = &r.dripper {
        subtitle.push(d.clone());
    }
    subtitle.push(match kind(r) {
        "tea" => "Tea".into(),
        "no-grind" => "Pre-ground".into(),
        "iced" => "Iced".into(),
        _ => r.kind.clone().filter(|k| k != "custom").unwrap_or_else(|| "Pour-over".into()),
    });
    let tint =
        s.meta.color.as_deref().and_then(parse_hex).unwrap_or_else(|| parse_hex(TINTS[index % TINTS.len()]).unwrap());
    RecipeCard {
        id: s.id.as_str().into(),
        name: r.name.as_str().into(),
        subtitle: subtitle.join(" · ").into(),
        kind: kind(r).into(),
        dose: r.dose_g,
        ratio: ratio_text(r.effective_ratio()).into(),
        ratio_value: r.effective_ratio() as f32,
        grind: if r.no_grind() { "—".into() } else { r.grind.to_string().into() },
        water: r.water_ml.map_or(r.total_water_ml(), |w| w.round() as i32),
        time: r.time.clone().unwrap_or_else(|| format!("~{}", mmss(estimate_seconds(r)))).into(),
        temp: r.pours.first().map_or_else(String::new, |p| format!("{}°", p.temp_c)).into(),
        note: r.note.clone().unwrap_or_default().into(),
        favorite: s.meta.favorite,
        tint,
        brews: s.meta.brews as i32,
        pours: ModelRc::new(VecModel::from(pours)),
    }
}

const SHORT_DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn same_day(a: LocalTime, b: LocalTime) -> bool {
    (a.year, a.month, a.day) == (b.year, b.month, b.day)
}

/// When a timed run happens, relative to `now`: "Today 07:00", "Tomorrow 07:00", "Sat 07:00".
/// `local` converts unix seconds to local time.
pub fn timer_when(at: u64, now: u64, h24: bool, local: &dyn Fn(i64) -> LocalTime) -> String {
    let (t, n) = (local(at as i64), local(now as i64));
    let time = clock::time_text(t, h24);
    if same_day(t, n) {
        format!("Today {time}")
    } else if same_day(t, local(now as i64 + 86_400)) {
        format!("Tomorrow {time}")
    } else {
        format!("{} {time}", SHORT_DAYS[t.weekday as usize % 7])
    }
}

/// A timer's days: "Every day", "Weekdays", "Weekends", "Mon, Wed, Fri", or "Once" for a
/// one-off.
pub fn days_text(days: &[bool; 7]) -> String {
    const NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    match days {
        [true, true, true, true, true, true, true] => "Every day".into(),
        [true, true, true, true, true, false, false] => "Weekdays".into(),
        [false, false, false, false, false, true, true] => "Weekends".into(),
        d if !d.iter().any(|&x| x) => "Once".into(),
        d => d.iter().zip(NAMES).filter(|(on, _)| **on).map(|(_, n)| n).collect::<Vec<_>>().join(", "),
    }
}

/// A timer for the list.
pub fn timer_row(s: &Schedule, recipe: &str, next: bool, h24: bool) -> TimerRow {
    let minutes = xbloom_app::settings::parse_hhmm(&s.time).unwrap_or(0);
    let t = LocalTime { hour: minutes / 60, minute: minutes % 60, second: 0, weekday: 0, day: 1, month: 0, year: 2026 };
    TimerRow {
        id: s.id.as_str().into(),
        time: clock::time_text(t, h24).into(),
        days: days_text(&s.days).into(),
        recipe: recipe.into(),
        enabled: s.enabled,
        next,
    }
}

/// Chart scaling for a brew: x spans the expected duration, y the target water + headroom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChartScale {
    /// Seconds across the full width.
    pub t_max: f32,
    /// Grams at the top.
    pub g_max: f32,
}

impl ChartScale {
    /// Scale for a brew of `target_g` expected to take `expected_s`, grown to fit `samples`.
    pub fn fit(target_g: f32, expected_s: f32, samples: &[Sample]) -> Self {
        let last_t = samples.last().map_or(0.0, |s| s.t);
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.water).max(s.coffee));
        Self { t_max: expected_s.max(last_t * 1.08).max(30.0), g_max: (target_g * 1.12).max(peak * 1.08).max(50.0) }
    }

    /// Where the target line sits, as a fraction from the top.
    pub fn target_y(&self, target_g: f32) -> f32 {
        (1.0 - target_g / self.g_max).clamp(0.0, 1.0)
    }
}

/// An SVG path (`M … L …`) in the chart's 1000 × 400 viewbox for one series.
pub fn chart_path(points: &[(f32, f32)], scale: ChartScale) -> String {
    if points.is_empty() {
        return String::new();
    }
    let step = (points.len() / 400).max(1);
    let mut out = String::with_capacity(points.len().min(400) * 14);
    let mut first = true;
    for (i, &(t, g)) in points.iter().enumerate() {
        if i % step != 0 && i != points.len() - 1 {
            continue;
        }
        let x = (t / scale.t_max * 1000.0).clamp(0.0, 1000.0);
        let y = (400.0 - g / scale.g_max * 400.0).clamp(0.0, 400.0);
        out.push_str(if first { "M " } else { " L " });
        out.push_str(&format!("{x:.1} {y:.1}"));
        first = false;
    }
    if points.len() == 1 {
        // A single point draws nothing; make it a dot-length segment.
        out.push_str(" l 0.1 0");
    }
    out
}

/// The water and coffee paths for a set of samples.
pub fn chart_paths(samples: &[Sample], scale: ChartScale) -> (String, String) {
    let water: Vec<(f32, f32)> = samples.iter().map(|s| (s.t, s.water)).collect();
    let coffee: Vec<(f32, f32)> = samples.iter().map(|s| (s.t, s.coffee)).collect();
    (chart_path(&water, scale), chart_path(&coffee, scale))
}

/// A history entry as a list row.
pub fn history_row(e: &HistoryEntry, when: String) -> HistoryRow {
    HistoryRow {
        id: e.id.as_str().into(),
        title: e.recipe.as_str().into(),
        when: when.into(),
        detail: format!("{} g · {} · {}", e.dose_g, ratio_text(e.ratio), mmss(e.duration_s)).into(),
        success: e.outcome == "completed",
    }
}

/// The samples of a saved curve.
pub fn curve_samples(e: &HistoryEntry) -> Vec<Sample> {
    e.curve.iter().map(|p| Sample { t: p[0], water: p[1], coffee: p[2] }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_round_trip() {
        let c = parse_hex("#3a86c8").unwrap();
        assert_eq!(to_hex(c), "#3a86c8");
        assert!(parse_hex("3a86c8").is_none());
    }

    #[test]
    fn times_and_ratios() {
        assert_eq!(mmss(151.4), "2:31");
        assert_eq!(ratio_text(15.0), "1:15");
        assert_eq!(ratio_text(15.5), "1:15.5");
    }

    #[test]
    fn chart_path_maps_into_viewbox() {
        let scale = ChartScale { t_max: 100.0, g_max: 200.0 };
        let p = chart_path(&[(0.0, 0.0), (50.0, 100.0), (100.0, 200.0)], scale);
        assert_eq!(p, "M 0.0 400.0 L 500.0 200.0 L 1000.0 0.0");
        assert_eq!(chart_path(&[], scale), "");
    }

    #[test]
    fn scale_grows_with_data() {
        let s = vec![Sample { t: 300.0, water: 400.0, coffee: 0.0 }];
        let sc = ChartScale::fit(240.0, 150.0, &s);
        assert!(sc.t_max >= 300.0 && sc.g_max >= 400.0);
        assert!((ChartScale::fit(240.0, 150.0, &[]).target_y(240.0) - (1.0 - 1.0 / 1.12)).abs() < 1e-3);
    }

    #[test]
    fn cards_from_starters() {
        let r = xbloom_app::recipes::parse_recipes(xbloom_app::recipes::STARTERS[2].1).unwrap().remove(0).unwrap();
        let s = StoredRecipe { id: "x".into(), meta: Default::default(), recipe: r };
        let c = recipe_card(&s, 0);
        assert_eq!(c.kind.as_str(), "iced");
        assert_eq!(c.ratio.as_str(), "1:10");
        assert_eq!(c.time.as_str(), "~1:45");
    }

    fn utc(ts: i64) -> LocalTime {
        let days = ts.div_euclid(86_400);
        let secs = ts.rem_euclid(86_400);
        LocalTime {
            hour: (secs / 3600) as u32,
            minute: (secs / 60 % 60) as u32,
            second: (secs % 60) as u32,
            weekday: ((days + 4).rem_euclid(7)) as u32,
            day: (days % 28) as u32 + 1,
            month: (days / 28 % 12) as u32,
            year: 1970 + (days / 336) as i32,
        }
    }

    #[test]
    fn timer_texts() {
        // Monday 2026-10-19 00:00 UTC.
        let monday: u64 = 1_792_368_000;
        let at = monday + 7 * 3600;
        assert_eq!(timer_when(at, monday + 3600, true, &utc), "Today 07:00");
        assert_eq!(timer_when(at + 86_400, monday + 22 * 3600, true, &utc), "Tomorrow 07:00");
        assert_eq!(timer_when(at + 5 * 86_400, monday, false, &utc), "Sat 7:00 AM");
        assert_eq!(days_text(&[true, true, true, true, true, false, false]), "Weekdays");
        assert_eq!(days_text(&[true, false, true, false, true, false, false]), "Mon, Wed, Fri");
        assert_eq!(days_text(&[false; 7]), "Once");
        assert_eq!(days_text(&[true; 7]), "Every day");
    }

    #[test]
    fn tea_cards() {
        let r = xbloom_app::recipes::parse_recipes(xbloom_app::recipes::STARTERS[3].1).unwrap().remove(0).unwrap();
        let s = StoredRecipe { id: "t".into(), meta: Default::default(), recipe: r };
        let c = recipe_card(&s, 0);
        assert_eq!((c.kind.as_str(), c.subtitle.as_str(), c.grind.as_str()), ("tea", "Omni · Tea", "—"));
        assert_eq!((c.water, c.temp.as_str()), (250, "80°"));
    }
}
