use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    for path in [
        ".git/HEAD",
        ".git/index",
        "Cargo.toml",
        "Cargo.lock",
        "README.md",
        "build.rs",
        "src",
        "skills",
        "templates",
        "agent-rules",
        "scripts",
        "install.ps1",
        "install.sh",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    set_build_id();
    ensure_frontend_aliases();
}

fn set_build_id() {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let commit = git_output(["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = commit != "unknown" && git_dirty();
    let suffix = if dirty { ".dirty" } else { "" };
    println!("cargo:rustc-env=INDEXSEARCH_BUILD_ID=build.{seconds}.g{commit}{suffix}");
}

fn ensure_frontend_aliases() {
    let Some(profile_dir) = profile_dir() else {
        return;
    };
    match env::var("CARGO_CFG_TARGET_FAMILY").as_deref() {
        Ok("unix") => {
            ensure_unix_alias(&profile_dir, "is");
            ensure_unix_alias(&profile_dir, "isgrep");
        }
        Ok("windows") => {
            remove_stale_alias(&profile_dir.join("is.exe"));
            remove_stale_alias(&profile_dir.join("isgrep.exe"));
        }
        _ => {}
    }
}

fn git_output<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn git_dirty() -> bool {
    !git_quiet(["diff", "--quiet", "--ignore-submodules", "--"])
        || !git_quiet(["diff", "--cached", "--quiet", "--ignore-submodules", "--"])
}

fn git_quiet<const N: usize>(args: [&str; N]) -> bool {
    Command::new("git")
        .args(args)
        .status()
        .is_ok_and(|status| status.success())
}

fn profile_dir() -> Option<PathBuf> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR")?);
    out_dir.ancestors().nth(3).map(Path::to_path_buf)
}

#[cfg(unix)]
fn ensure_unix_alias(profile_dir: &Path, name: &str) {
    use std::os::unix::fs::symlink;

    let path = profile_dir.join(name);
    if path.exists() || fs::symlink_metadata(&path).is_ok() {
        let _ = fs::remove_file(&path);
    }
    let _ = symlink("indexsearch", path);
}

#[cfg(not(unix))]
fn ensure_unix_alias(_profile_dir: &Path, _name: &str) {}

fn remove_stale_alias(path: &Path) {
    if path.exists() || fs::symlink_metadata(path).is_ok() {
        let _ = fs::remove_file(path);
    }
}
