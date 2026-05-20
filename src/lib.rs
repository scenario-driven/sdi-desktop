//! SDI desktop shell. Hosts the sdi-web SPA inside a Tauri window and spawns
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

mod daemon;
mod daemon_url;

use std::sync::Mutex;
use tauri::Manager;

#[derive(Default)]
struct DaemonState {
    handle: Mutex<Option<daemon::DaemonHandle>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(DaemonState::default())
        .setup(|app| {
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

            // 2. Wait for the daemon's port file, ping /health, then point the
            //    WebView at the daemon's HTTP origin so fetch / SSE land
            //    same-origin without an IPC relay.
            let window = app
                .get_webview_window("main")
                .expect("main webview window must exist");
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
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
