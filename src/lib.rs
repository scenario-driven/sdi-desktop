//! SDI desktop shell. Hosts the SDI dashboard SPA inside a Tauri window and spawns
//! `sdid` as a child process if it's not already running, mirroring Clawket's
//! desktop architecture so the user experience is the same: the window is
//! the dashboard; the daemon is local; data lives in XDG.
//!
//! Transport contract: the SDI daemon binds an ephemeral TCP port on 127.0.0.1
//! and serves both the HTTP API and the SPA bundle from the same origin (see
//! `crates/daemon/src/router/mod.rs`). To preserve same-origin semantics in
//! the desktop window the shell, after confirming the daemon is up, navigates
//! the WebView to `http://127.0.0.1:<port>/`. Without that the bundled SPA's
//! relative `fetch('/projects')` would dispatch to `tauri://localhost` and
//! never reach the daemon. Cross-origin/IPC bridging is intentionally avoided
//! to keep one transport story shared with the browser flow.
//!
//! Autonomy surface (D14/D17/D18). The shell mirrors the daemon's resolved
//! autonomy mode into the window title and a tray-icon header, and exposes
//! D18's circuit breaker via both a tray menu item and a global shortcut
//! (Cmd+Shift+L / Ctrl+Shift+L). All state is read through the daemon's
//! HTTP API — no back-channel — so the indicator is exactly what the SPA
//! and the CLI see.
//!
//! Pattern surface (v0.5 — D22 / D23 / D26). On top of the autonomy
//! indicator the shell renders an **active CollaborationPattern badge** in
//! the tray tooltip and the window title, with a red marker when any
//! active pattern is `kind = 'direct'` (the anti-pattern marker per D23).
//! Same transport story as autonomy: HTTP for the seed, SSE for deltas,
//! no IPC back-channel.

mod autonomy;
mod daemon;
mod daemon_url;
mod patterns;

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{
    Builder as GlobalShortcutBuilder, Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
};

use crate::autonomy::AutonomyMode;
use crate::patterns::{PatternBadge, PatternMonitor};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const PROJECT_POLL_BUDGET: Duration = Duration::from_secs(15);
const MENU_ITEM_MODE: &str = "sdi-mode";
const MENU_ITEM_BREAKER: &str = "sdi-breaker";
const MENU_ITEM_QUIT: &str = "sdi-quit";

#[derive(Default)]
struct DaemonState {
    handle: Mutex<Option<daemon::DaemonHandle>>,
}

/// Cached daemon port + project id so the menu handler and the global
/// shortcut can fire the circuit breaker without waiting on the polling
/// loop's next tick. `None` until the daemon is up and a project exists.
#[derive(Default)]
struct AutonomyState {
    target: Mutex<Option<AutonomyTarget>>,
}

#[derive(Clone)]
struct AutonomyTarget {
    port: u16,
    project_id: String,
}

/// Shared two-channel surface state. Both the autonomy poller and the
/// pattern monitor publish into this; the [`render_surface`] helper
/// composes the final tooltip / title string from whatever the latest
/// snapshot is. Lives behind a single `Mutex` because writes are rare
/// (every 3s for autonomy, on every SSE delta for patterns) and the
/// `tauri::WebviewWindow::set_title` / `TrayIcon::set_tooltip` calls
/// happen synchronously on the publisher's thread.
struct SurfaceState {
    autonomy: Mutex<AutonomyMode>,
    pattern: Mutex<PatternBadge>,
}

impl SurfaceState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            autonomy: Mutex::new(AutonomyMode::Unknown),
            pattern: Mutex::new(PatternBadge::default()),
        })
    }

    fn set_autonomy(&self, mode: AutonomyMode) {
        *self.autonomy.lock().expect("autonomy surface lock poisoned") = mode;
    }

    fn set_pattern(&self, badge: PatternBadge) {
        *self.pattern.lock().expect("pattern surface lock poisoned") = badge;
    }

    fn snapshot(&self) -> (AutonomyMode, PatternBadge) {
        let m = *self.autonomy.lock().expect("autonomy surface lock poisoned");
        let p = *self.pattern.lock().expect("pattern surface lock poisoned");
        (m, p)
    }
}

fn compose_title(mode: AutonomyMode, badge: PatternBadge) -> String {
    format!("SDI · {}{}", mode.label(), badge.title_suffix())
}

fn compose_tooltip(mode: AutonomyMode, badge: PatternBadge) -> String {
    format!("SDI — autonomy: {}{}", mode.label(), badge.tooltip_suffix())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // D18 circuit-breaker hotkey. Cmd+Shift+L on macOS, Ctrl+Shift+L on
    // every other desktop platform — same mental model as the web app's
    // "Circuit breaker" button so muscle memory transfers.
    #[cfg(target_os = "macos")]
    let breaker_shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyL);
    #[cfg(not(target_os = "macos"))]
    let breaker_shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyL);

    tauri::Builder::default()
        .manage(DaemonState::default())
        .manage(AutonomyState::default())
        .plugin(
            GlobalShortcutBuilder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if *shortcut == breaker_shortcut {
                        fire_circuit_breaker(app.clone(), "sdi-desktop · global shortcut");
                    }
                })
                .build(),
        )
        .setup(move |app| {
            // 1. Spawn the daemon. If `sdid` is already running, its flock
            //    rejects the second instance — we tolerate that and verify
            //    liveness via the /health ping below.
            match daemon::spawn() {
                Ok(handle) => {
                    eprintln!(
                        "sdi-desktop: daemon spawned bin={} pid={}",
                        handle.bin.display(),
                        handle.child.id()
                    );
                    let state: tauri::State<DaemonState> = app.state();
                    let mut guard = state.handle.lock().expect("daemon lock poisoned");
                    *guard = Some(handle);
                }
                Err(e) => {
                    eprintln!(
                        "sdi-desktop: daemon spawn skipped/failed: {e}. \
                         Will attempt to use an already-running daemon."
                    );
                }
            }

            // 2. Build the tray icon up front so the user has a visible
            //    L3/L4/L5 indicator even before the first /health succeeds.
            //    The label flips to the resolved mode once the poller below
            //    learns it.
            let app_handle = app.handle().clone();
            let mode_item = MenuItem::with_id(
                &app_handle,
                MENU_ITEM_MODE,
                format!("Autonomy: {}", AutonomyMode::Unknown.label()),
                false,
                None::<&str>,
            )?;
            let breaker_item = MenuItem::with_id(
                &app_handle,
                MENU_ITEM_BREAKER,
                "Circuit breaker (demote to L3)",
                true,
                Some(breaker_shortcut_label()),
            )?;
            let quit_item = MenuItem::with_id(
                &app_handle,
                MENU_ITEM_QUIT,
                "Quit SDI",
                true,
                None::<&str>,
            )?;
            let separator = PredefinedMenuItem::separator(&app_handle)?;
            let menu = Menu::with_items(
                &app_handle,
                &[&mode_item, &separator, &breaker_item, &separator, &quit_item],
            )?;
            let mut tray_builder = TrayIconBuilder::new()
                .tooltip("SDI")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    MENU_ITEM_BREAKER => {
                        fire_circuit_breaker(app.clone(), "sdi-desktop · tray menu");
                    }
                    MENU_ITEM_QUIT => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray_builder = tray_builder.icon(icon);
            }
            let tray: TrayIcon = tray_builder.build(app)?;

            // 3. Register the D18 global shortcut now that the plugin is
            //    initialized. Failure is logged but never fatal — the user
            //    still has the tray menu item and the in-app button.
            if let Err(e) = app_handle.global_shortcut().register(breaker_shortcut) {
                eprintln!(
                    "sdi-desktop: failed to register circuit-breaker shortcut \
                     ({}): {e}",
                    breaker_shortcut_label()
                );
            }

            // 4. Wait for the daemon's port file, ping /health, then point the
            //    WebView at the daemon's HTTP origin so fetch / SSE land
            //    same-origin without an IPC relay. Once both succeed, start
            //    the autonomy poller AND the pattern monitor — both feed
            //    the same SurfaceState so the tray tooltip / window title
            //    show a coherent composed string.
            let window = app
                .get_webview_window("main")
                .expect("main webview window must exist");
            let surface = SurfaceState::new();
            let poller_handle = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let port = match daemon_url::wait_for_port().await {
                    Some(p) => p,
                    None => {
                        eprintln!(
                            "sdi-desktop: no sdid port file found at {} — SPA \
                             will render the disconnected error state",
                            daemon_url::port_file_path().display()
                        );
                        return;
                    }
                };
                match daemon_url::ping_health(port).await {
                    Ok(status) => eprintln!(
                        "sdi-desktop: daemon /health ok port={port} status={status}"
                    ),
                    Err(e) => eprintln!(
                        "sdi-desktop: daemon /health failed port={port} err={e}"
                    ),
                }
                let origin = daemon_url::origin(port);
                match tauri::Url::parse(&origin) {
                    Ok(url) => {
                        if let Err(e) = window.navigate(url) {
                            eprintln!("sdi-desktop: navigate({origin}) failed: {e}");
                        } else {
                            eprintln!("sdi-desktop: WebView navigated to {origin}");
                        }
                    }
                    Err(e) => eprintln!("sdi-desktop: bad daemon origin {origin}: {e}"),
                }

                // Pattern monitor (v0.5 tray badge). Runs concurrently
                // with the autonomy poller — both write into the shared
                // SurfaceState and the render helper composes the final
                // tooltip / title from whichever snapshot is current.
                let monitor = PatternMonitor::new();
                monitor.bootstrap(port).await;
                let initial = monitor.snapshot().await;
                surface.set_pattern(initial);
                render_surface(&window, &tray, &surface);

                let pattern_window = window.clone();
                let pattern_tray = tray.clone();
                let pattern_surface = Arc::clone(&surface);
                let pattern_monitor = monitor.clone();
                tauri::async_runtime::spawn(async move {
                    pattern_monitor
                        .run_events_loop(port, move |badge| {
                            pattern_surface.set_pattern(badge);
                            render_surface(&pattern_window, &pattern_tray, &pattern_surface);
                        })
                        .await;
                });

                run_autonomy_loop(
                    poller_handle,
                    port,
                    window,
                    tray,
                    mode_item,
                    surface,
                )
                .await;
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Poll the daemon's resolved autonomy mode and mirror it into the window
/// title and the tray menu. Runs forever (cancelled implicitly when the
/// Tauri event loop tears down the async runtime on app exit).
async fn run_autonomy_loop(
    app: AppHandle,
    port: u16,
    window: tauri::WebviewWindow,
    tray: TrayIcon,
    mode_item: MenuItem<tauri::Wry>,
    surface: Arc<SurfaceState>,
) {
    let project_id = match wait_for_project(port).await {
        Some(p) => p,
        None => {
            eprintln!(
                "sdi-desktop: no project found within {}s — autonomy indicator \
                 will remain '—' until a project is created",
                PROJECT_POLL_BUDGET.as_secs(),
            );
            return;
        }
    };
    {
        let state: tauri::State<AutonomyState> = app.state();
        let mut guard = state.target.lock().expect("autonomy lock poisoned");
        *guard = Some(AutonomyTarget {
            port,
            project_id: project_id.clone(),
        });
    }

    let mut last: Option<AutonomyMode> = None;
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let mode = autonomy::resolve_mode(port, &project_id).await;
        if Some(mode) == last {
            continue;
        }
        last = Some(mode);
        surface.set_autonomy(mode);
        let menu_label = format!("Autonomy: {}", mode.label());
        if let Err(e) = mode_item.set_text(&menu_label) {
            eprintln!("sdi-desktop: tray set_text({menu_label}) failed: {e}");
        }
        render_surface(&window, &tray, &surface);
    }
}

/// Compose the autonomy + pattern surfaces into the window title and tray
/// tooltip. Called whenever either monitor publishes a new snapshot —
/// idempotent and cheap (a couple of string allocations + two Tauri
/// setter calls).
fn render_surface(window: &tauri::WebviewWindow, tray: &TrayIcon, surface: &SurfaceState) {
    let (mode, badge) = surface.snapshot();
    let title = compose_title(mode, badge);
    if let Err(e) = window.set_title(&title) {
        eprintln!("sdi-desktop: set_title({title}) failed: {e}");
    }
    let tooltip = compose_tooltip(mode, badge);
    if let Err(e) = tray.set_tooltip(Some(&tooltip)) {
        eprintln!("sdi-desktop: tray set_tooltip({tooltip}) failed: {e}");
    }
}

/// Spin until the daemon reports at least one project. New installs commonly
/// land here on first launch — we wait up to [`PROJECT_POLL_BUDGET`] so the
/// poller can latch on without forcing the user to relaunch the app.
async fn wait_for_project(port: u16) -> Option<String> {
    let deadline = tokio::time::Instant::now() + PROJECT_POLL_BUDGET;
    loop {
        if let Some(pid) = autonomy::first_project_id(port).await {
            return Some(pid);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn fire_circuit_breaker(app: AppHandle, reason: &'static str) {
    let target = {
        let state: tauri::State<AutonomyState> = app.state();
        let guard = state.target.lock().expect("autonomy lock poisoned");
        guard.clone()
    };
    let Some(t) = target else {
        eprintln!(
            "sdi-desktop: circuit breaker invoked before a project was \
             resolved — daemon not ready yet, ignoring"
        );
        return;
    };
    tauri::async_runtime::spawn(async move {
        match autonomy::circuit_breaker(t.port, &t.project_id, reason).await {
            Ok(n) => eprintln!(
                "sdi-desktop: circuit breaker fired ({reason}) — {n} policies \
                 demoted to L3"
            ),
            Err(e) => eprintln!("sdi-desktop: circuit breaker failed: {e}"),
        }
    });
}

fn breaker_shortcut_label() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "Cmd+Shift+L"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Ctrl+Shift+L"
    }
}
