//! Read-only polling of the daemon's resolved-autonomy endpoint and the
//! D18 circuit-breaker write. Lives in the desktop shell so the title-bar /
//! tray surface stays in sync with the daemon's source of truth without
//! introducing a back-channel — every state read goes through the daemon's
//! public HTTP API, same as sdi-web.

use std::io;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomyMode {
    L3,
    L4,
    L5,
    Unknown,
}

impl AutonomyMode {
    pub fn label(self) -> &'static str {
        match self {
            AutonomyMode::L3 => "L3",
            AutonomyMode::L4 => "L4",
            AutonomyMode::L5 => "L5",
            AutonomyMode::Unknown => "—",
        }
    }
}

/// Returns the first project id the daemon knows about, or `None` if the
/// daemon answers with an empty list. The polling loop drops back to the
/// "no project" indicator instead of failing — a fresh install with zero
/// projects is a valid state.
pub async fn first_project_id(port: u16) -> Option<String> {
    let body = http_get_body(port, "/projects").await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let arr = v.as_array().or_else(|| {
        v.as_object().and_then(|o| o.values().next().and_then(|x| x.as_array()))
    })?;
    let first = arr.first()?;
    first.get("id")?.as_str().map(str::to_string)
}

/// Resolve the autonomy mode for the project root (no plan / no decision_kind).
/// Matches the daemon `/autonomy_policies/resolve` semantics: when no policy
/// has been set, the daemon returns `{ "policy": null }` and the title falls
/// back to the daemon default (currently L5 per D17, but we render `—` so the
/// indicator never lies about a value the daemon didn't return).
pub async fn resolve_mode(port: u16, project_id: &str) -> AutonomyMode {
    let path = format!(
        "/autonomy_policies/resolve?project_id={}",
        url_encode(project_id),
    );
    let Ok(body) = http_get_body(port, &path).await else {
        return AutonomyMode::Unknown;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return AutonomyMode::Unknown;
    };
    let mode = v
        .get("policy")
        .and_then(|p| p.get("mode"))
        .and_then(|m| m.as_str());
    match mode {
        Some("L3") => AutonomyMode::L3,
        Some("L4") => AutonomyMode::L4,
        Some("L5") => AutonomyMode::L5,
        _ => AutonomyMode::Unknown,
    }
}

/// D18 — circuit breaker. Posts to `/autonomy_policies/circuit_breaker` for the
/// given project and returns the number of demoted rows, or an error from the
/// daemon. The desktop uses this for the global shortcut so the user has one
/// always-on lockdown action regardless of which window has focus.
pub async fn circuit_breaker(port: u16, project_id: &str, reason: &str) -> io::Result<usize> {
    let payload = serde_json::json!({
        "project_id": project_id,
        "actor": "sdi-desktop",
        "reason": reason,
    });
    let body = serde_json::to_string(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let resp = http_post(port, "/autonomy_policies/circuit_breaker", &body).await?;
    let v: serde_json::Value = serde_json::from_str(&resp.body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if resp.status >= 400 {
        return Err(io::Error::other(format!(
            "circuit_breaker HTTP {} — {body}",
            resp.status,
            body = resp.body
        )));
    }
    let demoted = v
        .get("demoted")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as usize;
    Ok(demoted)
}

struct HttpResp {
    status: u16,
    body: String,
}

async fn http_get_body(port: u16, path: &str) -> io::Result<String> {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
    );
    let resp = exchange(port, req.as_bytes()).await?;
    if resp.status >= 400 {
        return Err(io::Error::other(format!(
            "GET {path} HTTP {}",
            resp.status
        )));
    }
    Ok(resp.body)
}

async fn http_post(port: u16, path: &str, body: &str) -> io::Result<HttpResp> {
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    exchange(port, req.as_bytes()).await
}

async fn exchange(port: u16, req: &[u8]) -> io::Result<HttpResp> {
    let fut = async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream.write_all(req).await?;
        stream.flush().await?;
        let mut raw = Vec::with_capacity(4096);
        stream.read_to_end(&mut raw).await?;
        parse_http(&raw)
    };
    timeout(HTTP_TIMEOUT, fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "http exchange timeout"))?
}

fn parse_http(raw: &[u8]) -> io::Result<HttpResp> {
    let split = find_double_crlf(raw)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header/body split"))?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 headers"))?;
    let status_line = head
        .lines()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no status line"))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status"))?;
    let body = String::from_utf8_lossy(&raw[split + 4..]).into_owned();
    Ok(HttpResp { status, body })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_extracts_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nhello";
        let resp = parse_http(raw).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "hello");
    }

    #[test]
    fn parse_http_rejects_malformed() {
        assert!(parse_http(b"not-http").is_err());
    }

    #[test]
    fn url_encode_escapes_path_components() {
        assert_eq!(url_encode("PROJ-abc_123"), "PROJ-abc_123");
        assert_eq!(url_encode("a b"), "a%20b");
    }

    #[test]
    fn mode_labels_match_protocol() {
        assert_eq!(AutonomyMode::L3.label(), "L3");
        assert_eq!(AutonomyMode::L4.label(), "L4");
        assert_eq!(AutonomyMode::L5.label(), "L5");
        assert_eq!(AutonomyMode::Unknown.label(), "—");
    }
}
