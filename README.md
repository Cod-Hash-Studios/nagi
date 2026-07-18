<p align="center">
  <img src="assets/brand/nagi-lockup.svg" alt="Nagi" width="680" />
</p>

<h3 align="center">Keep coding agents running. Keep one terminal in control.</h3>

<p align="center">
  A fast, persistent terminal workspace for Codex, Claude Code, OpenCode, and the shells around them.
</p>

<p align="center">
  <a href="https://github.com/Cod-Hash-Studios/nagi/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Cod-Hash-Studios/nagi/ci.yml?branch=main&style=flat-square&label=build&labelColor=F7F1E3&color=80CFC4" alt="Build status" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-23395B?style=flat-square&labelColor=F7F1E3" alt="AGPL 3.0 license" /></a>
  <img src="https://img.shields.io/badge/status-early%20access-EF624D?style=flat-square&labelColor=F7F1E3" alt="Early access" />
  <img src="https://img.shields.io/badge/Rust-1.96-23395B?style=flat-square&labelColor=F7F1E3" alt="Rust 1.96" />
</p>

<p align="center">
  <img src="assets/screenshots/nagi-agent-cockpit.gif" alt="The real Nagi agent cockpit filtering blocked, working, and done agents" width="100%" />
</p>

<p align="center"><sub>Real Nagi build. Deterministic demo data. No mock interface.</sub></p>

Nagi is a native terminal multiplexer with an agent-aware cockpit. Your panes
keep running when the terminal disappears. Reattach locally, over SSH, or from
a phone. Press `Ctrl+B`, then `G`, to see what needs you and jump straight to it.

| Stay running | See attention | Automate it |
|---|---|---|
| Persistent panes, tabs, and workspaces | Blocked, working, idle, and done at a glance | Typed CLI and Unix socket API |
| Detach and reattach over SSH | Search and filter every live agent pane | Durable mission journal and worktree claims |
| One Rust binary, no Electron | Mouse and keyboard navigation | Headless server for scripts and agents |

## Why Nagi

Starting agents is easy. Keeping track of them is not.

Nagi keeps real terminal processes, their current state, and the workspace they
own on one calm surface. It does not replace Codex, Claude Code, or OpenCode. It
gives them somewhere reliable to run.

## Works today

| Surface | Status |
|---|---|
| Persistent terminal sessions, splits, tabs, mouse, SSH reattach | working |
| Agent cockpit with search, state filters, counts, and direct switching | working |
| CLI and Unix socket API | working |
| Mission create, list, get, configure, journal replay, and worktree claims | working |
| Managed Codex and Claude Code mission start | early read-only path |
| Managed OpenCode mission start | actor tested, final wiring pending |
| Proof execution, provider consent UI, and mission closure | in progress |
| Signed binaries and public release channel | not available yet |

The cockpit is real and usable now. The deeper mission-to-proof loop is an early
preview, and this README labels it that way on purpose.

## Agent compatibility

| Agent | Runs in Nagi | Managed mission path |
|---|:---:|---|
| Codex | yes | early read-only path |
| Claude Code | yes | early read-only path |
| OpenCode | yes | start wiring pending |

Any terminal program can run in a pane. The managed mission column is narrower:
it means Nagi understands that provider's lifecycle instead of merely hosting
its process.

## Build from source

The mission runtime currently targets Unix systems. You need Rust `1.96.1` and
Zig `0.15.2`.

```bash
git clone https://github.com/Cod-Hash-Studios/nagi.git
cd nagi

zig version  # 0.15.2
cargo build --release --locked
./target/release/nagi
```

Nagi uses its own config, runtime paths, sockets, logs, and environment
variables. It does not reuse an existing Herdr session.

## Run the cockpit demo

Build Nagi, then start an isolated development server:

```bash
cargo run -- server
```

In another terminal:

```bash
scripts/seed_navigator_demo.sh
cargo run
```

Open the cockpit with `Ctrl+B`, then `G`. Use `b`, `w`, `i`, `d`, and `a` to
filter by state. The script inserts simulated status data into real Nagi
workspaces and panes, and refuses to touch the main socket unless explicitly
allowed.

## API

```bash
nagi api schema
nagi api snapshot
```

The generated mission schema lives at
[`docs/next/api/nagi-api.schema.json`](docs/next/api/nagi-api.schema.json).

<details>
<summary><strong>Architecture in 20 seconds</strong></summary>

```text
terminal clients
      │
      ▼
single-writer Nagi server
      ├── panes, tabs, sessions, render streams
      ├── mission journal, worktree claims, attention
      └── managed provider adapters
             ├── Codex
             ├── Claude Code
             └── OpenCode
```

The TUI is a client of the server. Mission truth stays in the durable runtime,
so SSH clients, headless automation, and future interfaces share one contract.

</details>

## Contributing

Read [`AGENTS.md`](AGENTS.md) before changing runtime contracts and
[`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a pull request.

```bash
cargo fmt --check
cargo test --locked -- --test-threads=1
python3 -m unittest scripts.test_brand_isolation scripts.test_fork_safety
bun test src/integration/assets/nagi-agent-state.test.ts
```

## Provenance

Nagi is an independent derivative of
[Herdr](https://github.com/ogulcancelik/herdr), starting from `v0.7.4` at
`50aaa2ec046ee26ff407c20f49de496f522512a8`. Required copyright and attribution
notices remain intact. See [`FORK.md`](FORK.md).

Nagi is licensed under [`AGPL-3.0-or-later`](LICENSE). The separate commercial
license offered upstream is not granted by this repository.

<p align="center">
  <strong>Less tab hunting. More finished work.</strong><br />
  <sub>凪</sub>
</p>
