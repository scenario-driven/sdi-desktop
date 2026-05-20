//! Locate the local daemon's HTTP origin from its port file and confirm
//! liveness with a single `GET /health` over TCP.
//!
//! The SDI daemon binds an ephemeral TCP port on 127.0.0.1 (see
//! `crates/daemon/src/main.rs`) and writes the chosen port to
//! `$SDI_HOME/.cache/sdi/sdid.port` (or `$XDG_CACHE_HOME/sdi/sdid.port`).
//! The desktop shell points the WebView at that origin so the bundled SPA
//! makes same-origin requests (matching the browser flow — no separate IPC
//! transport).

use std::io;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

const PORT_POLL_INTERVAL: Duration = Duration::from_millis(150);
const PORT_POLL_BUDGET: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

pub fn port_file_path() -> PathBuf {
    if let Ok(sdi_home) = std::env::var("SDI_HOME") {
        return PathBuf::from(sdi_home).join(".cache").join("sdi").join("sdid.port");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let xdg = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    xdg.join("sdi").join("sdid.port")
}

pub fn read_port() -> Option<u16> {
    let raw = std::fs::read_to_string(port_file_path()).ok()?;
    raw.trim().parse().ok()
}

pub async fn wait_for_port() -> Option<u16> {
    let deadline = tokio::time::Instant::now() + PORT_POLL_BUDGET;
    loop {
        if let Some(p) = read_port() {
            return Some(p);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(PORT_POLL_INTERVAL).await;
    }
}

pub async fn ping_health(port: u16) -> io::Result<u16> {
    let fut = async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        let req = b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream.write_all(req).await?;
        stream.flush().await?;
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await?;
        let head = std::str::from_utf8(&buf[..n])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8"))?;
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no status"))?;
        Ok::<u16, io::Error>(status)
    };
    timeout(HEALTH_TIMEOUT, fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "health timed out"))?
}

pub fn origin(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_file_respects_sdi_home() {
        let prev = std::env::var_os("SDI_HOME");
        // Use a scoped env override; serial tests are fine here since the
        // file path is purely derived.
        unsafe { std::env::set_var("SDI_HOME", "/tmp/sdi-test"); }
        let p = port_file_path();
        assert!(p.ends_with("sdi-test/.cache/sdi/sdid.port"), "got: {}", p.display());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("SDI_HOME", v),
                None => std::env::remove_var("SDI_HOME"),
            }
        }
    }

    #[test]
    fn origin_formats_localhost() {
        assert_eq!(origin(52234), "http://127.0.0.1:52234/");
    }
}
