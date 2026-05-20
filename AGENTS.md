# AGENTS.md — Alleycat (upstream `dnakov/alleycat` working copy)

> Living documentation for AI agents working on this repo.
> Update this file when you discover gotchas, conventions, or non-obvious
> behavior that future agents should know about.

## Project overview

This is a working copy of the **upstream** Alleycat repo (`dnakov/alleycat`). Alleycat is a Rust workspace for an Iroh-backed daemon that multiplexes local coding-agent CLIs (Codex, Pi, Amp, OpenCode, Claude, Factory Droid, Hermes, etc.) over a single QUIC connection for paired clients.

This tree is used locally to run a **PWA-serving variant** of the daemon for the [`litter-pwa`](https://github.com/dnakov/litter) project — the daemon binds an HTTP listener and serves a static SPA bundle alongside the normal iroh surface.

## Key commands

- **Build:** `cargo build`
- **Test:** `cargo test -p alleycat`
- **Run PWA-serve mode (local convention):**
  ```bash
  target/debug/alleycat serve \
    --serve-pwa \
    --listen 127.0.0.1:5851 \
    --pwa-dir /home/shuv/repos/litter-pwa/apps/web/dist
  ```
- **Install (do not run unprompted — see "Parallel local alleycat instances" below):** `cargo install --locked --path crates/alleycat`

## Operational notes

- `alleycat serve` is long-running; never start it via plain `bash`. Use the existing tmux session `alleycat-pwa` on socket `/tmp/tmux-skill-sockets/litter-pwa-alleycat.sock` (window `serve`), or an `interactive_shell` session.
- This tree's running instance is launched with an isolated home so it does **not** collide with the systemd-managed alleycat on this box:
  ```bash
  HOME=/tmp/litter-pwa-alleycat-home \
  XDG_CONFIG_HOME=/tmp/litter-pwa-alleycat-home/.config \
  XDG_STATE_HOME=/tmp/litter-pwa-alleycat-home/.local/state \
  XDG_RUNTIME_DIR=/tmp/litter-pwa-alleycat-home/.run \
  exec target/debug/alleycat serve --serve-pwa --listen 127.0.0.1:5851 --pwa-dir <dir>
  ```
  Preserve this `HOME`/XDG isolation on every restart, otherwise the upstream instance will clobber the production daemon's `~/.config/alleycat/host.toml` and `~/.local/state/alleycat/`.
- Control socket for this instance: `/tmp/litter-pwa-alleycat-home/.run/alleycat-<hash>/control.sock`. Run `HOME=/tmp/litter-pwa-alleycat-home target/debug/alleycat status` to query it.
- Logs: `/tmp/litter-pwa-alleycat-home/.local/state/alleycat/logs/daemon.log.<date>`. Expect routine `QADv6 NetworkUnreachable` IPv6 relay-probe warnings every ~75s on this host (no IPv6 routing); harmless.
- The PWA static bundle path is resolved at process start and not watched — rebuild `litter-pwa` and restart this daemon to pick up new SPA assets. The Rust code itself is *not* live-reloaded; rebuild + restart for every change.
- The running binary's `exe` link will read `(deleted)` after any rebuild because the on-disk file is replaced. That means the process is **stale** — it is still running the previous image until you restart it. Use `stat target/debug/alleycat` vs `ls -l /proc/<pid>/exe` to detect drift.

## Parallel local alleycat instances

Two distinct alleycat repos run on this machine simultaneously under the same binary name. Always disambiguate before restarting, killing, installing, or editing — they share zero state but share the binary name and several conventions.

| Fact | This repo (dnakov upstream) | Sibling (shuv1337 fork) |
|---|---|---|
| Repo path | `/home/shuv/repos/alleycat-fork` | `/home/shuv/repos/alleycat` |
| Remote | `git@github.com:dnakov/alleycat.git` (origin) | `git@github.com:shuv1337/alleycat.git` (origin), `dnakov/alleycat` (upstream) |
| Binary in use | `target/debug/alleycat` (in-tree debug build) | `~/.cargo/bin/alleycat` (release, `cargo install`ed from the sibling tree) |
| Launch | tmux session `alleycat-pwa` on socket `/tmp/tmux-skill-sockets/litter-pwa-alleycat.sock`, window `serve` | systemd user unit `alleycat.service` |
| Ports | `127.0.0.1:5851` (`--serve-pwa`) | `127.0.0.1:8390` (codex bridge), `127.0.0.1:8391` (provider router) |
| `HOME` / state | `/tmp/litter-pwa-alleycat-home/{,.config,.local/state,.run}` (isolated `XDG_*`) | `~`, `~/.config/alleycat/`, `~/.local/state/alleycat/` |
| Identify by cgroup | none (parent is the tmux server PID) | `/user.slice/.../app.slice/alleycat.service` |
| Unique CLI flags | `--serve-pwa` and `--pwa-dir` (upstream-only) | none unusual; runs `serve` default |

Rules:

- **Never `pkill alleycat` or blanket-kill by name** — it hits both. Disambiguate by cgroup, `HOME` env (`tr '\0' '\n' < /proc/<pid>/environ | grep HOME`), `cwd`, or unique flags (`--serve-pwa` indicates this repo's instance).
- **Do not `cargo install` from this tree.** It overwrites `~/.cargo/bin/alleycat`, which is the binary `systemctl --user restart alleycat.service` re-executes. This tree is meant to run from `target/debug/`, not installed. If you accidentally install, reinstall from the sibling: `cd /home/shuv/repos/alleycat && cargo install --locked --path crates/alleycat`.
- `systemctl --user restart alleycat.service` does **not** affect this tree's instance. Restarting this instance means Ctrl-C in the `alleycat-pwa:serve` tmux pane (or `tmux send-keys`) and re-running the launch command above.
- The sibling tree is the source of truth for `~/.config/alleycat/host.toml` and `~/.local/state/alleycat/`. Do not point this debug build at those paths — keep `HOME=/tmp/litter-pwa-alleycat-home` on every invocation.
- The sibling implements a Codex Desktop "provider-router" experiment on port 8391 with paths like `/agent/claude` and `/agent/opencode`. That code does **not** exist in this upstream tree.

## Project-local context

- Working PWA assets live at `/home/shuv/repos/litter-pwa/apps/web/dist`. Build them in that repo, not here.
- Local working state of this repo is typically ahead of `origin/main` by 1 commit with `crates/alleycat/src/http_server.rs` and `crates/pi-bridge/src/handlers/thread.rs` modified (PWA-serving + pi handler tweaks). Check `git status` before assuming the tree is clean.
