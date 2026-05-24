# Goal: Codex Remote-Control T2 Native

Implement native Codex remote-control supervision in Alleycat as an isolated new Rust workspace crate, then wire it into the existing Codex UnixProxy lifecycle without re-enabling the orphan-prone `CodexMode::UnixDaemon` path. The T1 `codex-rc-keeper.service` remains the rollback path until the native supervisor is installed, verified, and proven across an Alleycat restart.

Use `goals/codex-remote-control-t2-native/facts.md` as the shared understanding for scope, constraints, status expectations, cutover policy, validation, and push policy.

Use `goals/codex-remote-control-t2-native/plan.md` as the execution plan. The plan has passed Plannotator gate review.

Done means the native crate and thin Alleycat integration are implemented, automated validation passes, production cutover is performed only after explicit approval, native remote control reaches `connected` after at least one Alleycat restart, rollback is documented, implementation work is committed locally, and nothing is pushed to `origin/main` without a separate explicit decision.
