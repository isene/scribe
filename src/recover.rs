//! Unsaved text kept on disk, so a crash or a dead battery costs a few
//! seconds of typing and no more.
//!
//! While a buffer has changes that are not saved, its text is written to
//! `~/.config/scribe/recover/` each time the typing pauses, and removed
//! again on a save or a quit. A file found there at start is text that
//! never got saved: scribe offers it back.
//!
//! The files are named after the file they belong to and never sit next
//! to it, so a synced folder of notes does not grow a second copy of a
//! note on every device.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Unsaved text nobody came back for is removed after this long.
const KEEP: Duration = Duration::from_secs(30 * 86400);

pub fn dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".config/scribe/recover")
}

/// The recovery file of a buffer. It is named after the buffer's file:
/// the file's own name, then a number made from its whole path, so two
/// `notes.md` in two folders do not share one. A buffer with no file is
/// named after this process.
pub fn path_for(file: Option<&Path>) -> PathBuf {
    let Some(file) = file else {
        return dir().join(format!("unnamed.{}", std::process::id()));
    };
    let whole = std::fs::canonicalize(file)
        .or_else(|_| std::path::absolute(file))
        .unwrap_or_else(|_| file.to_path_buf());
    // FNV-1a over the path: the same on every run, which the hasher in
    // std does not promise.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in whole.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let name: String = whole.file_name()
        .map(|n| n.to_string_lossy().chars().take(60).collect())
        .unwrap_or_default();
    dir().join(format!("{name}.{hash:016x}"))
}

/// Write `text` so that a power cut leaves the old file or the new one,
/// never half of one. Only the user can read it.
pub fn write(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true).create(true).truncate(true).mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_data()?;
    std::fs::rename(&tmp, path)
}

/// The text of a scribe that had no file and is gone: the newest one.
pub fn newest_unnamed() -> Option<PathBuf> {
    let mine = format!("unnamed.{}", std::process::id());
    std::fs::read_dir(dir()).ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("unnamed.") && !name.ends_with(".tmp") && *name != mine
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(when, _)| *when)
        .map(|(_, path)| path)
}

/// Remove the files that have waited longer than `KEEP`.
pub fn sweep() {
    let Ok(entries) = std::fs::read_dir(dir()) else { return };
    for entry in entries.flatten() {
        let old = entry.metadata().ok()
            .and_then(|m| m.modified().ok())
            .and_then(|when| when.elapsed().ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// How long ago a file was written, in the words of a status line:
/// "12 minutes", "3 hours", "2 days".
pub fn age(path: &Path) -> String {
    let secs = std::fs::metadata(path).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|when| SystemTime::now().duration_since(when).ok())
        .map_or(0, |d| d.as_secs());
    age_words(secs)
}

fn age_words(secs: u64) -> String {
    let (count, unit) = match secs {
        0..=89 => return "a minute".into(),
        90..=5399 => ((secs + 30) / 60, "minutes"),
        5400..=129_599 => ((secs + 1800) / 3600, "hours"),
        _ => ((secs + 43_200) / 86_400, "days"),
    };
    format!("{count} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_keeps_its_recovery_name_and_two_files_do_not_share_one() {
        let a = path_for(Some(Path::new("/tmp/scribe-recover-test/a/notes.md")));
        let b = path_for(Some(Path::new("/tmp/scribe-recover-test/b/notes.md")));
        assert_eq!(a, path_for(Some(Path::new("/tmp/scribe-recover-test/a/notes.md"))));
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("notes.md."), "{name}");
        assert_eq!(a.parent(), Some(dir().as_path()));
    }

    #[test]
    fn a_write_replaces_the_file_whole_and_only_the_user_reads_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("scribe-recover-{}", std::process::id()));
        let path = dir.join("draft.txt.0123");
        write(&path, "first").unwrap();
        write(&path, "second, longer").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second, longer");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        // No half-written file is left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_age_reads_as_minutes_hours_or_days() {
        assert_eq!(age_words(20), "a minute");
        assert_eq!(age_words(12 * 60), "12 minutes");
        assert_eq!(age_words(3 * 3600 + 100), "3 hours");
        assert_eq!(age_words(2 * 86_400), "2 days");
    }
}
