//! Bake the build's identity into the binary: the cargo version and the git
//! revision of the tree it was built from, as the `AJ_VERSION` variable that
//! `aj_app::VERSION` reads.

use std::process::Command;

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let revision = revision().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AJ_VERSION={version} ({revision})");
    watch();
}

/// Tell cargo when the revision may have changed.
///
/// Cargo recompiles this crate and everything above it whenever the script
/// reruns, whether or not its output changed, so rerunning on every build is
/// not affordable. A missed rerun bakes in a stale revision, which is worse
/// than none, so the watched set errs towards rerunning.
fn watch() {
    let Some(root) = git(&["rev-parse", "--show-toplevel"]) else {
        // Not a checkout: the revision stays `unknown`, and cargo's default
        // of watching the package is as good as any rule.
        return;
    };
    // The dirty marker follows the working tree. Every tracked top-level
    // entry is watched, directories recursively, which leaves out ignored
    // build output that changes on each build.
    for entry in git(&["ls-tree", "--full-tree", "--name-only", "HEAD"])
        .unwrap_or_default()
        .lines()
    {
        println!("cargo:rerun-if-changed={root}/{entry}");
    }
    // The hash follows `HEAD`, which moves without touching a file in the
    // tree on a commit. `HEAD` itself changes on a checkout, and its reflog on
    // every move, including a commit to the checked-out branch. Cargo reruns
    // the script on every build while a watched path is missing, so a
    // repository without reflogs stays correct at the cost of rebuilds.
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// The short hash of `HEAD`, marked `-dirty` when the tree differs from it.
/// `None` when the source is not a git checkout or git is unavailable.
fn revision() -> Option<String> {
    let hash = git(&["rev-parse", "--short=12", "HEAD"])?;
    if hash.is_empty() {
        return None;
    }
    // Untracked files count: a new source file changes the build as much as
    // an edited one. A status that cannot be read claims nothing about the
    // tree, so it must not pass for a clean one.
    let dirty = git(&["--no-optional-locks", "status", "--porcelain"])
        .is_none_or(|status| !status.is_empty());
    Some(if dirty { format!("{hash}-dirty") } else { hash })
}

/// Trimmed stdout of a git command run in this package, which resolves the
/// repository from linked worktrees as well.
fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var_os("CARGO_MANIFEST_DIR")?;
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}
