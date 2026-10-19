use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=TXWATCH_GIT_SHA");

    // Rebuild when the checked-out commit changes. The paths must point at the
    // repository's real git dir: relative `.git/...` paths resolve against
    // crates/cli, don't exist, and make Cargo rerun this script every build.
    if let Some(git_dir) = git_dir() {
        for path in rerun_paths(&git_dir) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    println!("cargo:rustc-env=TXWATCH_GIT_SHA={}", git_sha());
    println!(
        "cargo:rustc-env=TXWATCH_BUILD_TIMESTAMP={}",
        build_timestamp()
    );
}

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn git_dir() -> Option<PathBuf> {
    git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// HEAD itself, the branch ref it points at (loose or packed), so both a
/// checkout and a new commit trigger a rerun.
fn rerun_paths(git_dir: &Path) -> Vec<PathBuf> {
    let head = git_dir.join("HEAD");
    let mut paths = vec![head.clone()];
    if let Some(reference) = std::fs::read_to_string(&head)
        .ok()
        .and_then(|h| h.strip_prefix("ref: ").map(|r| r.trim().to_owned()))
    {
        let loose = git_dir.join(reference);
        if loose.exists() {
            paths.push(loose);
        }
    }
    let packed = git_dir.join("packed-refs");
    if packed.exists() {
        paths.push(packed);
    }
    paths
}

/// `TXWATCH_GIT_SHA` wins when set (Docker builds exclude `.git/`), otherwise
/// the checked-out commit is used, and only a build outside git reports
/// "unknown".
fn git_sha() -> String {
    std::env::var("TXWATCH_GIT_SHA")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Build time as an RFC 3339 UTC string. Reproducible: `SOURCE_DATE_EPOCH`
/// wins when set, otherwise the HEAD commit's timestamp is used, and only a
/// build outside git falls back to the current time.
fn build_timestamp() -> String {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .or_else(|| git(&["log", "-1", "--format=%ct"]).and_then(|v| v.parse().ok()))
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });
    format_rfc3339(epoch)
}

/// Unix seconds to `YYYY-MM-DDTHH:MM:SSZ` without a date binary or crate
/// (civil-from-days, Howard Hinnant's algorithm).
fn format_rfc3339(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}
