//! GitHub issue polling (TRG-1): same events as the webhook, cursor on `updated_at` (read-only).

use std::process::Command;

use serde_json::{json, Value};
use sf_core::{Store, Timestamp};
use sf_intake::{github, ConnectorError, Intake};

/// `2024-05-06T07:08:09Z` -> epoch milliseconds.
pub fn parse_iso(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 24 + hh) * 60 + mm) * 60 * 1000 + ss * 1000).ok()
}

fn safe(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Open issues carrying `label`, via `gh api` (no shell, read-only GET).
pub fn fetch(repo: &str, label: &str) -> Result<Value, String> {
    if !safe(repo, "-_./") || repo.matches('/').count() != 1 || !safe(label, "-_.:") {
        return Err("repo must be org/repo and label plain characters".into());
    }
    let label = label.replace(':', "%3A");
    let path = format!("repos/{repo}/issues?state=open&labels={label}&sort=updated&direction=asc&per_page=100");
    let out = Command::new("gh").args(["api", "--method", "GET", &path]).output().map_err(|e| format!("gh: {e}"))?;
    if !out.status.success() {
        return Err(format!("gh api failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("gh api output: {e}"))
}

/// Feeds issues updated after the `github-poll:{repo}` cursor to `Intake::accept`, then advances it.
pub fn poll(store: &dyn Store, intake: &Intake, repo: &str, issues: &Value, now: Timestamp) -> Result<Value, String> {
    let name = format!("github-poll:{repo}");
    let cursor = store.get_cursor(&name).map_err(|e| e.to_string())?.unwrap_or(0);
    let list = issues.as_array().ok_or("issues must be a JSON array")?;
    let (mut newest, mut seen, mut skipped) = (cursor, 0, 0);
    let mut accepted = Vec::new();
    for issue in list {
        let updated = issue.get("updated_at").and_then(Value::as_str).and_then(parse_iso).unwrap_or(0);
        if updated <= cursor {
            continue;
        }
        seen += 1;
        newest = newest.max(updated);
        match github::from_issue(repo, issue, now) {
            Ok(ev) => {
                let o = intake.accept(&ev).map_err(|e| e.to_string())?;
                accepted.push(json!({"item_id": o.item.item_id, "status": o.status, "inserted": o.inserted}));
            }
            Err(ConnectorError::Unsupported(_)) => skipped += 1,
            Err(e) => return Err(e.to_string()),
        }
    }
    store.set_cursor(&name, newest).map_err(|e| e.to_string())?;
    Ok(json!({"repo": repo, "considered": seen, "skipped": skipped, "items": accepted, "cursor": newest}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso_timestamps() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2024-03-01T00:00:01Z"), Some(1_709_251_201_000));
        assert_eq!(parse_iso("2024-02-29T12:00:00Z"), Some(1_709_208_000_000));
        assert_eq!(parse_iso("nope"), None);
    }
}
