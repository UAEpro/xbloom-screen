//! Brew history: completed brews with their water / coffee curves.
//!
//! Newest first, capped (smaller on an ESP32's flash), with each curve downsampled so the
//! file stays small.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use xbloom_core::brew::Sample;

use crate::util::{new_id, read_json, write_json};

/// One finished brew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Stable id.
    pub id: String,
    /// Finished at, unix seconds.
    pub ts: u64,
    /// Library id of the recipe, if it came from the library.
    pub recipe_id: Option<String>,
    /// Recipe name at brew time.
    pub recipe: String,
    /// Dose, grams.
    pub dose_g: i32,
    /// Brew ratio.
    pub ratio: f64,
    /// Grind (0 = no-grind).
    pub grind: i32,
    /// Peak water dispensed, grams.
    pub water_g: f32,
    /// Peak coffee in the cup, grams.
    pub coffee_g: f32,
    /// Duration, seconds.
    pub duration_s: f32,
    /// Started remotely from the screen (vs approved on the machine).
    pub remote_start: bool,
    /// Outcome id (see `BrewOutcome::id`).
    pub outcome: String,
    /// The curve: `[t, water, coffee]` points.
    #[serde(default)]
    pub curve: Vec<[f32; 3]>,
}

impl HistoryEntry {
    /// A new entry with a fresh id.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ts: u64,
        recipe_id: Option<String>,
        recipe: &xbloom_core::Recipe,
        water_g: f32,
        coffee_g: f32,
        duration_s: f32,
        remote_start: bool,
        outcome: &str,
        samples: &[Sample],
        max_points: usize,
    ) -> Self {
        Self {
            id: new_id(),
            ts,
            recipe_id,
            recipe: recipe.name.clone(),
            dose_g: recipe.dose_g,
            ratio: recipe.effective_ratio(),
            grind: recipe.grind,
            water_g,
            coffee_g,
            duration_s,
            remote_start,
            outcome: outcome.into(),
            curve: downsample(samples, max_points),
        }
    }

    /// The entry without its curve (for list views).
    pub fn summary(&self) -> Self {
        Self { curve: Vec::new(), ..self.clone() }
    }
}

/// Keep at most `max` evenly spaced points (always including the last).
pub fn downsample(samples: &[Sample], max: usize) -> Vec<[f32; 3]> {
    let round = |x: f32| (x * 10.0).round() / 10.0;
    let pt = |s: &Sample| [round(s.t), round(s.water), round(s.coffee)];
    if samples.len() <= max || max < 2 {
        return samples.iter().map(pt).collect();
    }
    let step = (samples.len() - 1) as f32 / (max - 1) as f32;
    (0..max).map(|i| pt(&samples[((i as f32 * step).round() as usize).min(samples.len() - 1)])).collect()
}

/// The history log.
#[derive(Debug)]
pub struct HistoryStore {
    path: PathBuf,
    entries: Vec<HistoryEntry>,
    max_entries: usize,
    /// Points kept per curve.
    pub max_points: usize,
}

impl HistoryStore {
    /// Open the log at `path`.
    pub fn open(path: PathBuf, max_entries: usize, max_points: usize) -> Self {
        let entries = read_json::<Vec<HistoryEntry>>(&path).unwrap_or_else(|e| {
            log::error!("history: {} unreadable ({e}); starting fresh", path.display());
            None
        });
        Self { path, entries: entries.unwrap_or_default(), max_entries, max_points }
    }

    fn save(&self) {
        if let Err(e) = write_json(&self.path, &self.entries) {
            log::error!("history: save failed: {e}");
        }
    }

    /// Newest-first summaries (no curves).
    pub fn list(&self) -> Vec<HistoryEntry> {
        self.entries.iter().map(HistoryEntry::summary).collect()
    }

    /// One entry with its curve.
    pub fn get(&self, id: &str) -> Option<HistoryEntry> {
        self.entries.iter().find(|e| e.id == id).cloned()
    }

    /// Record a brew.
    pub fn record(&mut self, entry: HistoryEntry) {
        self.entries.insert(0, entry);
        self.entries.truncate(self.max_entries);
        self.save();
    }

    /// Delete one entry.
    pub fn delete(&mut self, id: &str) -> bool {
        let n = self.entries.len();
        self.entries.retain(|e| e.id != id);
        let removed = n != self.entries.len();
        if removed {
            self.save();
        }
        removed
    }

    /// Delete everything.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.save();
    }

    /// CSV export (no curves).
    pub fn to_csv(&self) -> String {
        let mut out =
            String::from("finished_unix,recipe,dose_g,ratio,grind,water_g,coffee_g,duration_s,remote_start,outcome\n");
        for e in &self.entries {
            let name = e.recipe.replace('"', "\"\"");
            out.push_str(&format!(
                "{},\"{}\",{},{},{},{},{},{},{},{}\n",
                e.ts, name, e.dose_g, e.ratio, e.grind, e.water_g, e.coffee_g, e.duration_s, e.remote_start, e.outcome
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsample_keeps_ends() {
        let s: Vec<Sample> = (0..1000).map(|i| Sample { t: i as f32, water: i as f32, coffee: 0.0 }).collect();
        let d = downsample(&s, 100);
        assert_eq!(d.len(), 100);
        assert_eq!(d[0][0], 0.0);
        assert_eq!(d[99][0], 999.0);
    }

    #[test]
    fn caps_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.json");
        let mut h = HistoryStore::open(p.clone(), 3, 10);
        let r = crate::recipes::parse_recipes(crate::recipes::STARTERS[0]).unwrap().remove(0).unwrap();
        for i in 0..5 {
            h.record(HistoryEntry::new(i, None, &r, 240.0, 200.0, 150.0, true, "completed", &[], 10));
        }
        let h2 = HistoryStore::open(p, 3, 10);
        assert_eq!(h2.list().len(), 3);
        assert_eq!(h2.list()[0].ts, 4);
        assert!(h2.to_csv().contains("\"Example Washed\""));
    }
}
