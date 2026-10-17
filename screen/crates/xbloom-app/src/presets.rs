//! The machine's three Easy-Mode dial presets (A / B / C).
//!
//! The screen remembers which library recipe is assigned to each slot and when the set was
//! last pushed. The machine can't report its slots back (neither can the vendor app), and the
//! phone app may overwrite them, so this record is the source of truth: re-push to restore.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::{read_json, write_json};

/// Slot assignments and push state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Presets {
    /// Recipe id per slot (A, B, C).
    pub slots: [Option<String>; 3],
    /// When the current assignment was last written to the machine, unix seconds.
    pub pushed_at: Option<u64>,
    /// The assignment that was pushed (to show "changed since last push").
    pub pushed_slots: [Option<String>; 3],
}

impl Presets {
    /// True if the assignment differs from what was last pushed.
    pub fn dirty(&self) -> bool {
        self.slots != self.pushed_slots || self.pushed_at.is_none()
    }

    /// All three slots assigned?
    pub fn complete(&self) -> bool {
        self.slots.iter().all(Option::is_some)
    }
}

/// Persistence for [`Presets`].
#[derive(Debug)]
pub struct PresetStore {
    path: PathBuf,
    /// Current state.
    pub presets: Presets,
}

impl PresetStore {
    /// Open the store.
    pub fn open(path: PathBuf) -> Self {
        let presets = read_json(&path).ok().flatten().unwrap_or_default();
        Self { path, presets }
    }

    /// Persist.
    pub fn save(&self) {
        if let Err(e) = write_json(&self.path, &self.presets) {
            log::error!("presets: save failed: {e}");
        }
    }

    /// Clear any slot pointing at a deleted recipe.
    pub fn forget_recipe(&mut self, id: &str) {
        let mut changed = false;
        for s in &mut self.presets.slots {
            if s.as_deref() == Some(id) {
                *s = None;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }
}
