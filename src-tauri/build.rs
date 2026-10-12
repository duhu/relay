use std::process::Command;

/// `git rev-parse --short HEAD`, with a trailing `+` when the working tree
/// has uncommitted changes; `unknown` when git cannot answer (a source
/// tarball, or git missing on the build machine).
fn git_commit() -> String {
    let short = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    let Ok(out) = short else {
        return "unknown".into();
    };
    if !out.status.success() {
        return "unknown".into();
    }
    let mut hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        hash.push('+');
    }
    hash
}

/// The local date of the build, `YYYY-MM-DD`; `unknown` if `date` fails.
fn build_date() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn main() {
    println!("cargo:rustc-env=RELAY_COMMIT={}", git_commit());
    println!("cargo:rustc-env=RELAY_BUILD_DATE={}", build_date());
    // Re-stamp when HEAD moves or the index changes (commit, checkout, add);
    // the paths are relative to src-tauri/, the crate root.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    // Unstaged edits move neither file, and the date is whatever day the
    // script last ran, so an installed build could carry a clean, stale label.
    // `dev-install.sh` exports a fresh RELAY_STAMP to force a re-stamp per
    // install, and nowhere else: `cargo test` stays incremental.
    println!("cargo:rerun-if-env-changed=RELAY_STAMP");
    tauri_build::build();
}
