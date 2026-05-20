# CLAUDE.md — sdi-desktop

Self-contained AI context for **this repository** (`@scenario-driven/sdi-desktop`). Agents working in a fresh clone of this repo must operate from this document alone.

## Identity (do not paraphrase, do not soften)

This repository is the **SDI Tauri 2 desktop shell**. It bundles the `sdi-web` SPA, spawns the `sdid` daemon as a sidecar child process, and navigates a Tauri WebView to the daemon's HTTP origin so the bundled SPA's `fetch('/...')` reaches it same-origin. It is NOT a thin wrapper that re-implements daemon logic — `sdid` is the body, this is the shell.

## Repository position & contract

```
sdi-plugin/   # five Rust crates (cli, daemon, mcp, core, db) + Claude Code plugin shell
sdi-web/      # SPA (Vite/React 19) — bundled here as frontendDist
sdi-desktop/  # this repo
```

One-way dependency: `sdi-desktop → { sdid binary, sdi-web/dist }`. Nothing in `sdi-plugin` or `sdi-web` imports from this repo. See split decision artifact (Clawket: `SDI multi-repo 분리 — 인터페이스 계약 v1`).

### sdid resolution (do not change without notice)

`src/daemon.rs` resolves the daemon binary in this fixed order:

1. `SDI_DAEMON_BIN` env override (absolute path)
2. Sibling of the current executable (`<exe_dir>/sdid`)
3. `<plugin_root>/daemon/bin/sdid` (Claude Code plugin layout)
4. `$XDG_DATA_HOME/sdi/bin/sdid` (default: `~/.local/share/sdi/bin/sdid`)
5. Bare name `sdid` (PATH lookup)

`SDI_NO_AUTOSPAWN=1` disables spawn entirely.

### Frontend dist resolution

`tauri.conf.json` reads `frontendDist = "../sdi-web/dist"`. The wrapper layout assumption: `sdi-web` sits as a sibling of `sdi-desktop` under the `scenario-driven/` wrapper. If the user lays the two repos out elsewhere, override the path in their local `tauri.conf.json` — do not patch the canonical default.

## Standalone Cargo project

The Cargo.toml carries an empty `[workspace]` block deliberately. This isolates the crate from any parent workspace so it can sit next to `sdi-plugin` (which has its own workspace) without being absorbed.

It has **no source-level dependency on sdi-core / sdi-db / sdi-mcp**. The only deps are `tauri`, `serde`, `serde_json`, `tokio`, and the build-time `tauri-build`.

## Verification before claiming complete

```sh
cargo check                # offline structural check
cargo build                # full compile (needs system Tauri prerequisites)
```

`cargo tauri build` additionally requires `../sdi-web/dist` to exist (run `pnpm build` in sibling sdi-web).

## Commit & release

- Agents do not commit or push without explicit instruction.

## What to read next

1. `README.md` — public pitch + build prerequisites.
2. `src/lib.rs` — Tauri setup, transport contract comment (same-origin via daemon TCP port).
3. `src/daemon.rs` — `sdid` resolution + spawn.
4. `src/daemon_url.rs` — port handshake.
5. `tauri.conf.json` — bundle config.
