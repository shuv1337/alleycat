# NOTICE

This repository is a fork of dnakov/alleycat
(https://github.com/dnakov/alleycat), pinned at commit
8d65ed006cdefb3467865e6ed0b49b6919edb7c5 taken at 2026-05-18.

Upstream license: GPL-3.0-only.
Fork license:     GPL-3.0-only (inherited).

Files added in this fork:
- crates/alleycat/src/http_server.rs
- crates/alleycat/tests/http_server.rs
- scripts/wscat-binary.mjs

Files modified in this fork:
- Cargo.toml
- crates/alleycat/Cargo.toml
- crates/alleycat/src/agent_manifest.rs
- crates/alleycat/src/agents.rs
- crates/alleycat/src/cli/status.rs
- crates/alleycat/src/daemon/control.rs
- crates/alleycat/src/daemon/mod.rs
- crates/alleycat/src/lib.rs

The fork exists to support an external desktop PWA frontend
(github.com/shuv1337/litter-pwa). It is intended to be offered
upstream as a pull request once stable.
