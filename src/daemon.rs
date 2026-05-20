//! `sdid` child-process spawn for the desktop shell.
//!
//! Resolution order mirrors the CLI's daemon-bin lookup:
//!   1. `SDI_DAEMON_BIN` env override
//!   2. plugin layout candidates (sibling, `<plugin_root>/daemon/bin/`)
//!   3. XDG data install (`~/.local/share/sdi/bin/sdid`)
//!   4. PATH lookup (`sdid`)
//!
//! The desktop crate stays free of source-level coupling to `sdi-cli` so the
//! installer can drop them in independently. The shared invariant is the
//! resolution order and the binary name only.

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const DAEMON_BIN_NAME: &str = "sdid";

#[derive(Debug)]
pub struct DaemonHandle {
    pub bin: PathBuf,
    pub child: Child,
}

pub fn resolve_daemon_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SDI_DAEMON_BIN") {
        if !p.is_empty() {
            let path = PathBuf::from(p);
            if path.exists() {
                return Some(path);
            }
        }
    }
    for candidate in candidate_paths() {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Last-resort: bare name; Command::spawn delegates to PATH lookup.
    Some(PathBuf::from(DAEMON_BIN_NAME))
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join(DAEMON_BIN_NAME));
            if let Some(parent) = dir.parent() {
                out.push(parent.join("daemon").join("bin").join(DAEMON_BIN_NAME));
            }
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let xdg_data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local").join("share"));
    out.push(xdg_data.join("sdi").join("bin").join(DAEMON_BIN_NAME));
    out
}

/// Spawn the daemon as a detached child. If `SDI_NO_AUTOSPAWN` is truthy,
/// this is a no-op and the desktop shell relies on an already-running daemon.
pub fn spawn() -> io::Result<DaemonHandle> {
    if std::env::var("SDI_NO_AUTOSPAWN")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Err(io::Error::other("SDI_NO_AUTOSPAWN is set"));
    }
    let bin = resolve_daemon_bin()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no sdid candidate"))?;
    let child = Command::new(&bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(DaemonHandle { bin, child })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_returns_some_fallback_when_nothing_exists() {
        // Even with no candidates we fall back to the bare binary name so
        // Command::spawn can defer to PATH lookup. The desktop must never
        // hard-fail on resolution before the daemon has had a chance to be
        // looked up via PATH.
        let resolved = resolve_daemon_bin();
        assert!(resolved.is_some());
    }
}
