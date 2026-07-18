# Nagi contributor guide

Nagi is a terminal-native mission runtime for coding agents. It keeps the speed
and composability of a terminal while adding durable missions, explicit closure
criteria, isolated worktrees, attention routing, and proof-backed completion.

## Product boundaries

- The terminal remains the primary interface. Do not add a graphical chat UI.
- A mission is more than a prompt. Preserve its objective, closure contract,
  run isolation, journal, evidence, and lifecycle as one coherent model.
- Provider-specific behavior belongs behind the managed provider boundary.
  Shared mission semantics must not depend on one vendor's protocol.
- Keep Nagi approachable. Prefer clear labels, progressive disclosure, and safe
  defaults over expert-only shortcuts.
- Never claim a mission is complete from provider output alone. Completion must
  be backed by the configured checks and fresh workspace evidence.

## Architecture

- `src/mission/` owns durable mission state, claims, evidence, proof, and replay.
- `src/managed_provider/` owns coding-agent protocol adapters.
- `src/server/` owns the single-writer runtime and mission API bridge.
- `src/api/` owns the public socket schema and wire contract.
- `src/app/` owns the TUI client. Do not move runtime authority into the UI.
- Platform-specific behavior stays in `src/platform/` or behind narrow
  compile-gated boundaries.

Before changing persisted state, API fields, identifiers, journal records, or
handoff behavior, identify the compatibility contract and add an adversarial
test for it.

## Engineering rules

- Use Rust 1.96.1 and Zig 0.15.2, as pinned by the repository.
- Keep changes typed, testable, and scoped. Avoid speculative refactors.
- Do not use `unwrap()` in production code. Return typed errors and use
  `tracing` for diagnostics.
- Treat IDs as opaque strings. Never derive them from display order.
- Preserve private runtime permissions and fail closed on symlinks, weak file
  modes, stale response tokens, journal corruption, and lease conflicts.
- Provider payloads and user answers must not leak into durable logs unless the
  public contract explicitly requires that data.
- Do not weaken a safety boundary to make a flaky integration pass.

## Brand and runtime isolation

- The binary, package, config directory, sockets, logs, environment variables,
  integration assets, and plugin manifests use the `nagi` or `NAGI` namespace.
- Do not reintroduce the legacy fork namespace outside attribution, license,
  migration notes, or an explicit compatibility surface.
- Keep `FORK.md`, `LICENSE`, and upstream copyright notices intact.

## Verification

Run the narrowest relevant test while iterating. Before a broad change is
complete, run:

```bash
cargo fmt --check
cargo test --locked -- --test-threads=1
python3 -m unittest \
  scripts.test_brand_isolation \
  scripts.test_fork_safety
bun test src/integration/assets/nagi-agent-state.test.ts
```

The vendored terminal parser requires `ZIG` to resolve to Zig 0.15.2. Serial
Rust tests are the authoritative local baseline until the inherited timing
tests have been fully isolated.

## Release safety

Automated publishing, self-update, remote binary download, and upstream write
workflows remain disabled until the repository has its own signed release
channel and a reviewed release-readiness artifact. Do not bypass those gates.

## Commits

Use lowercase Conventional Commit subjects in English. Keep commits atomic and
do not mix unrelated cleanup with product work.
