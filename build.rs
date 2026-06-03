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

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let commit = git_output(["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = commit != "unknown" && git_dirty();
    let suffix = if dirty { ".dirty" } else { "" };
    println!("cargo:rustc-env=INDEXSEARCH_BUILD_ID=build.{seconds}.g{commit}{suffix}");
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
