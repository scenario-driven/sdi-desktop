//! Active CollaborationPattern mirror for the tray badge (v0.5).
//!
//! Subscribes to the daemon's `/events` SSE stream and keeps a snapshot of
//! currently-active patterns in memory so the tray icon can render:
//!
//! * the **count of active patterns** (any `lifecycle = 'active'` row), and
//! * a **red-dot marker when at least one of them is `kind = 'direct'`** —
//!   per ARCHITECTURE.md §"How the surfaces render this" the desktop must
//!   surface the direct-pattern anti-marker visibly (D23 / D27).
//!
//! Transport mirrors `autonomy.rs`: every read goes through the daemon's
//! public HTTP API + SSE (no back-channel, no IPC bridge). State is seeded
//! once via `GET /patterns/active` on startup; thereafter the SSE stream
//! drives the cache. The endpoints are scheduled for v0.5 and may not exist
//! on the running daemon yet — every failure mode degrades silently to
//! "unknown" (count `None`), so the tray tooltip drops the suffix instead
//! of panicking.
//!
//! Event kinds consumed (per PRD §3.9 lifecycle + ARCHITECTURE.md §2.6):
//!   - `pattern_created`     → insert row (any lifecycle; non-active filtered on read)
//!   - `pattern_lifecycle`   → update lifecycle; remove if leaving `active`
//!   - `pattern_aborted`     → remove row
//!   - `pattern_converged` / `pattern_dissensus` → treated as terminal (remove)
//!
//! Reconnect strategy: exponential backoff capped at 30s, jitter-free
//! because a single desktop client is the only consumer of its own loop
//! (no thundering-herd concern).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::{sleep, timeout};

/// HTTP request timeout for the one-shot bootstrap call. The SSE socket is
/// long-lived and uses its own loop-level backoff instead.
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
#[allow(dead_code)] // `plan_id` is captured per the v0.5 task spec for
                    // per-plan surfaces (PatternTree / PatternTimeline)
                    // that ship with later releases; the tray badge
                    // itself only needs the aggregate count + the
                    // "any direct?" flag, but the field MUST be on
                    // ActivePattern per spec so a future surface
                    // doesn't need to reshape the cache.
pub struct ActivePattern {
    pub id: String,
    pub kind: String,
    /// Plan that owns the pattern. See struct-level `#[allow(dead_code)]`.
    pub plan_id: String,
    /// Last observed lifecycle. Only `"active"` rows are surfaced through
    /// `active_count()` / `has_direct_pattern()`. Non-active rows are kept
    /// briefly during transition events so a `pattern_lifecycle` payload
    /// that touches `pending → active` doesn't need an out-of-band fetch.
    pub lifecycle: String,
}

impl ActivePattern {
    fn is_active(&self) -> bool {
        self.lifecycle == "active"
    }

    fn is_direct(&self) -> bool {
        self.kind == "direct"
    }
}

/// Read-mostly cache of the daemon's active CollaborationPattern rows.
/// Cloned freely across async tasks — internal state lives behind an
/// `Arc<RwLock<…>>` so the SSE loop and the surface composer don't fight
/// each other.
#[derive(Clone, Default)]
pub struct PatternMonitor {
    inner: Arc<RwLock<MonitorInner>>,
}

#[derive(Default)]
struct MonitorInner {
    /// `id → ActivePattern`. `None` means the daemon has never answered
    /// (initial state and after the bootstrap failed) — the surface shows
    /// `CP:?` rather than `CP:0` to avoid lying about a 0-active state we
    /// haven't verified.
    rows: Option<HashMap<String, ActivePattern>>,
}

/// Snapshot of the cache, taken under the read lock so the surface
/// composer never sees a half-applied SSE event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatternBadge {
    /// `None` until the bootstrap (or any later SSE batch) succeeds.
    pub active_count: Option<usize>,
    pub has_direct: bool,
}

impl PatternBadge {
    /// Trailing string for the tray tooltip and window title. Returns an
    /// empty string when the count is unknown so the autonomy surface stays
    /// the only thing visible until the daemon answers.
    pub fn tooltip_suffix(self) -> String {
        let Some(n) = self.active_count else {
            return String::new();
        };
        if n == 0 {
            return " | active patterns: 0".to_string();
        }
        let marker = if self.has_direct {
            // U+1F534 large red circle — renders as a red dot on every
            // modern macOS / Windows / Linux tray font we tested. Avoids
            // depending on a custom icon overlay.
            " \u{1F534}"
        } else {
            ""
        };
        format!(" | active patterns: {n}{marker}")
    }

    /// Suffix for the window title (and macOS menubar icon text). Kept
    /// short — autonomy mode remains the primary signal.
    pub fn title_suffix(self) -> String {
        let Some(n) = self.active_count else {
            return String::new();
        };
        if n == 0 {
            return String::new();
        }
        let marker = if self.has_direct { " \u{26A0}" } else { "" };
        format!(" • CP:{n}{marker}")
    }
}

impl PatternMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> PatternBadge {
        let guard = self.inner.read().await;
        match guard.rows.as_ref() {
            None => PatternBadge::default(),
            Some(map) => {
                let active: Vec<&ActivePattern> =
                    map.values().filter(|p| p.is_active()).collect();
                PatternBadge {
                    active_count: Some(active.len()),
                    has_direct: active.iter().any(|p| p.is_direct()),
                }
            }
        }
    }

    /// Distinct plan IDs that currently host at least one active pattern.
    /// Not surfaced in the v0.5 tray (the badge is aggregate-only), but
    /// the daemon exposes plan-scoped pattern endpoints in the v0.5 PRD
    /// and a future surface — per-plan menu items in the tray submenu,
    /// or a debug-log breakdown — will consume this. Kept on the public
    /// API so adding that surface is a no-touch change to the monitor.
    #[allow(dead_code)] // Forward-looking accessor; see struct-level note.
    pub async fn active_plan_ids(&self) -> Vec<String> {
        let guard = self.inner.read().await;
        let Some(map) = guard.rows.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<String> = map
            .values()
            .filter(|p| p.is_active() && !p.plan_id.is_empty())
            .map(|p| p.plan_id.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// One-shot seed of the cache from `GET /patterns/active`. Tolerates a
    /// 404 (endpoint not yet shipped) or any other I/O failure by leaving
    /// `rows` as `None`, so the surface continues to show `?` until the
    /// SSE stream produces a `pattern_created` it can apply.
    pub async fn bootstrap(&self, port: u16) {
        let Ok(body) = http_get_body(port, "/patterns/active").await else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
            return;
        };
        let arr = v
            .as_array()
            .cloned()
            .or_else(|| {
                v.as_object()
                    .and_then(|o| o.get("patterns"))
                    .and_then(|x| x.as_array().cloned())
            })
            .unwrap_or_default();
        let mut map = HashMap::with_capacity(arr.len());
        for row in arr {
            if let Some(p) = parse_pattern_value(&row) {
                map.insert(p.id.clone(), p);
            }
        }
        let mut guard = self.inner.write().await;
        guard.rows = Some(map);
    }

    /// Long-lived SSE subscription. Reconnects with exponential backoff on
    /// any disconnect (daemon restart, network glitch, endpoint absent).
    /// Returns when the surrounding async runtime is torn down by Tauri's
    /// quit hook.
    pub async fn run_events_loop(self, port: u16, mut on_change: impl FnMut(PatternBadge)) {
        let mut backoff = RECONNECT_MIN;
        loop {
            let before = self.snapshot().await;
            match self.run_one_stream(port, &mut on_change, before).await {
                Ok(()) => {
                    // Server-side EOF — usually means the daemon restarted.
                    // Reset backoff so the next attempt is immediate-ish.
                    backoff = RECONNECT_MIN;
                }
                Err(_e) => {
                    // Connection refused / endpoint not present / timeout.
                    // Backoff to avoid hot-spinning against a dead daemon.
                }
            }
            sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
    }

    async fn run_one_stream(
        &self,
        port: u16,
        on_change: &mut impl FnMut(PatternBadge),
        mut last_emitted: PatternBadge,
    ) -> io::Result<()> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        let req = b"GET /events HTTP/1.1\r\n\
                    Host: localhost\r\n\
                    Accept: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Connection: keep-alive\r\n\r\n";
        stream.write_all(req).await?;
        stream.flush().await?;

        let (read_half, _write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // Skip the HTTP response head. The SDI daemon answers chunked
        // text/event-stream, but the chunk framing for SSE doesn't matter
        // for us — every SSE payload ends in a blank line and we read by
        // line, so the chunk length markers fall out as junk lines that
        // the parser tolerates.
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(());
            }
            headers.push(line.clone());
            if line == "\r\n" || line == "\n" {
                break;
            }
        }
        // Surface a connection failure when the daemon returns an error
        // status — typically 404 if /events is missing.
        if let Some(status_line) = headers.first() {
            if let Some(code) = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u16>().ok())
            {
                if code >= 400 {
                    return Err(io::Error::other(format!("/events HTTP {code}")));
                }
            }
        }

        // SSE framing: lines of `event: <kind>` and `data: <json>`, blocks
        // separated by a blank line. We accumulate per-block then dispatch.
        let mut cur_event: Option<String> = None;
        let mut cur_data = String::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(());
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                if let Some(kind) = cur_event.take() {
                    self.apply_event(&kind, &cur_data).await;
                    let now = self.snapshot().await;
                    if now != last_emitted {
                        on_change(now);
                        last_emitted = now;
                    }
                }
                cur_data.clear();
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("event:") {
                cur_event = Some(rest.trim().to_string());
            } else if let Some(rest) = trimmed.strip_prefix("data:") {
                if !cur_data.is_empty() {
                    cur_data.push('\n');
                }
                cur_data.push_str(rest.trim_start());
            }
            // Other lines (`id:`, `retry:`, chunked-length markers, hex
            // chunk sizes that fall out when reading a chunked body
            // line-by-line) are ignored.
        }
    }

    async fn apply_event(&self, kind: &str, data: &str) {
        // Every pattern-touching event carries either the full row or
        // enough fields to update the cache. Anything that doesn't parse
        // is silently dropped — the SSE loop will catch up on the next
        // delta. We never error out mid-stream.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };
        // The daemon's EventEnvelope wraps payloads as `{ kind, entity_id,
        // payload }`. We accept both that shape and a bare pattern row so
        // the monitor works with whichever shape the daemon settles on.
        let payload = v.get("payload").unwrap_or(&v);

        match kind {
            "pattern_created" | "pattern_lifecycle" => {
                if let Some(p) = parse_pattern_value(payload) {
                    let mut guard = self.inner.write().await;
                    let map = guard.rows.get_or_insert_with(HashMap::new);
                    // A `pattern_lifecycle` that transitioned away from
                    // active is the natural removal signal (matches
                    // PRD §3.9 — converged / dissensus / aborted all
                    // leave the active set).
                    if matches!(
                        p.lifecycle.as_str(),
                        "converged" | "dissensus" | "aborted"
                    ) {
                        map.remove(&p.id);
                    } else {
                        map.insert(p.id.clone(), p);
                    }
                }
            }
            "pattern_aborted" | "pattern_converged" | "pattern_dissensus" => {
                if let Some(id) = payload
                    .get("id")
                    .and_then(|x| x.as_str())
                    .or_else(|| v.get("entity_id").and_then(|x| x.as_str()))
                {
                    let mut guard = self.inner.write().await;
                    if let Some(map) = guard.rows.as_mut() {
                        map.remove(id);
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_pattern_value(v: &serde_json::Value) -> Option<ActivePattern> {
    let id = v.get("id")?.as_str()?.to_string();
    // `kind` is required on creation; if it's missing this is not a
    // pattern row and we skip.
    let kind = v.get("kind")?.as_str()?.to_string();
    let plan_id = v
        .get("plan_id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let lifecycle = v
        .get("lifecycle")
        .and_then(|x| x.as_str())
        .unwrap_or("pending")
        .to_string();
    Some(ActivePattern {
        id,
        kind,
        plan_id,
        lifecycle,
    })
}

async fn http_get_body(port: u16, path: &str) -> io::Result<String> {
    let fut = async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        );
        stream.write_all(req.as_bytes()).await?;
        stream.flush().await?;
        let mut raw = Vec::with_capacity(4096);
        stream.read_to_end(&mut raw).await?;
        parse_http_body(&raw)
    };
    timeout(HTTP_TIMEOUT, fut)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "http exchange timeout"))?
}

fn parse_http_body(raw: &[u8]) -> io::Result<String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no header/body split"))?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 headers"))?;
    let status = head
        .lines()
        .next()
        .and_then(|s| s.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status"))?;
    if status >= 400 {
        return Err(io::Error::other(format!("HTTP {status}")));
    }
    Ok(String::from_utf8_lossy(&raw[split + 4..]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_default_renders_empty_suffix() {
        let b = PatternBadge::default();
        assert_eq!(b.tooltip_suffix(), "");
        assert_eq!(b.title_suffix(), "");
    }

    #[test]
    fn badge_zero_renders_no_title_suffix() {
        let b = PatternBadge {
            active_count: Some(0),
            has_direct: false,
        };
        assert_eq!(b.tooltip_suffix(), " | active patterns: 0");
        assert_eq!(b.title_suffix(), "");
    }

    #[test]
    fn badge_with_direct_renders_red_marker() {
        let b = PatternBadge {
            active_count: Some(2),
            has_direct: true,
        };
        let tt = b.tooltip_suffix();
        assert!(tt.contains("active patterns: 2"));
        assert!(tt.contains('\u{1F534}'), "missing red-circle marker: {tt}");
        let title = b.title_suffix();
        assert!(title.contains("CP:2"));
        assert!(title.contains('\u{26A0}'), "missing warning marker: {title}");
    }

    #[test]
    fn badge_without_direct_omits_marker() {
        let b = PatternBadge {
            active_count: Some(1),
            has_direct: false,
        };
        assert_eq!(b.tooltip_suffix(), " | active patterns: 1");
        assert_eq!(b.title_suffix(), " • CP:1");
    }

    #[test]
    fn parse_pattern_row_extracts_required_fields() {
        let v = serde_json::json!({
            "id": "CP-01",
            "kind": "direct",
            "plan_id": "PLAN-01",
            "lifecycle": "active"
        });
        let p = parse_pattern_value(&v).expect("must parse");
        assert_eq!(p.id, "CP-01");
        assert_eq!(p.kind, "direct");
        assert_eq!(p.plan_id, "PLAN-01");
        assert_eq!(p.lifecycle, "active");
        assert!(p.is_active());
        assert!(p.is_direct());
    }

    #[test]
    fn parse_pattern_row_rejects_missing_kind() {
        let v = serde_json::json!({ "id": "CP-01" });
        assert!(parse_pattern_value(&v).is_none());
    }

    #[tokio::test]
    async fn snapshot_starts_unknown_until_bootstrap() {
        let mon = PatternMonitor::new();
        let snap = mon.snapshot().await;
        assert_eq!(snap.active_count, None);
        assert!(!snap.has_direct);
    }

    #[tokio::test]
    async fn apply_event_inserts_active_pattern() {
        let mon = PatternMonitor::new();
        // Seed an empty (but known) cache so apply_event has somewhere to
        // store rows.
        {
            let mut g = mon.inner.write().await;
            g.rows = Some(HashMap::new());
        }
        let payload = serde_json::json!({
            "kind": "pattern_created",
            "payload": {
                "id": "CP-A",
                "kind": "workflow",
                "plan_id": "PLAN-1",
                "lifecycle": "active"
            }
        });
        mon.apply_event("pattern_created", &payload.to_string()).await;
        let snap = mon.snapshot().await;
        assert_eq!(snap.active_count, Some(1));
        assert!(!snap.has_direct);
    }

    #[tokio::test]
    async fn apply_event_marks_direct_when_present() {
        let mon = PatternMonitor::new();
        {
            let mut g = mon.inner.write().await;
            g.rows = Some(HashMap::new());
        }
        let direct = serde_json::json!({
            "payload": {
                "id": "CP-D",
                "kind": "direct",
                "plan_id": "PLAN-1",
                "lifecycle": "active"
            }
        });
        mon.apply_event("pattern_created", &direct.to_string()).await;
        let snap = mon.snapshot().await;
        assert_eq!(snap.active_count, Some(1));
        assert!(snap.has_direct);
    }

    #[tokio::test]
    async fn apply_event_removes_on_terminal_lifecycle() {
        let mon = PatternMonitor::new();
        {
            let mut g = mon.inner.write().await;
            g.rows = Some(HashMap::new());
        }
        let create = serde_json::json!({
            "payload": {
                "id": "CP-X",
                "kind": "swarm",
                "plan_id": "PLAN-1",
                "lifecycle": "active"
            }
        });
        mon.apply_event("pattern_created", &create.to_string()).await;
        let converge = serde_json::json!({
            "payload": {
                "id": "CP-X",
                "kind": "swarm",
                "plan_id": "PLAN-1",
                "lifecycle": "converged"
            }
        });
        mon.apply_event("pattern_lifecycle", &converge.to_string()).await;
        let snap = mon.snapshot().await;
        assert_eq!(snap.active_count, Some(0));
        assert!(!snap.has_direct);
    }

    #[tokio::test]
    async fn active_plan_ids_dedups_and_skips_blanks() {
        let mon = PatternMonitor::new();
        {
            let mut g = mon.inner.write().await;
            let mut map = HashMap::new();
            map.insert(
                "CP-1".into(),
                ActivePattern {
                    id: "CP-1".into(),
                    kind: "workflow".into(),
                    plan_id: "PLAN-A".into(),
                    lifecycle: "active".into(),
                },
            );
            map.insert(
                "CP-2".into(),
                ActivePattern {
                    id: "CP-2".into(),
                    kind: "graph".into(),
                    plan_id: "PLAN-A".into(),
                    lifecycle: "active".into(),
                },
            );
            map.insert(
                "CP-3".into(),
                ActivePattern {
                    id: "CP-3".into(),
                    kind: "swarm".into(),
                    plan_id: "PLAN-B".into(),
                    lifecycle: "active".into(),
                },
            );
            map.insert(
                "CP-4".into(),
                ActivePattern {
                    id: "CP-4".into(),
                    kind: "direct".into(),
                    plan_id: String::new(),
                    lifecycle: "active".into(),
                },
            );
            map.insert(
                "CP-5".into(),
                ActivePattern {
                    id: "CP-5".into(),
                    kind: "swarm".into(),
                    plan_id: "PLAN-C".into(),
                    lifecycle: "pending".into(),
                },
            );
            g.rows = Some(map);
        }
        let plans = mon.active_plan_ids().await;
        assert_eq!(plans, vec!["PLAN-A".to_string(), "PLAN-B".to_string()]);
    }

    #[tokio::test]
    async fn apply_event_removes_on_aborted_kind() {
        let mon = PatternMonitor::new();
        {
            let mut g = mon.inner.write().await;
            let mut map = HashMap::new();
            map.insert(
                "CP-Y".to_string(),
                ActivePattern {
                    id: "CP-Y".into(),
                    kind: "graph".into(),
                    plan_id: "PLAN-1".into(),
                    lifecycle: "active".into(),
                },
            );
            g.rows = Some(map);
        }
        let abort = serde_json::json!({
            "kind": "pattern_aborted",
            "entity_id": "CP-Y",
            "payload": { "id": "CP-Y" }
        });
        mon.apply_event("pattern_aborted", &abort.to_string()).await;
        let snap = mon.snapshot().await;
        assert_eq!(snap.active_count, Some(0));
    }
}
