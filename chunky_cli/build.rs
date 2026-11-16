//! Sets `CHUNKY_GIT_SHA` (short, `-dirty` with local changes) and `CHUNKY_COMMIT_TIME` (UTC) for `--version`
//! (both `unknown` without git). The commit's time, not the build's: a build time forces rebuilds when nothing
//! changed and means nothing once builds are reproducible.

use std::process::Command;

fn main() {
    // .git is a file in a worktree: ask git where HEAD and the index live
    for file in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--git-path", file]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rustc-env=CHUNKY_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=CHUNKY_COMMIT_TIME={}", commit_time());
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_sha() -> String {
    let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) else {
        return "unknown".into();
    };
    match git(&["status", "--porcelain"]) {
        Some(s) if !s.is_empty() => format!("{sha}-dirty"),
        _ => sha,
    }
}

/// "3 Oct 2026 18:20 UTC", from the last commit's time, without a date crate
fn commit_time() -> String {
    let Some(epoch) = git(&["log", "-1", "--format=%ct"]) else {
        return "unknown".into();
    };
    let Ok(secs) = epoch.parse::<i64>() else {
        return "unknown".into();
    };
    let (days, rest) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{day} {} {year} {:02}:{:02} UTC",
        MONTHS[(month - 1) as usize],
        rest / 3600,
        rest % 3600 / 60
    )
}
