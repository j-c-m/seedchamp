//! Embed short git revision into VERSION for TUI / doctor display.

use std::process::Command;

fn main() {
    let sha = git_stdout(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git_stdout(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let rev = if dirty { format!("{sha}-dirty") } else { sha };
    println!("cargo:rustc-env=GIT_SHA={rev}");
    // Best-effort rebuild when HEAD moves (works when .git is present).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

fn git_stdout(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}
