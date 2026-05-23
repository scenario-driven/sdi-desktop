# sdi-desktop

SDI desktop application. Tauri 2 shell that bundles the `sdi-web` SPA, spawns
`sdid` as a sidecar child process, and points the WebView at the daemon's
HTTP origin so `fetch` and SSE land same-origin without an IPC relay.

Canonical spec: [`sdi-plugin/docs/PRD.md`](https://github.com/scenario-driven/sdi-plugin/blob/main/docs/PRD.md).

## OS-chrome surface (autonomy + pattern mirror)

The desktop shell mirrors the daemon's resolved state into the OS-native
chrome so the user can see L3 / L4 / L5 and the active CollaborationPattern at
a glance without opening the SPA:

| Surface | Behavior |
|---|---|
| **Window title** | `SDI · L3 / L4 / L5 / — | active patterns: N[ · direct ⚠]` updated every 3s from `/autonomy_policies/resolve` + `/patterns/active`. |
| **Tray tooltip + menu** | Same composite text plus a **Circuit breaker (demote to L3)** menu item that hits `/autonomy_policies/circuit_breaker` (D18). |
| **Global shortcut** | **Cmd+Shift+L** on macOS, **Ctrl+Shift+L** elsewhere — same circuit breaker, accessible regardless of which window has focus. |
| **Direct-pattern marker** | When any active pattern has `kind = 'direct'`, the title and tooltip render a red `direct ⚠` chip — the D23 / D27 anti-pattern badge surfaces all the way out to the OS. |

The pattern monitor hydrates once via `GET /patterns/active` on startup, then
listens to the `/events` SSE stream (`pattern_created`, `pattern_lifecycle`,
`pattern_aborted`, `pattern_converged`, `pattern_dissensus`) and falls back to
periodic HTTP polling if SSE drops.

All surfaces read state through the daemon's public HTTP API — no
back-channel — so what the tray reports is exactly what the SPA and the CLI
see.

## Sibling repositories

```
scenario-driven/      # wrapper (not a git repo)
├── sdi-plugin/       # Claude Code plugin + Rust workspace (cli, daemon, mcp, core, db)
├── sdi-web/          # dashboard SPA — bundled by this shell
├── sdi-desktop/      # this repo
└── sdi-docs/         # Astro/Starlight landing + bilingual guide site
```

One-way dependency:

```
sdi-desktop → sdid binary       (resolved via SDI_DAEMON_BIN env, plugin layout, XDG, PATH — see src/daemon.rs)
sdi-desktop → sdi-web/dist      (bundled at build time via tauri.conf.json `frontendDist`)
sdi-plugin  ──nothing──         sdi-desktop / sdi-web (no reverse dependency)
```

## Build

Prerequisites:
1. `sdi-web` cloned as a sibling directory (`../sdi-web`) with `pnpm install` run.
2. `sdid` binary available — either built from `sdi-plugin` (`cargo build -p sdi-daemon --release`) or installed under `~/.local/share/sdi/bin/`, or on `$PATH`.

```sh
cargo check          # offline check (does not require sdi-web/dist or sdid)
cargo tauri dev      # spawns sdi-web dev server (pnpm --dir ../sdi-web dev) + sdid sidecar
cargo tauri build    # bundles ../sdi-web/dist into the desktop binary
```

Override the daemon binary at runtime:

```sh
SDI_DAEMON_BIN=/custom/path/sdid cargo tauri dev
SDI_NO_AUTOSPAWN=1 cargo tauri dev   # rely on an already-running daemon
```

## License

MIT.
