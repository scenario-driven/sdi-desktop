# sdi-desktop

SDI desktop application. Tauri 2 shell that bundles the `sdi-web` SPA, spawns `sdid` as a sidecar child process, and points the WebView at the daemon's HTTP origin.

## Position in the SDI multi-repo layout

```
sdi/                  # wrapper (not a git repo)
├── sdi-plugin/       # Claude Code plugin + Rust workspace (cli, daemon, mcp, core, db)
├── sdi-web/          # dashboard SPA — bundled by this shell
└── sdi-desktop/      # this repo
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
