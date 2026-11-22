//! Bakes **which source this binary was built from** into berthd / berth.
//!
//! `berth info` asks the running daemon to name itself, and it answered `v0.1.0`
//! from every build ever made — so "which berthd is deployed on that box?" could
//! only be answered by looking at mtime and file size. That is exactly the
//! question `berth info` exists to answer, and the one that matters when the
//! daemon on a remote machine is older than the client talking to it.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rustc-env=BERTHD_BUILD_ID={}", build_id());

    // Without these, cargo reuses the previous build-script output and the
    // binary reports a commit it was **not** built from — the lie this field
    // exists to prevent.
    for path in rerun_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// The short commit, plus `-dirty` when the tree carried uncommitted changes.
/// `unknown` when git cannot answer — an identity field that guesses is worse
/// than one that abstains.
///
/// 注意: `-dirty` describes the tree **at compile time**. Release builds made from
/// a committed tree — the ones that get deployed — are exact.
fn build_id() -> String {
    let Some(hash) = git(&["rev-parse", "--short=9", "HEAD"]) else {
        return "unknown".to_string();
    };
    match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(status) if !status.is_empty() => format!("{hash}-dirty"),
        _ => hash,
    }
}

/// The git files whose change means a new commit.
fn rerun_paths() -> Vec<PathBuf> {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) else {
        return Vec::new();
    };
    let mut paths = vec![git_dir.join("HEAD"), git_dir.join("packed-refs")];
    if let Some(head_ref) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        paths.push(git_dir.join(head_ref));
    }
    paths.into_iter().filter(|p| p.exists()).collect()
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
