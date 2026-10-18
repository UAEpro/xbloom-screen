//! The recipe model, its (YAML/JSON) schema, and validation — a port of `xbloom_ble/recipe.py`.
//!
//! The schema is the same one the Python package reads, so a recipe file works with both the
//! `xbloom` CLI and the screen:
//!
//! ```yaml
//! name: Example Washed
//! dose_g: 16          # 1–18 g
//! grind: 62           # 1–80, or 0 = no-grind (brew pre-ground)
//! ratio: 15           # optional; if given, Σ pour ml must equal dose_g × ratio
//! stage_temps: [110.0, 90.0]
//! pours:
//!   - {label: Bloom, ml: 45, temp_c: 93, pattern: spiral, agitation: true, pause_s: 40, rpm: 100, flow_ml_s: 3.0}
//!   - {ml: 195, temp_c: 91, pattern: spiral, pause_s: 5, rpm: 100, flow_ml_s: 3.2}
//! ```
//!
//! Optional metadata (`kind`, `dripper`, `water_ml`, `hot_water_ml`, `ice_g`, `time`, `note`,
//! per-pour `label`) round-trips but is never sent to the machine.
//!
//! Validation messages are identical to the Python reference (checked by the golden tests),
//! plus one wire-format guard the reference lacks: the encoded pours must fit the pours
//! frame's one-byte length field.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::protocol::{self, WirePour, WireRecipe, LOAD_SEQ};
use crate::pyfmt::{py_bool, py_float, py_num, round_half_even};

/// A pour pattern. `Other` keeps an unknown name so validation can report it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Pattern {
    /// A spiral pour (the default; the only pattern that allows agitation).
    #[default]
    Spiral,
    /// A ring ("middle") pour.
    Ring,
    /// A single centre dot.
    Center,
    /// An unrecognised pattern name (always fails validation).
    Other(String),
}

impl Pattern {
    /// The schema name (`"spiral"`, `"ring"`, `"center"`).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Spiral => "spiral",
            Self::Ring => "ring",
            Self::Center => "center",
            Self::Other(s) => s,
        }
    }

    /// Parse a schema name.
    pub fn parse(s: &str) -> Self {
        match s {
            "spiral" => Self::Spiral,
            "ring" => Self::Ring,
            "center" => Self::Center,
            other => Self::Other(other.to_string()),
        }
    }
}

impl Serialize for Pattern {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Pattern {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self::parse(&s))
    }
}

/// A recipe failed to parse or validate. The message matches the Python reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeError(pub String);

impl fmt::Display for RecipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for RecipeError {}

/// A single pour stage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pour {
    /// Optional human label ("Bloom", "Pour 1"). Never sent to the machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Water volume, ml (≥ 1; over 127 is auto-split on the wire).
    pub ml: i32,
    /// Water temperature, °C (40–95).
    pub temp_c: i32,
    /// Pour pattern.
    pub pattern: Pattern,
    /// Pause after this pour, seconds (0–255; the machine's countdown caps near 99).
    pub pause_s: i32,
    /// Agitation speed, RPM (60–120; 0 allowed only for `center`).
    pub rpm: i32,
    /// Flow rate, ml/s (3.0–3.5).
    pub flow_ml_s: f64,
    /// Agitated bloom — only valid with `spiral`.
    pub agitation: bool,
}

impl Pour {
    /// The protocol-layer shape of this pour.
    pub fn to_wire(&self) -> WirePour {
        WirePour {
            ml: self.ml,
            temp: self.temp_c,
            pattern: self.pattern.clone(),
            agitation: self.agitation,
            pause: self.pause_s,
            rpm: self.rpm,
            flow: self.flow_ml_s,
        }
    }
}

fn ser_opt_num<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(x) if x.is_finite() && *x == libm::trunc(*x) && libm::fabs(*x) < 1e15 => s.serialize_i64(*x as i64),
        Some(x) => s.serialize_f64(*x),
        None => s.serialize_none(),
    }
}

/// A full, validated xBloom Studio recipe.
///
/// Field order matches the reference's `to_dict()`, so serialising produces the same
/// readable layout the Python package writes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recipe {
    /// Display name.
    pub name: String,
    /// Coffee dose, grams (1–18).
    pub dose_g: i32,
    /// Grinder setting 1–80 (lower is finer), or 0 for no-grind.
    pub grind: i32,
    /// Optional brew ratio (water : coffee); if set, Σ pours must equal `dose_g × ratio`.
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_num")]
    pub ratio: Option<f64>,
    /// Machine stage set-points (preheat), default `[110, 90]` °C.
    pub stage_temps: [f64; 2],
    /// Recipe kind / preset base (metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Dripper used (metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dripper: Option<String>,
    /// Total brew water, may exceed Σ pours for bypass / iced brews (metadata).
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_num")]
    pub water_ml: Option<f64>,
    /// Iced: hot water poured over ice (metadata).
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_num")]
    pub hot_water_ml: Option<f64>,
    /// Iced: ice weight (metadata).
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_num")]
    pub ice_g: Option<f64>,
    /// Expected brew time, display string (metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    /// Free-text notes (metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The pours, in order (≥ 2: a bloom and a first pour).
    pub pours: Vec<Pour>,
}

// ── Lenient raw input ────────────────────────────────────────────────────────────────────

/// A number the way the reference reads one: an int or a float (ints truncate like `int()`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Num(f64);

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Num;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number")
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Num, E> {
                Ok(Num(v as f64))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Num, E> {
                Ok(Num(v as f64))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Num, E> {
                Ok(Num(v))
            }
        }
        d.deserialize_any(V)
    }
}

impl Num {
    fn int(self) -> i32 {
        libm::trunc(self.0).clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
}

/// Any YAML/JSON scalar, stringified like Python's `str()` (for `name`).
#[derive(Debug, Clone, PartialEq)]
struct Scalar(String);

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Scalar;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or number")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Scalar, E> {
                Ok(Scalar(v.to_string()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Scalar, E> {
                Ok(Scalar(py_float(v)))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Scalar, E> {
                Ok(Scalar(py_bool(v).to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

/// A pour as read from a file, before required-key checks.
#[derive(Debug, Clone, Deserialize)]
pub struct RawPour {
    ml: Option<Num>,
    temp_c: Option<Num>,
    pattern: Option<Pattern>,
    agitation: Option<bool>,
    pause_s: Option<Num>,
    rpm: Option<Num>,
    flow_ml_s: Option<Num>,
    label: Option<Scalar>,
}

/// A recipe as read from a file, before required-key checks and validation.
///
/// Deserialize into this, then call [`Recipe::from_raw`] to get the reference's exact
/// error messages. (Deserializing straight into [`Recipe`] does the same via `try_from`.)
#[derive(Debug, Clone, Deserialize)]
pub struct RawRecipe {
    name: Option<Scalar>,
    dose_g: Option<Num>,
    grind: Option<Num>,
    pours: Option<Vec<RawPour>>,
    stage_temps: Option<Vec<Num>>,
    ratio: Option<Num>,
    dripper: Option<Scalar>,
    kind: Option<Scalar>,
    water_ml: Option<Num>,
    hot_water_ml: Option<Num>,
    ice_g: Option<Num>,
    time: Option<Scalar>,
    note: Option<Scalar>,
}

impl<'de> Deserialize<'de> for Recipe {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = RawRecipe::deserialize(d)?;
        Recipe::from_raw(raw).map_err(|e| de::Error::custom(e.0))
    }
}

impl Recipe {
    /// Build and validate a recipe from raw input, mirroring the reference's `from_dict`.
    pub fn from_raw(raw: RawRecipe) -> Result<Self, RecipeError> {
        let err = |m: &str| RecipeError(m.to_string());
        let raw_pours = raw.pours.ok_or_else(|| err("recipe is missing required key 'pours'"))?;
        if raw_pours.is_empty() {
            return Err(err("recipe 'pours' must be a non-empty list"));
        }
        let mut pours = Vec::with_capacity(raw_pours.len());
        for (i, rp) in raw_pours.into_iter().enumerate() {
            let missing = |k: &str| RecipeError(format!("pour #{} missing key '{k}'", i + 1));
            pours.push(Pour {
                ml: rp.ml.ok_or_else(|| missing("ml"))?.int(),
                temp_c: rp.temp_c.ok_or_else(|| missing("temp_c"))?.int(),
                pattern: rp.pattern.unwrap_or_default(),
                agitation: rp.agitation.unwrap_or(false),
                pause_s: rp.pause_s.map_or(0, Num::int),
                rpm: rp.rpm.map_or(0, Num::int),
                flow_ml_s: rp.flow_ml_s.map_or(3.0, |n| n.0),
                label: rp.label.map(|s| s.0),
            });
        }
        let stage_temps = match raw.stage_temps {
            None => [110.0, 90.0],
            Some(t) if t.len() == 2 => [t[0].0, t[1].0],
            Some(_) => return Err(err("'stage_temps' must be a 2-element list [temp1, temp2]")),
        };
        let dose = raw.dose_g.ok_or_else(|| err("recipe is missing required key 'dose_g'"))?;
        let grind = raw.grind.ok_or_else(|| err("recipe is missing required key 'grind'"))?;
        let recipe = Recipe {
            name: raw.name.map_or_else(|| "Unnamed".to_string(), |s| s.0),
            dose_g: dose.int(),
            grind: grind.int(),
            ratio: raw.ratio.map(|n| n.0),
            stage_temps,
            kind: raw.kind.map(|s| s.0),
            dripper: raw.dripper.map(|s| s.0),
            water_ml: raw.water_ml.map(|n| n.0),
            hot_water_ml: raw.hot_water_ml.map(|n| n.0),
            ice_g: raw.ice_g.map(|n| n.0),
            time: raw.time.map(|s| s.0),
            note: raw.note.map(|s| s.0),
            pours,
        };
        recipe.validate()?;
        Ok(recipe)
    }

    /// Validate against the machine's limits (see the README's ranges table).
    pub fn validate(&self) -> Result<(), RecipeError> {
        let mut errors: Vec<String> = Vec::new();
        if !(1..=18).contains(&self.dose_g) {
            errors.push(format!("dose_g {} out of range (1–18 g; 18 g is the app maximum)", self.dose_g));
        }
        if self.grind != 0 && !(1..=80).contains(&self.grind) {
            errors.push(format!(
                "grind {} out of range (1–80; the grinder has 80 micro-steps — \
                 or 0 for no-grind / pre-ground)",
                self.grind
            ));
        }
        for (label, t) in [("stage temp1", self.stage_temps[0]), ("stage temp2", self.stage_temps[1])] {
            if !(40.0..=130.0).contains(&t) {
                errors.push(format!("{label} {}°C out of range (40–130°C)", py_float(t)));
            }
        }
        if self.pours.len() < 2 {
            errors.push("recipe needs at least a bloom pour and a first pour (≥2 pours)".into());
        }
        let mut total_ml: i64 = 0;
        for (n, p) in self.pours.iter().enumerate() {
            let i = n + 1;
            if protocol::pattern_codes(&p.pattern, p.agitation).is_none() {
                errors.push(format!(
                    "pour #{i}: pattern/agitation ('{}', {}) not in known set \
                     ['center', 'ring', 'spiral'] (agitation only valid with 'spiral')",
                    p.pattern.as_str(),
                    py_bool(p.agitation)
                ));
            }
            if !(1..=4000).contains(&p.ml) {
                errors.push(format!("pour #{i}: ml {} out of range (1–4000)", p.ml));
            }
            if !(40..=95).contains(&p.temp_c) {
                errors.push(format!("pour #{i}: temp_c {} out of range (40–95°C)", p.temp_c));
            }
            if p.pattern == Pattern::Center {
                if p.rpm != 0 && !(60..=120).contains(&p.rpm) {
                    errors.push(format!("pour #{i}: rpm {} out of range (0 for a center pour, else 60–120)", p.rpm));
                }
            } else if !(60..=120).contains(&p.rpm) {
                errors.push(format!("pour #{i}: rpm {} out of range (60–120)", p.rpm));
            }
            if !(3.0..=3.5).contains(&p.flow_ml_s) {
                errors.push(format!("pour #{i}: flow_ml_s {} out of range (3.0–3.5)", py_float(p.flow_ml_s)));
            }
            if !(0..=255).contains(&p.pause_s) {
                errors.push(format!("pour #{i}: pause_s {} out of range (0–255)", p.pause_s));
            }
            total_ml += i64::from(p.ml);
        }
        if let Some(ratio) = self.ratio {
            let expected = round_half_even(f64::from(self.dose_g) * ratio) as i64;
            if total_ml != expected {
                errors.push(format!(
                    "Σpours = {total_ml} ml but dose_g*ratio = {}*{} = {expected} ml",
                    self.dose_g,
                    py_num(ratio)
                ));
            }
        }
        for (label, v) in [("water_ml", self.water_ml), ("hot_water_ml", self.hot_water_ml), ("ice_g", self.ice_g)] {
            if let Some(v) = v {
                if v < 0.0 {
                    errors.push(format!("{label} must be a non-negative number, got {}", py_num(v)));
                }
            }
        }
        // Wire-format guard (not in the reference): the pours body length is one byte.
        if errors.is_empty() {
            if let Err(e @ protocol::ProtocolError::PoursTooLong { .. }) =
                protocol::build_41(&self.wire_pours(), self.grind, 0)
            {
                errors.push(e.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(RecipeError(errors.join("; ")))
        }
    }

    /// True for a **no-grind** recipe (`grind == 0`): brew pre-ground, grinder off.
    pub fn no_grind(&self) -> bool {
        self.grind == protocol::NO_GRIND
    }

    /// Σ pour ml.
    pub fn total_water_ml(&self) -> i32 {
        self.pours.iter().map(|p| p.ml).sum()
    }

    /// The brew ratio: the explicit `ratio`, else Σ pours / dose to one decimal.
    pub fn effective_ratio(&self) -> f64 {
        if let Some(r) = self.ratio {
            return r;
        }
        if self.dose_g == 0 {
            return 0.0;
        }
        libm::round(f64::from(self.total_water_ml()) / f64::from(self.dose_g) * 10.0) / 10.0
    }

    fn wire_pours(&self) -> Vec<WirePour> {
        self.pours.iter().map(Pour::to_wire).collect()
    }

    /// The protocol-layer shape consumed by [`protocol::build_load_frames`].
    ///
    /// The ratio byte is not carried: the protocol derives it from Σ(pour ml) / dose so it
    /// always matches (the machine rejects an inconsistent one).
    pub fn to_wire(&self) -> WireRecipe {
        WireRecipe {
            dose: self.dose_g,
            grind: self.grind,
            stage_temps: (self.stage_temps[0], self.stage_temps[1]),
            pours: self.wire_pours(),
            tail: None,
            seq: LOAD_SEQ,
        }
    }

    /// A copy brewed at a different dose with the same ratio and pour shape.
    ///
    /// Pours scale proportionally (rounded, with the rounding error folded into the last
    /// pour so the total stays exactly `new_dose × ratio`). An explicit `ratio` is kept, so
    /// the scaled recipe still validates.
    pub fn scaled_to_dose(&self, new_dose: i32) -> Recipe {
        let mut out = self.clone();
        if new_dose == self.dose_g || self.dose_g <= 0 || new_dose <= 0 {
            return out;
        }
        let target = match self.ratio {
            Some(r) => round_half_even(f64::from(new_dose) * r) as i32,
            None => libm::round(f64::from(self.total_water_ml()) * f64::from(new_dose) / f64::from(self.dose_g)) as i32,
        };
        let total = f64::from(self.total_water_ml().max(1));
        let mut acc = 0;
        let last = out.pours.len().saturating_sub(1);
        for (i, p) in out.pours.iter_mut().enumerate() {
            if i == last {
                p.ml = (target - acc).max(1);
            } else {
                p.ml = (libm::round(f64::from(p.ml) * f64::from(target) / total) as i32).max(1);
                acc += p.ml;
            }
        }
        out.dose_g = new_dose;
        if let Some(w) = out.water_ml.as_mut() {
            *w = libm::round(*w * f64::from(new_dose) / f64::from(self.dose_g));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn pour(ml: i32) -> Pour {
        Pour {
            label: None,
            ml,
            temp_c: 92,
            pattern: Pattern::Spiral,
            pause_s: 5,
            rpm: 100,
            flow_ml_s: 3.0,
            agitation: false,
        }
    }

    fn recipe() -> Recipe {
        Recipe {
            name: "T".into(),
            dose_g: 16,
            grind: 62,
            ratio: Some(15.0),
            stage_temps: [110.0, 90.0],
            kind: None,
            dripper: None,
            water_ml: None,
            hot_water_ml: None,
            ice_g: None,
            time: None,
            note: None,
            pours: vec![pour(45), pour(100), pour(95)],
        }
    }

    #[test]
    fn scaling_keeps_ratio_and_validates() {
        let r = recipe();
        r.validate().unwrap();
        let s = r.scaled_to_dose(12);
        assert_eq!(s.total_water_ml(), 180);
        s.validate().unwrap();
        let up = r.scaled_to_dose(18);
        assert_eq!(up.total_water_ml(), 270);
        up.validate().unwrap();
    }

    #[test]
    fn oversize_pours_fail_validation() {
        let mut r = recipe();
        r.ratio = None;
        r.pours = vec![pour(4000), pour(4000), pour(4000)];
        assert!(r.validate().unwrap_err().0.contains("at most 255"));
    }
}
