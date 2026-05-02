//! Scan an HL buffer for items tagged with future dates and post them
//! to the user's Google calendar via `gcalcli` (when `g:calendar` is
//! configured) or write `.ics` files to the working directory.
//!
//! Mirrors hyperlist.vim's CalendarAdd() function. Date detection
//! follows hyper's three-format scanner (ISO `YYYY-MM-DD`, Nordic
//! `DD.MM.YYYY`, EU `DD/MM/YYYY`).

use std::io::Write;
use std::process::{Command, Stdio};

/// Result of a calendar add: number of events posted + per-event log.
pub struct CalendarReport {
    pub posted: usize,
    pub errors: Vec<String>,
}

pub fn add_future_events(
    text: &str,
    calendar: Option<&str>,
    alldates: bool,
) -> CalendarReport {
    let today = today_ymd();
    let mut posted = 0;
    let mut errors = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let date = match scan_date(trimmed) {
            Some(d) => d,
            None    => continue,
        };
        if !alldates && !is_future_or_today(date, today) { continue; }
        let summary = sanitize_summary(trimmed);
        let when = format!("{:04}-{:02}-{:02}", date.0, date.1, date.2);
        let res = if let Some(cal) = calendar {
            post_via_gcalcli(cal, &when, &summary)
        } else {
            write_ics_file(date, &summary)
        };
        match res {
            Ok(()) => posted += 1,
            Err(e) => errors.push(format!("{}: {}", when, e)),
        }
    }
    CalendarReport { posted, errors }
}

fn post_via_gcalcli(calendar: &str, when: &str, summary: &str) -> Result<(), String> {
    let status = Command::new("gcalcli")
        .args(["add",
            "--calendar", calendar,
            "--when", when,
            "--title", summary,
            "--noprompt"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("gcalcli spawn: {}", e))?;
    if !status.success() {
        return Err(format!("gcalcli exit code {:?}", status.code()));
    }
    Ok(())
}

fn write_ics_file((y, m, d): (i64, u32, u32), summary: &str) -> Result<(), String> {
    let safe_title: String = summary.chars().filter(|c| c.is_alphanumeric() || *c == '_').take(20).collect();
    let path = format!("{:04}{:02}{:02}-{}.ics", y, m, d,
        if safe_title.is_empty() { "event".into() } else { safe_title });
    let body = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//scribe//hyperlist//EN\r\n\
         BEGIN:VEVENT\r\nUID:scribe-{:04}{:02}{:02}-{}@local\r\n\
         DTSTART;VALUE=DATE:{:04}{:02}{:02}\r\n\
         SUMMARY:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        y, m, d, summary.chars().take(8).collect::<String>().replace(' ', "_"),
        y, m, d,
        summary.replace(',', "\\,").replace(';', "\\;"));
    let mut f = std::fs::File::create(&path).map_err(|e| format!("create {}: {}", path, e))?;
    f.write_all(body.as_bytes()).map_err(|e| format!("write {}: {}", path, e))?;
    Ok(())
}

fn sanitize_summary(line: &str) -> String {
    // Drop leading TABs / `*` / numbering, drop trailing date stamps,
    // drop the first `[…]` qualifier (typically the date itself).
    let mut s = line.trim_start_matches(|c: char| c == '\t' || c == '*' || c == ' ').to_string();
    if let Some(open) = s.find('[') {
        if let Some(close) = s[open..].find(']') {
            s.replace_range(open..=open + close, "");
            s = s.trim().to_string();
        }
    }
    s
}

fn scan_date(text: &str) -> Option<(i64, u32, u32)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 9 < bytes.len() {
        if bytes[i].is_ascii_digit() {
            if let Some(d) = try_iso_at(text, i) { return Some(d); }
            if let Some(d) = try_nordic_at(text, i) { return Some(d); }
            if let Some(d) = try_eu_at(text, i) { return Some(d); }
        }
        i += 1;
    }
    None
}

fn try_iso_at(s: &str, i: usize) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if i + 9 >= b.len() { return None; }
    let yr: String = b[i..i+4].iter().map(|&c| c as char).collect();
    if b[i+4] != b'-' || b[i+7] != b'-' { return None; }
    let mo: String = b[i+5..i+7].iter().map(|&c| c as char).collect();
    let da: String = b[i+8..i+10].iter().map(|&c| c as char).collect();
    Some((yr.parse().ok()?, mo.parse().ok()?, da.parse().ok()?))
}

fn try_nordic_at(s: &str, i: usize) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if i + 9 >= b.len() { return None; }
    if b[i+2] != b'.' || b[i+5] != b'.' { return None; }
    let da: String = b[i..i+2].iter().map(|&c| c as char).collect();
    let mo: String = b[i+3..i+5].iter().map(|&c| c as char).collect();
    let yr: String = b[i+6..i+10].iter().map(|&c| c as char).collect();
    Some((yr.parse().ok()?, mo.parse().ok()?, da.parse().ok()?))
}

fn try_eu_at(s: &str, i: usize) -> Option<(i64, u32, u32)> {
    let b = s.as_bytes();
    if i + 9 >= b.len() { return None; }
    if b[i+2] != b'/' || b[i+5] != b'/' { return None; }
    let da: String = b[i..i+2].iter().map(|&c| c as char).collect();
    let mo: String = b[i+3..i+5].iter().map(|&c| c as char).collect();
    let yr: String = b[i+6..i+10].iter().map(|&c| c as char).collect();
    Some((yr.parse().ok()?, mo.parse().ok()?, da.parse().ok()?))
}

fn today_ymd() -> (i64, u32, u32) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs / 86400;
    days_to_ymd(days)
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    let mut year = 1970i64;
    loop {
        let leap = is_leap(year);
        let dy = if leap { 366 } else { 365 };
        if days < dy { break; }
        days -= dy;
        year += 1;
    }
    while days < 0 {
        year -= 1;
        let leap = is_leap(year);
        days += if leap { 366 } else { 365 };
    }
    let dim: [u32; 12] = [31, if is_leap(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    let mut d = days as u32;
    while m < 12 && d >= dim[m] {
        d -= dim[m];
        m += 1;
    }
    (year, (m + 1) as u32, d + 1)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn is_future_or_today(d: (i64, u32, u32), today: (i64, u32, u32)) -> bool {
    d >= today
}
