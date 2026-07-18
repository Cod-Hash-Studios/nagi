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

## Before opening a pull request

Run the checks relevant to your change. For a broad Rust change, use:

```bash
cargo fmt --check
cargo test --locked -- --test-threads=1
python3 -m unittest \
  scripts.test_brand_isolation \
  scripts.test_fork_safety
bun test src/integration/assets/nagi-agent-state.test.ts
```

Explain any skipped check in the pull request.

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
