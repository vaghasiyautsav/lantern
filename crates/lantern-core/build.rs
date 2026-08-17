//! Stamp the build with where it came from.
//!
//! Updating from GitHub needs three facts the source alone can't provide:
//! which commit this binary was built from, when that commit landed, and
//! which working copy it was built in. All three are known here and nowhere
//! else, so they're baked in as env vars for `update.rs` to read.
//!
//! Everything degrades to "unknown": a tarball with no `.git`, a build in CI,
//! a machine without git — none of those are errors, they just mean this copy
//! can't update itself, and the app says so plainly rather than guessing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    // crates/lantern-core → repo root.
    let repo: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .unwrap_or_default();

    let commit = git(&repo, &["rev-parse", "--short", "HEAD"]);
    let date = git(&repo, &["log", "-1", "--format=%cs"]);
    let is_repo = commit.is_some();

    println!(
        "cargo:rustc-env=LANTERN_BUILD_COMMIT={}",
        commit.unwrap_or_else(|| "unknown".into())
    );
    println!(
        "cargo:rustc-env=LANTERN_BUILD_DATE={}",
        date.unwrap_or_else(|| "unknown".into())
    );
    println!(
        "cargo:rustc-env=LANTERN_BUILD_REPO={}",
        if is_repo {
            repo.display().to_string()
        } else {
            String::new()
        }
    );

    // Rebuild when HEAD moves, so a pulled update doesn't keep reporting the
    // commit it was first built at. Both paths exist in a normal checkout;
    // in a worktree or a tarball they don't, and missing rerun-if paths are
    // ignored rather than fatal.
    println!("cargo:rerun-if-changed={}", repo.join(".git/HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        repo.join(".git/refs/heads").display()
    );
}
