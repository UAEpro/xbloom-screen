//! Small shared helpers: time, ids, atomic file writes.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch (0 if the clock isn't set yet, e.g. an ESP32 before SNTP).
pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `n` random bytes as lower-case hex (OS / hardware RNG).
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("system RNG unavailable");
    xbloom_core::driver::hex(&buf)
}

/// A short random id for recipes, history entries, sessions.
pub fn new_id() -> String {
    random_hex(6)
}

/// Write `bytes` to `path` atomically: write a sibling temp file, flush, then rename over.
///
/// A power cut mid-write (common on a kitchen appliance) leaves either the old or the new
/// file, never a truncated one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    replace_file(&tmp, path)
}

/// Move `from` over `to`, replacing it.
///
/// POSIX filesystems do that atomically. FAT (the ESP32's data partition) refuses to rename
/// onto an existing name, so there the old file is removed first; a power cut in that gap
/// leaves only the complete `from`, which [`read_json`] picks up again.
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if to.exists() => {
            fs::remove_file(to)?;
            fs::rename(from, to)
        }
        Err(e) => Err(e),
    }
}

/// Serialize `value` as pretty JSON and write it atomically.
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

/// Read JSON from `path`; `Ok(None)` if the file doesn't exist.
///
/// If the file is missing but a complete temp file from an interrupted [`write_atomic`] is
/// there, that one is recovered.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let tmp = path.with_extension("tmp");
            let Ok(bytes) = fs::read(&tmp) else { return Ok(None) };
            match serde_json::from_slice(&bytes) {
                Ok(v) => {
                    let _ = fs::rename(&tmp, path);
                    Ok(Some(v))
                }
                // A temp file cut short mid-write: there's nothing to recover.
                Err(_) => Ok(None),
            }
        }
        Err(e) => Err(e),
    }
}

/// A filesystem-safe slug ("Ethiopia Natural!" → "ethiopia-natural").
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("recipe");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Ethiopia Natural!"), "ethiopia-natural");
        assert_eq!(slug("  ☕  "), "recipe");
    }

    #[test]
    fn atomic_write_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b.json");
        write_json(&p, &vec![1, 2, 3]).unwrap();
        assert_eq!(read_json::<Vec<i32>>(&p).unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(read_json::<Vec<i32>>(&dir.path().join("missing.json")).unwrap(), None);
    }

    #[test]
    fn interrupted_replace_is_recovered() {
        // The FAT path: the old file was removed, then power went before the rename.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(p.with_extension("tmp"), b"[4, 5]").unwrap();
        assert_eq!(read_json::<Vec<i32>>(&p).unwrap(), Some(vec![4, 5]));
        assert!(p.is_file());
        // A temp file cut short is ignored.
        let q = dir.path().join("history.json");
        fs::write(q.with_extension("tmp"), b"[4, ").unwrap();
        assert_eq!(read_json::<Vec<i32>>(&q).unwrap(), None);
    }
}
