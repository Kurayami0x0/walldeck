//! The wallpaper backend: shells out to an `awww`-compatible CLI.
//!
//! Nothing here assumes a particular flag set beyond the `img [flags] <path>`
//! shape -- which flags get passed is entirely driven by the config, and any
//! unset option is simply omitted (see `TransitionOpts`).

use crate::config::{Config, TransitionOpts};
use std::path::Path;
use std::process::Command;

/// Why an apply failed, so the caller can decide how loudly to complain.
#[derive(Debug)]
pub enum ApplyError {
    /// The backend binary itself couldn't be run at all.
    Spawn(String),
    /// The backend ran and reported failure. For a namespaced target this
    /// most commonly means no daemon is running for that namespace.
    Backend { status: String, stderr: String },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Spawn(e) => write!(f, "{e}"),
            ApplyError::Backend { status, stderr } if stderr.is_empty() => {
                write!(f, "exited with {status}")
            }
            ApplyError::Backend { status, stderr } => write!(f, "exited with {status}: {stderr}"),
        }
    }
}

/// Apply `image` to one target. `namespace` of `None` is the backend's
/// default (unnamed) namespace.
pub fn apply(
    cfg: &Config,
    namespace: Option<&str>,
    opts: &TransitionOpts,
    image: &Path,
) -> Result<(), ApplyError> {
    let mut cmd = Command::new(&cfg.command);
    cmd.arg("img");
    cmd.args(opts.to_args());
    if let Some(ns) = namespace {
        cmd.args(["--namespace", ns]);
    }
    cmd.arg(image);

    // Captured rather than inherited so a backend error can be folded into
    // our own message instead of being dumped raw between our warnings.
    let out = cmd
        .output()
        .map_err(|e| ApplyError::Spawn(format!("failed to run '{}': {e}", cfg.command)))?;

    if out.status.success() {
        return Ok(());
    }
    Err(ApplyError::Backend {
        status: out.status.to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Look up an executable on `$PATH`. Used for the startup dependency check,
/// in place of shelling out to `which` for something this simple.
pub fn is_on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}
