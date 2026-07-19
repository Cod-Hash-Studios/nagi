# Contributing to Nagi

Thanks for helping make agent work calmer, safer, and easier to understand.

Nagi is early and opinionated. The terminal stays central, missions remain
provider-neutral, and completion must be backed by evidence. Changes that move
those boundaries deserve a discussion before code.

## Choose the right place

- Reproducible bug: open an issue using the bug template.
- Feature, workflow, or design idea: start a GitHub Discussion.
- Security concern: follow `SECURITY.md` and do not open a public issue.
- Small documentation or test correction: a focused pull request is welcome.

Before working on a larger change, comment on the relevant issue or discussion
so effort is not duplicated.

Changes to provider contracts, plugin capabilities, persisted formats, public
APIs, or authority boundaries need an RFC before implementation. Follow
[`docs/rfcs/README.md`](docs/rfcs/README.md) and keep the RFC pull request
separate from its implementation.

## What makes a good contribution

- It solves one clear problem.
- It matches the existing architecture and interaction language.
- It keeps provider-specific behavior behind an adapter.
- It includes tests for the happy path and meaningful failure paths.
- It does not weaken permissions, journal integrity, worktree isolation, or
  proof requirements.
- The author can explain the behavior and tradeoffs without relying on generated
  code as the explanation.

Using coding agents is welcome. Submitting code nobody has reviewed or
understands is not.

## Development setup

Nagi currently requires Rust 1.96.1 and Zig 0.15.2.

```bash
git clone git@github.com:Cod-Hash-Studios/nagi.git
cd nagi
cargo build --locked
```

If Zig 0.15.2 is not the default binary on your machine, set `ZIG` explicitly
when running Cargo.

## Architecture boundaries

- `src/server/` is the single writer for durable runtime state.
- `src/mission/` owns mission journals, recovery, evidence, proof, and handoff.
- `src/managed_provider/` keeps provider-specific behavior behind adapters.
- `src/project_recipe/` owns bounded setup, services, resources, and cleanup.
- `src/app/api/plugins/` and `src/plugin_capabilities.rs` enforce plugin runtime
  and capability boundaries.
- `src/api/schema/` is the public socket contract; generated schema changes must
  be committed with their fixtures and docs.
- `src/ui/` renders projections and collects local consent. It must not become a
  second source of mission truth.

Read [`docs/architecture/authority.md`](docs/architecture/authority.md) before
changing consent, proof, handoff, or persistence. Read
[`docs/architecture/plugin-security.md`](docs/architecture/plugin-security.md)
before changing plugins or the marketplace.

## Before opening a pull request

Run the checks relevant to your change. For a broad Rust change, use:

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked -- --test-threads=1
python3 -m unittest scripts.test_brand_isolation scripts.test_fork_safety
bun test src/integration/assets/nagi-agent-state.test.ts
(cd workers/plugin-marketplace && bun test)
```

UI changes also run `python3 scripts/render_ui_goldens.py`. Runtime-boundary
changes run `python3 scripts/chaos_runtime.py`. Documentation changes run the
`release-docs-check` recipe in `justfile`. Explain every skipped check in the
pull request.

Keep commits atomic and use lowercase Conventional Commit subjects in English:

```text
fix(runtime): reject stale response token
```

Reference related issues in the commit body with `refs #123`. Avoid automatic
closing keywords because release tracking may close issues separately.

## Documentation

Update the public API schema and unreleased docs when a user-facing contract
changes. Do not edit historical changelog entries to rewrite fork history.

## License and provenance

Contributions are licensed under `AGPL-3.0-or-later`. Nagi is derived from
Herdr, and the required attribution is recorded in `FORK.md`.

Participation is governed by [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md), and
maintainer responsibilities are described in [`GOVERNANCE.md`](GOVERNANCE.md).
