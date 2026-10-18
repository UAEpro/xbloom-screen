//! Local wall-clock time via the C library (`localtime_r`).
//!
//! Works on both targets: glibc reads the system zone on the Pi; on the ESP32 the firmware sets
//! the POSIX `TZ` variable from the settings and SNTP sets the clock.

/// A broken-down local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// 0–23.
    pub hour: u32,
    /// 0–59.
    pub minute: u32,
    /// 0 = Sunday.
    pub weekday: u32,
    /// 1–31.
    pub day: u32,
    /// 0 = January.
    pub month: u32,
    /// e.g. 2026.
    pub year: i32,
}

extern "C" {
    fn tzset();
}

/// Re-read the time zone (call after changing `TZ`).
pub fn reload_zone() {
    // SAFETY: tzset only reads the TZ environment variable / zone files.
    unsafe { tzset() }
}

/// Local time for a Unix timestamp.
pub fn local(ts: i64) -> LocalTime {
    let t = ts as libc::time_t;
    // SAFETY: `tm` is plain data; localtime_r writes it and doesn't retain the pointers.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    LocalTime {
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        weekday: tm.tm_wday as u32,
        day: tm.tm_mday as u32,
        month: tm.tm_mon as u32,
        year: tm.tm_year + 1900,
    }
}

/// Local time now.
pub fn now() -> LocalTime {
    local(xbloom_app::util::now_unix() as i64)
}

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// "08:42" or "8:42 AM".
pub fn time_text(t: LocalTime, h24: bool) -> String {
    if h24 {
        format!("{:02}:{:02}", t.hour, t.minute)
    } else {
        let h = match t.hour % 12 {
            0 => 12,
            h => h,
        };
        format!("{h}:{:02} {}", t.minute, if t.hour < 12 { "AM" } else { "PM" })
    }
}

/// "Saturday 17 October".
pub fn date_text(t: LocalTime) -> String {
    format!("{} {} {}", DAYS[t.weekday as usize % 7], t.day, MONTHS[t.month as usize % 12])
}

/// "Sat 17 Oct, 08:42" (history rows).
pub fn short_text(t: LocalTime, h24: bool) -> String {
    format!(
        "{} {} {}, {}",
        &DAYS[t.weekday as usize % 7][..3],
        t.day,
        &MONTHS[t.month as usize % 12][..3],
        time_text(t, h24)
    )
}

/// "Good morning" / "Good afternoon" / "Good evening".
pub fn greeting(t: LocalTime) -> &'static str {
    match t.hour {
        5..=11 => "Good morning",
        12..=16 => "Good afternoon",
        _ => "Good evening",
    }
}

/// Night hours for the automatic dark theme (19:00–07:00).
pub fn is_night(t: LocalTime) -> bool {
    t.hour >= 19 || t.hour < 7
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(hour: u32, minute: u32) -> LocalTime {
        LocalTime { hour, minute, weekday: 6, day: 17, month: 9, year: 2026 }
    }

    #[test]
    fn formatting() {
        assert_eq!(time_text(t(8, 5), true), "08:05");
        assert_eq!(time_text(t(0, 5), false), "12:05 AM");
        assert_eq!(time_text(t(13, 30), false), "1:30 PM");
        assert_eq!(date_text(t(8, 0)), "Saturday 17 October");
        assert_eq!(short_text(t(8, 0), true), "Sat 17 Oct, 08:00");
        assert_eq!(greeting(t(9, 0)), "Good morning");
        assert!(is_night(t(22, 0)) && !is_night(t(12, 0)));
    }

    #[test]
    fn localtime_works() {
        let lt = local(0);
        assert!(lt.year == 1970 || lt.year == 1969);
    }
}
