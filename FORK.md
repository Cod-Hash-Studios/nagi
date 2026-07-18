# Fork notice

This repository is an independent derivative of
[Herdr](https://github.com/ogulcancelik/herdr), a terminal workspace manager
created by Can Çelik.

The fork started from:

- upstream tag: `v0.7.4`;
- upstream commit: `50aaa2ec046ee26ff407c20f49de496f522512a8`;
- upstream license: `AGPL-3.0-or-later`.

The complete upstream history is retained in Git. The `upstream` remote is
fetch-only in the development checkout so changes cannot accidentally be
pushed to the original project.

## License

The derivative program remains licensed under `AGPL-3.0-or-later`. The full
license is in [LICENSE](LICENSE). Existing copyright and attribution notices
must be preserved. New distributions must make the exact Corresponding Source
for the distributed binary available as required by the license.

Herdr also offers separate commercial licensing. That option belongs to the
upstream copyright holder and is not granted by this repository.

## Current status

The fork is now developed as **Nagi** by Cod'Hash Studios. It uses its own
binary, package, config directory, sockets, logs, environment variables, and
integration namespace. Upstream publishing workflows, update channels, and
automatic remote binary downloads remain disabled until Nagi has its own
security review and signed release-readiness artifact.

No public release is an MVP. The first public release is intended to include
the complete mission-to-proof workflow described in the local product plan.

## Reproducible local baseline

Herdr `v0.7.4` requires Rust `1.96.1` and Zig `0.15.2`. On macOS, use the
Homebrew `zig@0.15` bottle because it includes the current macOS compatibility
patches:

```bash
brew install zig@0.15
ZIG=/opt/homebrew/opt/zig@0.15/bin/zig \
  cargo test --bin nagi -- --test-threads=1
```

The pinned upstream baseline contains 2,621 unit tests. They pass serially on
the initial macOS development machine. The fully parallel suite exposes
upstream timing and shared-global-state flakes, so serial execution remains the
authoritative baseline until those tests are isolated.
