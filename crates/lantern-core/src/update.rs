//! Updating Lantern from GitHub.
//!
//! Lantern is built from source on the machine it runs on (`install.sh`), and
//! the repo is private, so there is no signed-release channel to pull a
//! finished binary from — CI-built .dmg/.deb artifacts are still ahead of us.
//! What exists is the working copy this binary was built in, so "update" here
//! means exactly what a person would do by hand:
//!
//! ```text
//! git fetch → git merge --ff-only → ./install.sh → reopen Lantern
//! ```
//!
//! Two rules shape the whole module:
//!
//! * **Never touch uncommitted work.** If the checkout is dirty the update is
//!   refused with the reason, not merged, stashed or forced. Somebody's
//!   half-finished change is worth more than being current.
//! * **Never pretend.** Every step that can't be verified — no git, no
//!   checkout, no network, no credentials — is reported as itself, so
//!   "up to date" always means checked, never assumed.
//!
//! The apply half runs as a detached script (`packaging/update.sh`), never as
//! a child of the app: it replaces the very binaries the app and engine are
//! running from, and on Linux you cannot overwrite a running executable at
//! all. See that script for the sequencing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// What this binary was built from — stamped in by `build.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    /// Short commit hash, or "unknown" for a build with no git around.
    pub commit: String,
    /// Commit date, `YYYY-MM-DD`.
    pub date: String,
    /// The working copy it was built in, if there was one.
    pub repo: Option<PathBuf>,
}

impl BuildInfo {
    pub fn current() -> Self {
        let repo = env!("LANTERN_BUILD_REPO");
        Self {
            commit: env!("LANTERN_BUILD_COMMIT").to_string(),
            date: env!("LANTERN_BUILD_DATE").to_string(),
            repo: (!repo.is_empty()).then(|| PathBuf::from(repo)),
        }
    }
}

/// The answer to "is there a newer Lantern, and can this copy take it?".
#[derive(Debug, Clone)]
pub struct UpdateCheck {
    pub build: BuildInfo,
    /// Branch the working copy is on.
    pub branch: String,
    /// Commits on the remote that aren't here yet.
    pub behind: u32,
    /// One line each, newest first, at most ten — what you'd read to decide.
    pub commits: Vec<String>,
    /// Uncommitted changes in the working copy.
    pub dirty: bool,
    /// Why updating isn't possible right now, in words a person can act on.
    /// `None` means the mechanism is fine — `behind` then says whether there
    /// is anything to do.
    pub blocked: Option<String>,
}

impl UpdateCheck {
    /// Something to install, and nothing in the way of installing it.
    pub fn can_update(&self) -> bool {
        self.behind > 0 && self.blocked.is_none()
    }

    /// One line for a status bar or a CLI.
    pub fn summary(&self) -> String {
        if let Some(reason) = &self.blocked {
            return reason.clone();
        }
        match self.behind {
            0 => format!(
                "Up to date — build {} ({})",
                self.build.commit, self.build.date
            ),
            1 => "1 new commit on GitHub".to_string(),
            n => format!("{n} new commits on GitHub"),
        }
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| format!("git couldn't run: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            err
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn blocked(build: BuildInfo, reason: String) -> UpdateCheck {
    UpdateCheck {
        build,
        branch: String::new(),
        behind: 0,
        commits: Vec::new(),
        dirty: false,
        blocked: Some(reason),
    }
}

/// Ask GitHub what's new. Blocking (git, network) — call it from
/// `spawn_blocking`, or use [`check`].
pub fn check_blocking() -> UpdateCheck {
    let build = BuildInfo::current();
    let Some(repo) = build.repo.clone() else {
        return blocked(
            build,
            "This copy wasn't built from a source checkout, so it can't \
             update itself. Reinstall with install.sh from the repo."
                .into(),
        );
    };
    if !repo.join(".git").exists() {
        return blocked(
            build,
            format!(
                "The working copy this was built from ({}) is gone, so there's \
                 nothing to update from. Clone the repo again and run \
                 install.sh.",
                repo.display()
            ),
        );
    }

    let branch = match git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(b) => b,
        Err(e) => return blocked(build, e),
    };
    let dirty = !git(&repo, &["status", "--porcelain"])
        .unwrap_or_default()
        .is_empty();

    // Whatever this branch actually tracks — never an assumed `origin/main`.
    // A work branch that exists only on this machine has no upstream at all,
    // and saying so beats the raw "couldn't find remote ref" git offers.
    let upstream = match git(&repo, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
    {
        Ok(u) => u,
        Err(_) => {
            return blocked(
                build,
                format!(
                    "You're on branch {branch}, which isn't tracking anything on \
                     GitHub — so there's nothing to update from. Lantern updates \
                     the branch it was built from; switch to main and check again."
                ),
            )
        }
    };
    let remote = upstream.split('/').next().unwrap_or("origin").to_string();

    // The fetch is the only step that needs the network — and for a private
    // repo, credentials. A GUI-launched engine may not have the ssh agent or
    // keychain helper its owner's terminal has, so the git error is passed
    // through verbatim rather than flattened to "check failed".
    if let Err(e) = git(&repo, &["fetch", "--quiet", &remote]) {
        return blocked(
            build,
            format!("Couldn't reach GitHub: {e}\nLantern didn't change anything."),
        );
    }

    let range = format!("HEAD..{upstream}");
    let behind = git(&repo, &["rev-list", "--count", &range])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let commits = git(&repo, &["log", "--oneline", "-n", "10", &range])
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let blocked_reason = if dirty && behind > 0 {
        Some(format!(
            "There are uncommitted changes in {} — Lantern won't touch them. \
             Commit or stash them, then update.",
            repo.display()
        ))
    } else {
        None
    };

    UpdateCheck {
        build,
        branch,
        behind,
        commits,
        dirty,
        blocked: blocked_reason,
    }
}

/// [`check_blocking`] off the async path.
pub async fn check() -> UpdateCheck {
    tokio::task::spawn_blocking(check_blocking)
        .await
        .unwrap_or_else(|_| blocked(BuildInfo::current(), "The update check crashed.".into()))
}

/// Where the updater records what it's doing, for whoever is watching —
/// including the next run of the app, since the app quits mid-update.
pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update.state")
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update.log")
}

/// Start the update and return immediately.
///
/// The script is deliberately orphaned (`nohup … &` inside a throwaway
/// shell): it outlives this engine on purpose, because installing means
/// replacing this very binary. Progress goes to `update.log`, the outcome to
/// `update.state`; nothing is streamed back through here, since the process
/// that would stream it is the one being replaced.
pub fn start(data_dir: &Path) -> Result<(), String> {
    let check = check_blocking();
    if let Some(reason) = check.blocked {
        return Err(reason);
    }
    let repo = check
        .build
        .repo
        .ok_or_else(|| "No source checkout to update from.".to_string())?;
    let script = repo.join("packaging/update.sh");
    if !script.exists() {
        return Err(format!(
            "The updater script is missing from {}. Pull the repo by hand \
             and run install.sh.",
            repo.display()
        ));
    }
    std::fs::create_dir_all(data_dir).ok();
    let log = log_path(data_dir);

    // `sh -c` exits at once, leaving the script parented to init and immune
    // to this process going away moments later.
    let status = Command::new("sh")
        .arg("-c")
        .arg(r#"nohup bash "$1" --handoff "$2" "$3" >> "$4" 2>&1 & exit 0"#)
        .arg("sh")
        .arg(&script)
        .arg(&repo)
        .arg(data_dir)
        .arg(&log)
        .status()
        .map_err(|e| format!("Couldn't start the updater: {e}"))?;
    if !status.success() {
        return Err("The updater refused to start.".into());
    }
    Ok(())
}

/// What the updater last said. `None` when it has never run.
///
/// The file is written by shell, so it's read defensively: an update that was
/// killed halfway leaves a `running` state that nothing will ever finish, and
/// a stale `running` must not be shown as "in progress" forever — callers
/// compare against `started` for that.
pub fn last_state(data_dir: &Path) -> Option<UpdateState> {
    let raw = std::fs::read_to_string(state_path(data_dir)).ok()?;
    let field = |key: &str| -> Option<String> {
        // Deliberately not a JSON dependency: this file has five flat string
        // fields written by one shell script, and a parser for it belongs in
        // one place, not in a schema.
        let pat = format!("\"{key}\":\"");
        let start = raw.find(&pat)? + pat.len();
        let rest = &raw[start..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    };
    Some(UpdateState {
        state: field("state").unwrap_or_else(|| "unknown".into()),
        step: field("step").unwrap_or_default(),
        message: field("message").unwrap_or_default(),
        commit: field("commit").unwrap_or_default(),
        started: field("started").unwrap_or_default(),
    })
}

/// The outcome of an update this machine hasn't been told about yet, marking
/// it as told.
///
/// An app cannot watch its own update finish — it quits partway through — so
/// the result is reported by whichever run comes next. Once. Repeating
/// "Updated to abc1234" at every launch for the rest of the week would train
/// people to ignore the one message that matters, so the acknowledgement is
/// recorded beside the state file.
pub fn take_unseen_result(data_dir: &Path) -> Option<UpdateState> {
    let state = last_state(data_dir)?;
    if state.is_running() || state.started.is_empty() {
        return None;
    }
    let seen_path = data_dir.join("update.seen");
    if std::fs::read_to_string(&seen_path).ok().as_deref() == Some(state.started.as_str()) {
        return None;
    }
    // Written before returning: a shell that crashes while showing this
    // should not show it again forever.
    let _ = std::fs::write(&seen_path, &state.started);
    Some(state)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateState {
    /// "running" · "ok" · "failed"
    pub state: String,
    /// Which step it's on (or failed at): fetch · merge · build · relaunch.
    pub step: String,
    /// Plain-language detail, safe to show as-is.
    pub message: String,
    /// Commit installed, once there is one.
    pub commit: String,
    /// Unix seconds, as text.
    pub started: String,
}

impl UpdateState {
    pub fn is_running(&self) -> bool {
        self.state == "running"
    }
    pub fn succeeded(&self) -> bool {
        self.state == "ok"
    }
    pub fn failed(&self) -> bool {
        self.state == "failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_is_stamped_in() {
        let b = BuildInfo::current();
        assert!(!b.commit.is_empty());
        assert!(!b.date.is_empty());
    }

    #[test]
    fn a_copy_with_no_checkout_says_so_instead_of_failing() {
        // Exercised through `blocked`, the shape every unusable case takes:
        // never `can_update`, and always a reason worth showing.
        let c = blocked(BuildInfo::current(), "no checkout".into());
        assert!(!c.can_update());
        assert_eq!(c.summary(), "no checkout");
    }

    #[test]
    fn dirty_and_behind_reports_the_reason_not_a_count() {
        let c = UpdateCheck {
            build: BuildInfo::current(),
            branch: "main".into(),
            behind: 3,
            commits: vec!["abc123 something".into()],
            dirty: true,
            blocked: Some("uncommitted changes".into()),
        };
        assert!(!c.can_update());
        assert_eq!(c.summary(), "uncommitted changes");
    }

    #[test]
    fn up_to_date_names_the_build_it_checked() {
        let c = UpdateCheck {
            build: BuildInfo {
                commit: "abc1234".into(),
                date: "2026-08-18".into(),
                repo: None,
            },
            branch: "main".into(),
            behind: 0,
            commits: vec![],
            dirty: false,
            blocked: None,
        };
        assert!(!c.can_update());
        assert_eq!(c.summary(), "Up to date — build abc1234 (2026-08-18)");
    }

    #[test]
    fn state_file_is_parsed_and_a_missing_one_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("lantern-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(last_state(&dir).is_none());

        std::fs::write(
            state_path(&dir),
            r#"{"state":"failed","step":"build","message":"cargo build failed","commit":"","started":"1786000000"}"#,
        )
        .unwrap();
        let s = last_state(&dir).unwrap();
        assert!(s.failed() && !s.succeeded() && !s.is_running());
        assert_eq!(s.step, "build");
        assert_eq!(s.message, "cargo build failed");

        // Garbage must degrade to "unknown", not panic or unwrap.
        std::fs::write(state_path(&dir), "not json at all").unwrap();
        assert_eq!(last_state(&dir).unwrap().state, "unknown");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_outcome_is_reported_once_and_a_running_update_not_at_all() {
        let dir = std::env::temp_dir().join(format!("lantern-seen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Mid-update: there's no outcome to report yet.
        std::fs::write(
            state_path(&dir),
            r#"{"state":"running","step":"build","message":"building","commit":"","started":"1786000001"}"#,
        )
        .unwrap();
        assert!(take_unseen_result(&dir).is_none());

        std::fs::write(
            state_path(&dir),
            r#"{"state":"ok","step":"done","message":"Updated to abc1234","commit":"abc1234","started":"1786000001"}"#,
        )
        .unwrap();
        assert_eq!(
            take_unseen_result(&dir).map(|s| s.commit),
            Some("abc1234".to_string())
        );
        // Second launch: already said.
        assert!(take_unseen_result(&dir).is_none());

        // A later update is a different outcome, and gets reported.
        std::fs::write(
            state_path(&dir),
            r#"{"state":"failed","step":"merge","message":"diverged","commit":"","started":"1786000999"}"#,
        )
        .unwrap();
        assert!(take_unseen_result(&dir).is_some_and(|s| s.failed()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
