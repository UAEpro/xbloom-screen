//! Timed brews: schedules ("07:00 on weekdays, Washed"), the one armed run, and when it is due.
//!
//! 🛑 Safety model. A schedule only says *when*; it never brews by itself. Each run must be
//! **armed on the screen**, at the machine, after a checklist (beans in the grinder, filter and
//! dripper, cup, water) and the hold gesture; one arming covers one run. The admin panel can
//! create, edit, delete and disarm schedules, but nothing on the network can arm one. When an
//! armed run is due, the screen counts down for [`COUNTDOWN_S`] seconds with a Cancel button,
//! then starts the brew, provided remote start is allowed and the machine is free (see
//! [`crate::App::scheduler_tick`]).
//!
//! Times are local wall-clock times. The arithmetic takes the local-time conversion as a
//! function so it can be tested without touching the process time zone.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::settings::{hhmm, parse_hhmm};
use crate::util::{new_id, read_json, write_json, LocalTime};

/// The countdown before an armed run starts, seconds.
pub const COUNTDOWN_S: u64 = 60;
/// An armed run that couldn't start within this long after its time (the screen was off, the
/// clock jumped) is dropped instead of brewing late.
pub const LATE_S: u64 = 10 * 60;
/// Most schedules kept.
pub const MAX_SCHEDULES: usize = 20;
/// Timestamps before this mean the clock isn't set yet (an ESP32 before SNTP): no timers.
pub const CLOCK_SET_AFTER: u64 = 1_700_000_000;

/// When a recipe should brew.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    /// Stable id.
    pub id: String,
    /// Library recipe to brew.
    pub recipe_id: String,
    /// Local time, `HH:MM`.
    pub time: String,
    /// Monday … Sunday. None set = once, at the next `time` after it was saved.
    pub days: [bool; 7],
    /// Off = kept but never due.
    pub enabled: bool,
    /// For a one-off schedule: the run it was saved for (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub once_at: Option<u64>,
}

impl Schedule {
    /// A one-off schedule (no weekdays).
    pub fn once(&self) -> bool {
        !self.days.iter().any(|&d| d)
    }
}

/// What the admin panel / screen send to create or change a schedule.
#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleInput {
    /// Library recipe.
    pub recipe_id: String,
    /// `HH:MM`.
    pub time: String,
    /// Monday … Sunday (none = once).
    #[serde(default)]
    pub days: [bool; 7],
    /// Defaults to on.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// The run that is armed: one schedule, one time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Armed {
    /// Its schedule.
    pub schedule_id: String,
    /// When it brews, unix seconds.
    pub at: u64,
    /// When it was armed.
    pub armed_at: u64,
}

/// A due-or-upcoming run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    /// Its schedule.
    pub schedule_id: String,
    /// The recipe.
    pub recipe_id: String,
    /// When, unix seconds.
    pub at: u64,
}

/// A run as the screen and the admin panel show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimerRun {
    /// Its schedule.
    pub schedule_id: String,
    /// The recipe.
    pub recipe_id: String,
    /// The recipe's name.
    pub recipe_name: String,
    /// When, unix seconds.
    pub at: u64,
    /// Armed at the machine.
    pub armed: bool,
}

/// Where timed brews stand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct TimerStatus {
    /// The armed run, or else the next one.
    pub next: Option<TimerRun>,
    /// Seconds until the armed run starts, while its countdown is on.
    pub countdown_s: Option<u64>,
    /// The clock is set (an ESP32 waits for its first time sync).
    pub clock_set: bool,
    /// Remote start is allowed; timers need it.
    pub remote_start: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default)]
    schedules: Vec<Schedule>,
    #[serde(default)]
    armed: Option<Armed>,
}

/// The schedules and the armed run, persisted in `timers.json`.
#[derive(Debug)]
pub struct ScheduleStore {
    path: PathBuf,
    schedules: Vec<Schedule>,
    armed: Option<Armed>,
}

impl ScheduleStore {
    /// Open (or start) the store at `path`.
    pub fn open(path: PathBuf) -> Self {
        let f: File = read_json(&path).ok().flatten().unwrap_or_default();
        Self { path, schedules: f.schedules, armed: f.armed }
    }

    fn save(&self) {
        let f = File { schedules: self.schedules.clone(), armed: self.armed.clone() };
        if let Err(e) = write_json(&self.path, &f) {
            log::error!("schedules: save failed: {e}");
        }
    }

    /// All schedules, in the order they were made.
    pub fn list(&self) -> Vec<Schedule> {
        self.schedules.clone()
    }

    /// One schedule.
    pub fn get(&self, id: &str) -> Option<Schedule> {
        self.schedules.iter().find(|s| s.id == id).cloned()
    }

    /// The armed run, if any.
    pub fn armed(&self) -> Option<Armed> {
        self.armed.clone()
    }

    /// Add a schedule. `now` and `local` place a one-off schedule's run.
    pub fn add(
        &mut self,
        input: ScheduleInput,
        now: u64,
        local: &dyn Fn(i64) -> LocalTime,
    ) -> Result<Schedule, String> {
        if self.schedules.len() >= MAX_SCHEDULES {
            return Err(format!("at most {MAX_SCHEDULES} timers"));
        }
        let s = build(new_id(), input, now, local)?;
        self.schedules.push(s.clone());
        self.save();
        Ok(s)
    }

    /// Replace a schedule. Changing an armed one disarms it (its time may have moved).
    /// Returns the schedule and whether a run was disarmed.
    pub fn update(
        &mut self,
        id: &str,
        input: ScheduleInput,
        now: u64,
        local: &dyn Fn(i64) -> LocalTime,
    ) -> Result<(Schedule, bool), String> {
        let pos = self.schedules.iter().position(|s| s.id == id).ok_or("no such timer")?;
        let s = build(id.to_string(), input, now, local)?;
        let changed = self.schedules[pos] != s;
        let disarmed = self.disarm_if(|a| a.schedule_id == id && changed);
        self.schedules[pos] = s.clone();
        self.save();
        Ok((s, disarmed))
    }

    /// Delete a schedule (disarming it). Returns whether it existed.
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.schedules.len();
        self.schedules.retain(|s| s.id != id);
        self.disarm_if(|a| a.schedule_id == id);
        let removed = self.schedules.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// Disarm the armed run if it brews `recipe_id` (the recipe was edited after arming).
    pub fn disarm_recipe(&mut self, recipe_id: &str) -> bool {
        let ids: Vec<&str> =
            self.schedules.iter().filter(|s| s.recipe_id == recipe_id).map(|s| s.id.as_str()).collect();
        let hit = self.armed.as_ref().is_some_and(|a| ids.contains(&a.schedule_id.as_str()));
        if hit {
            self.disarm();
        }
        hit
    }

    /// Delete every schedule that brews `recipe_id` (the recipe was deleted).
    pub fn forget_recipe(&mut self, recipe_id: &str) -> bool {
        let gone: Vec<String> =
            self.schedules.iter().filter(|s| s.recipe_id == recipe_id).map(|s| s.id.clone()).collect();
        for id in &gone {
            self.delete(id);
        }
        !gone.is_empty()
    }

    /// Switch a one-off schedule off once its run has passed.
    pub fn finish_once(&mut self, id: &str) {
        if let Some(s) = self.schedules.iter_mut().find(|s| s.id == id && s.once()) {
            s.enabled = false;
            self.save();
        }
    }

    /// Switch off one-off schedules whose run is more than [`LATE_S`] in the past (they were
    /// never armed, or the screen was off). Returns whether anything changed.
    pub fn expire_once(&mut self, now: u64) -> bool {
        let mut changed = false;
        for s in self.schedules.iter_mut().filter(|s| s.enabled && s.once()) {
            if s.once_at.is_none_or(|at| at + LATE_S < now) {
                s.enabled = false;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
        changed
    }

    /// Arm `run`.
    pub fn arm(&mut self, run: &Run, now: u64) {
        self.armed = Some(Armed { schedule_id: run.schedule_id.clone(), at: run.at, armed_at: now });
        self.save();
    }

    /// Disarm. Returns the run that was armed.
    pub fn disarm(&mut self) -> Option<Armed> {
        let a = self.armed.take();
        if a.is_some() {
            self.save();
        }
        a
    }

    fn disarm_if(&mut self, f: impl Fn(&Armed) -> bool) -> bool {
        if self.armed.as_ref().is_some_and(f) {
            self.disarm();
            true
        } else {
            false
        }
    }

    /// The next run at or after `now` across the enabled schedules.
    pub fn next_run(&self, now: u64, local: &dyn Fn(i64) -> LocalTime) -> Option<Run> {
        self.schedules
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| {
                let at = if s.once() { s.once_at.filter(|&at| at >= now)? } else { next_occurrence(s, now, local)? };
                Some(Run { schedule_id: s.id.clone(), recipe_id: s.recipe_id.clone(), at })
            })
            .min_by_key(|r| r.at)
    }

    /// Replace everything (restore from a backup). An armed run isn't restored.
    pub fn replace_all(&mut self, schedules: Vec<Schedule>) {
        self.schedules = schedules.into_iter().take(MAX_SCHEDULES).collect();
        self.armed = None;
        self.save();
    }
}

fn build(id: String, input: ScheduleInput, now: u64, local: &dyn Fn(i64) -> LocalTime) -> Result<Schedule, String> {
    let minute = parse_hhmm(&input.time).ok_or("the time must be HH:MM, 00:00–23:59")?;
    if input.recipe_id.trim().is_empty() {
        return Err("pick a recipe".into());
    }
    let mut s = Schedule {
        id,
        recipe_id: input.recipe_id,
        time: hhmm(minute),
        days: input.days,
        enabled: input.enabled,
        once_at: None,
    };
    if s.once() {
        s.once_at = next_occurrence(&s, now, local);
    }
    Ok(s)
}

/// The first time at or after `from` (unix seconds) that `s` runs, ignoring `enabled`.
///
/// Days are walked in local time; on a daylight-saving change day the wall clock is nudged
/// back onto the asked time (a time that doesn't exist that day, like 02:30 in spring, lands
/// an hour later).
pub fn next_occurrence(s: &Schedule, from: u64, local: &dyn Fn(i64) -> LocalTime) -> Option<u64> {
    let want = i64::from(parse_hhmm(&s.time)?);
    let from = from as i64;
    let lt = local(from);
    let midnight = from - i64::from(lt.hour * 3600 + lt.minute * 60 + lt.second);
    for day in 0..9 {
        let mut t = midnight + day * 86_400 + want * 60;
        let l = local(t);
        let got = i64::from(l.hour * 60 + l.minute);
        let mut diff = want - got;
        if diff > 720 {
            diff -= 1440;
        } else if diff < -720 {
            diff += 1440;
        }
        t += diff * 60;
        if t < from {
            continue;
        }
        // Monday = 0.
        let weekday = (local(t).weekday + 6) % 7;
        if !s.once() && !s.days[weekday as usize] {
            continue;
        }
        return Some(t as u64);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake zone: UTC + `offset` hours, with no daylight saving.
    fn zone(offset: i64) -> impl Fn(i64) -> LocalTime {
        move |ts: i64| {
            let t = ts + offset * 3600;
            let days = t.div_euclid(86_400);
            let secs = t.rem_euclid(86_400);
            LocalTime {
                hour: (secs / 3600) as u32,
                minute: (secs / 60 % 60) as u32,
                second: (secs % 60) as u32,
                // 1970-01-01 was a Thursday.
                weekday: ((days + 4).rem_euclid(7)) as u32,
                day: 1,
                month: 0,
                year: 1970,
            }
        }
    }

    // Monday 2026-10-19 00:00 UTC.
    const MONDAY: u64 = 1_792_368_000;

    fn input(time: &str, days: [bool; 7]) -> ScheduleInput {
        ScheduleInput { recipe_id: "r1".into(), time: time.into(), days, enabled: true }
    }

    const WEEKDAYS: [bool; 7] = [true, true, true, true, true, false, false];

    #[test]
    fn monday_is_a_monday() {
        assert_eq!(zone(0)(MONDAY as i64).weekday, 1);
    }

    #[test]
    fn weekday_schedules_skip_the_weekend() {
        let utc = zone(0);
        let s = build("a".into(), input("07:00", WEEKDAYS), MONDAY, &utc).unwrap();
        // Monday 06:00 → Monday 07:00.
        assert_eq!(next_occurrence(&s, MONDAY + 6 * 3600, &utc), Some(MONDAY + 7 * 3600));
        // Monday 07:00 exactly is still due; a second later it's Tuesday.
        assert_eq!(next_occurrence(&s, MONDAY + 7 * 3600, &utc), Some(MONDAY + 7 * 3600));
        assert_eq!(next_occurrence(&s, MONDAY + 7 * 3600 + 1, &utc), Some(MONDAY + 86_400 + 7 * 3600));
        // Friday 08:00 → next Monday 07:00.
        assert_eq!(next_occurrence(&s, MONDAY + 4 * 86_400 + 8 * 3600, &utc), Some(MONDAY + 7 * 86_400 + 7 * 3600));
    }

    #[test]
    fn times_are_local() {
        // UTC+4: 07:00 local is 03:00 UTC.
        let gst = zone(4);
        let s = build("a".into(), input("07:00", [true; 7]), MONDAY, &gst).unwrap();
        assert_eq!(next_occurrence(&s, MONDAY, &gst), Some(MONDAY + 3 * 3600));
    }

    #[test]
    fn a_daylight_saving_day_keeps_the_wall_clock_time() {
        // A zone that is UTC+1 before `change` and UTC+2 after (spring forward at 01:00 UTC).
        let change = (MONDAY + 86_400 + 3600) as i64;
        let local = move |ts: i64| zone(if ts < change { 1 } else { 2 })(ts);
        let s = build("a".into(), input("07:00", [true; 7]), MONDAY, &local).unwrap();
        // Tuesday 07:00 local is 05:00 UTC (UTC+2), not 06:00.
        let tue = next_occurrence(&s, MONDAY + 12 * 3600, &local).unwrap();
        assert_eq!(tue, MONDAY + 86_400 + 5 * 3600);
        assert_eq!((local(tue as i64).hour, local(tue as i64).minute), (7, 0));
    }

    #[test]
    fn one_off_schedules_run_once() {
        let utc = zone(0);
        let dir = tempfile::tempdir().unwrap();
        let mut store = ScheduleStore::open(dir.path().join("s.json"));
        // Made Monday 22:00 for 07:00: Tuesday 07:00.
        let s = store.add(input("7:00", [false; 7]), MONDAY + 22 * 3600, &utc).unwrap();
        let tue7 = MONDAY + 86_400 + 7 * 3600;
        assert_eq!((s.time.as_str(), s.once_at), ("07:00", Some(tue7)));
        assert_eq!(store.next_run(MONDAY + 23 * 3600, &utc).map(|r| r.at), Some(tue7));
        // Passed by more than LATE_S without brewing: switched off, and not due again Wednesday.
        assert!(!store.expire_once(tue7 + 60));
        assert!(store.expire_once(tue7 + LATE_S + 1));
        assert_eq!(store.next_run(tue7 + LATE_S + 1, &utc), None);
    }

    #[test]
    fn the_soonest_enabled_run_is_next() {
        let utc = zone(0);
        let dir = tempfile::tempdir().unwrap();
        let mut store = ScheduleStore::open(dir.path().join("s.json"));
        let weekdays = store.add(input("07:00", WEEKDAYS), MONDAY, &utc).unwrap();
        let mut sat = input("09:30", [false, false, false, false, false, true, false]);
        sat.recipe_id = "r2".into();
        let saturday = store.add(sat, MONDAY, &utc).unwrap();
        // Friday noon: Saturday 09:30 comes before Monday 07:00.
        let fri = MONDAY + 4 * 86_400 + 12 * 3600;
        let next = store.next_run(fri, &utc).unwrap();
        assert_eq!((next.schedule_id.as_str(), next.recipe_id.as_str()), (saturday.id.as_str(), "r2"));
        assert_eq!(next.at, MONDAY + 5 * 86_400 + 9 * 3600 + 1800);
        // Switched off, Saturday's is skipped.
        let mut off = input("09:30", saturday.days);
        off.enabled = false;
        store.update(&saturday.id, off, fri, &utc).unwrap();
        assert_eq!(store.next_run(fri, &utc).unwrap().schedule_id, weekdays.id);
    }

    #[test]
    fn arming_survives_a_restart_and_editing_disarms() {
        let utc = zone(0);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let mut store = ScheduleStore::open(p.clone());
        let s = store.add(input("07:00", WEEKDAYS), MONDAY, &utc).unwrap();
        let run = store.next_run(MONDAY, &utc).unwrap();
        store.arm(&run, MONDAY);
        let mut store = ScheduleStore::open(p.clone());
        assert_eq!(store.armed().map(|a| a.at), Some(run.at));
        // Saving it unchanged keeps the arming; moving the time drops it.
        assert!(!store.update(&s.id, input("07:00", WEEKDAYS), MONDAY, &utc).unwrap().1);
        assert!(store.update(&s.id, input("07:15", WEEKDAYS), MONDAY, &utc).unwrap().1);
        assert_eq!(store.armed(), None);
        // Deleting the recipe deletes its timers.
        store.arm(&store.next_run(MONDAY, &utc).unwrap(), MONDAY);
        assert!(store.forget_recipe("r1"));
        assert_eq!((store.list().len(), store.armed()), (0, None));
    }

    #[test]
    fn bad_input_is_refused() {
        let utc = zone(0);
        assert!(build("a".into(), input("24:00", WEEKDAYS), MONDAY, &utc).is_err());
        let mut no_recipe = input("07:00", WEEKDAYS);
        no_recipe.recipe_id = " ".into();
        assert!(build("a".into(), no_recipe, MONDAY, &utc).is_err());
    }
}
