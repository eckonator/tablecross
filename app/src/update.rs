//! Looking for a newer TableCross on GitHub, only if the user agreed.
//!
//! The only thing sent is the request for the newest release of the project. It goes through `curl`
//! (present on macOS and Windows 10+; on Linux `wget` where curl is not installed, as on a plain
//! Debian desktop), so the app needs no web client of its own. Errors (offline, no release) stay
//! silent unless the check was asked for.
//!
//! Every push to main rebuilds the test build without changing the version number. So for test
//! builds a release is also new if it was built from another, younger commit than this program.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

use eframe::egui;

/// All releases, the test build among them. GitHub lists them neither by version nor by date, and its
/// "latest" release is the one published last, which is an older version when two versions were
/// built together. So the highest version is picked from the list.
const RELEASES_URL: &str = "https://api.github.com/repos/eckonator/tablecross/releases?per_page=30";

/// Built from a release tag (not a test build from main).
pub fn is_release() -> bool {
    env!("TC_CHANNEL") == "release"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewVersion {
    pub version: String,
    /// Release page to download it from.
    pub url: String,
    /// The file for this system, if the app can install it itself.
    pub asset: Option<String>,
}

/// Why a check found nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckError {
    /// Offline, GitHub down or an answer that makes no sense.
    Unreachable,
    /// Neither curl nor wget is installed.
    NoTool,
}

/// Starts the check in the background; the result comes through the receiver (`Ok(None)`: nothing
/// new).
pub fn check_in_background(ctx: egui::Context) -> Receiver<Result<Option<NewVersion>, CheckError>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("tablecross-update".into())
        .spawn(move || {
            let result = check();
            match &result {
                Ok(Some(v)) => tracing::info!("newer version available: {} ({})", v.version, v.url),
                Err(CheckError::NoTool) => tracing::warn!("update check: neither curl nor wget is installed"),
                _ => {}
            }
            let _ = tx.send(result);
            ctx.request_repaint();
        })
        .ok();
    rx
}

fn command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        // No console window flashing up
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

/// Fetches `url` with curl, or with wget where curl is missing. With `to` the answer goes into that
/// file, else it is returned. `accept`: the Accept header; `seconds`: the longest it may take.
pub fn fetch(url: &str, accept: Option<&str>, seconds: u32, to: Option<&Path>) -> Result<Vec<u8>, CheckError> {
    let agent = "TableCross-update";
    let mut curl = command("curl");
    curl.args(["-fsSL", "--max-time", &seconds.to_string(), "-A", agent]);
    if let Some(accept) = accept {
        curl.args(["-H", &format!("Accept: {accept}")]);
    }
    if let Some(to) = to {
        // A download takes long enough for a weak Wi-Fi to drop out on the way
        curl.args(["--retry", "3"]).arg("-o").arg(to);
    }
    curl.arg(url);
    let out = match curl.output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut wget = command("wget");
            wget.args(["-q", "-T", &seconds.to_string(), "-U", agent]);
            if let Some(accept) = accept {
                wget.arg(format!("--header=Accept: {accept}"));
            }
            match to {
                Some(to) => wget.arg("-O").arg(to),
                None => wget.args(["-O", "-"]),
            };
            wget.arg(url);
            match wget.output() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(CheckError::NoTool),
                other => other,
            }
        }
        other => other,
    };
    match out {
        Ok(out) if out.status.success() => Ok(out.stdout),
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            tracing::warn!("fetching {url} failed ({}): {}", out.status, err.trim());
            Err(CheckError::Unreachable)
        }
        Err(e) => {
            tracing::warn!("fetching {url} failed: {e}");
            Err(CheckError::Unreachable)
        }
    }
}

fn check() -> Result<Option<NewVersion>, CheckError> {
    // TC_UPDATE_URL (debug builds only): another source, e.g. a local JSON file for testing
    let url = std::env::var("TC_UPDATE_URL")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .unwrap_or_else(|| RELEASES_URL.into());
    let body = fetch(&url, Some("application/vnd.github+json"), 8, None)?;
    let json: serde_json::Value = serde_json::from_slice(&body).map_err(|_| CheckError::Unreachable)?;
    let releases = json.as_array().cloned().unwrap_or_else(|| vec![json.clone()]);
    let suffix = crate::self_update::asset_suffix();
    let version = env!("CARGO_PKG_VERSION");
    // A higher version first; a test build also takes a younger test build of the same version
    let higher = highest(&releases).and_then(|r| newer_release(r, version, "", "", suffix));
    Ok(higher.or_else(|| {
        if is_release() {
            return None;
        }
        newest(&releases).and_then(|r| newer_release(r, version, env!("TC_GIT_HASH"), env!("TC_GIT_TIME"), suffix))
    }))
}

/// The release with the highest version ("v1.2.3" tags; the test build and drafts do not count).
fn highest(releases: &[serde_json::Value]) -> Option<&serde_json::Value> {
    releases
        .iter()
        .filter(|r| !r.get("draft").and_then(|d| d.as_bool()).unwrap_or(false))
        .filter_map(|r| {
            let tag = r.get("tag_name")?.as_str()?.strip_prefix('v')?;
            tag.split('.').all(|p| p.parse::<u64>().is_ok()).then_some((tag, r))
        })
        .reduce(|a, b| if newer(b.0, a.0) { b } else { a })
        .map(|(_, r)| r)
}

/// The release published last ("2026-…" times sort as text).
fn newest(releases: &[serde_json::Value]) -> Option<&serde_json::Value> {
    releases
        .iter()
        .max_by_key(|r| r.get("published_at").and_then(|p| p.as_str()).unwrap_or_default())
}

/// Compares a release (GitHub API object) with this build.
fn newer_release(
    release: &serde_json::Value,
    version: &str,
    commit: &str,
    commit_time: &str,
    suffix: Option<&str>,
) -> Option<NewVersion> {
    let tag = release.get("tag_name")?.as_str()?;
    let page = release.get("html_url")?.as_str()?.to_string();
    let latest = tag.trim_start_matches('v');
    let asset = suffix.and_then(|suffix| {
        release
            .get("assets")?
            .as_array()?
            .iter()
            .filter_map(|a| a.get("browser_download_url")?.as_str())
            .find(|u| u.ends_with(suffix))
            .map(str::to_string)
    });
    let found = NewVersion {
        version: latest.to_string(),
        url: page,
        asset,
    };
    if newer(latest, version) {
        return Some(found);
    }
    if latest != version || commit.is_empty() {
        return None;
    }
    // Same version: newer if built from another commit published after ours
    let release_commit = release.get("target_commitish")?.as_str()?;
    let published = release.get("published_at")?.as_str().and_then(unix_time)?;
    let ours: i64 = commit_time.parse().ok()?;
    (!release_commit.starts_with(commit) && published > ours).then_some(found)
}

/// Is `a` newer than `b`? Compares "1.2.3" part by part as numbers.
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    parse(a) > parse(b)
}

/// "2026-10-17T10:52:11Z" as seconds since 1970.
fn unix_time(text: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Days since 1970 for a date in the Gregorian calendar
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(!newer("0.1.2", "0.1.2"));
        assert!(!newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn reads_github_times() {
        assert_eq!(unix_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_time("2026-10-17T10:52:11Z"), Some(1_792_234_331));
        assert_eq!(unix_time("nonsense"), None);
    }

    #[test]
    fn same_version_counts_when_built_later_from_another_commit() {
        let release = serde_json::json!({
            "tag_name": "v0.0.1",
            "html_url": "https://example/releases/v0.0.1",
            "target_commitish": "abcdef1234",
            "published_at": "2026-10-17T12:00:00Z",
        });
        let noon = unix_time("2026-10-17T12:00:00Z").unwrap();
        let earlier = (noon - 3600).to_string();
        let later = (noon + 3600).to_string();
        assert!(newer_release(&release, "0.0.1", "1111111", &earlier, None).is_some());
        // Our own build, or a local build from a younger commit: nothing new
        assert!(newer_release(&release, "0.0.1", "abcdef1", &earlier, None).is_none());
        assert!(newer_release(&release, "0.0.1", "1111111", &later, None).is_none());
        // Without build information only the version counts
        assert!(newer_release(&release, "0.0.1", "", "", None).is_none());
        assert!(newer_release(&release, "0.0.0", "", "", None).is_some());
        assert!(newer_release(&release, "0.0.2", "1111111", &earlier, None).is_none());
    }

    #[test]
    fn the_newest_is_found_wherever_it_is_listed() {
        let list = serde_json::json!([
            {"tag_name": "v0.1.2", "published_at": "2026-10-17T16:25:09Z"},
            {"tag_name": "v0.1.1", "published_at": "2026-10-17T16:11:12Z"},
            {"tag_name": "testing", "published_at": "2026-10-17T17:44:53Z"},
        ]);
        let found = newest(list.as_array().unwrap()).unwrap();
        assert_eq!(found["tag_name"], "testing");
    }

    #[test]
    fn the_highest_version_wins_over_the_one_published_last() {
        // 0.3.10 and 0.4.0 pushed together: 0.3.10 was published a few seconds later
        let list = serde_json::json!([
            {"tag_name": "v0.4.0", "published_at": "2026-10-19T12:06:09Z"},
            {"tag_name": "v0.3.10", "published_at": "2026-10-19T12:06:22Z"},
            {"tag_name": "testing", "published_at": "2026-10-19T12:07:00Z"},
            {"tag_name": "v9.0.0", "draft": true},
            {"tag_name": "v0.3.9", "published_at": "2026-10-19T11:22:39Z"},
        ]);
        let found = highest(list.as_array().unwrap()).unwrap();
        assert_eq!(found["tag_name"], "v0.4.0");
    }

    #[test]
    fn picks_the_file_for_this_system() {
        let release = serde_json::json!({
            "tag_name": "v0.2.0",
            "html_url": "https://example/releases/v0.2.0",
            "assets": [
                {"browser_download_url": "https://example/TableCross-0.2.0-macOS.dmg"},
                {"browser_download_url": "https://example/TableCross-0.2.0-macOS.zip"},
                {"browser_download_url": "https://example/TableCross-0.2.0-Windows-Setup.exe"},
            ],
        });
        let found = newer_release(&release, "0.1.0", "", "", Some("-macOS.zip")).unwrap();
        assert_eq!(
            found.asset.as_deref(),
            Some("https://example/TableCross-0.2.0-macOS.zip")
        );
        let found = newer_release(&release, "0.1.0", "", "", Some("-Linux.tar.gz")).unwrap();
        assert_eq!(found.asset, None);
    }
}
