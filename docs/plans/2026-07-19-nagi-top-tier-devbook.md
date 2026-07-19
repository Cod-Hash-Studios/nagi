# Nagi v1: Best-in-Class Devbook

> **Implementation note:** execute this document task by task with the
> `executing-plans`, test-driven development and verification workflows. A
> milestone is not a release. Nagi v1 ships only when every release gate at the
> end of this document is green.

**Date:** 2026-07-19  
**Status:** implementation blueprint  
**Target:** macOS and Linux as supported platforms, Windows as an explicitly
labelled terminal-multiplexer beta until mission parity is proven.

## Implementation checkpoint, not a release claim

The current development branch now contains a coherent first product loop:

| Lane | Implemented now |
|---|---|
| Mission contract | creation, criteria, declared proof, durable journal, recovery, attention, and closure |
| Provider path | Codex, Claude Code, and OpenCode registry, conformance fixtures, guided launch, and isolated worktrees |
| Human authority | public socket launches are read-only; only the local TUI can confirm workspace writes or provider permissions |
| Proof | bounded execution, fresh rerun on close, content-addressed evidence packs, API, and CLI |
| Cockpit | responsive mission list, inspector, proof review, command palette, and structured questions |
| Visual system | semantic tokens, Nagi Dawn/Night, theme files, Ghostty import, density, and compact layouts |
| First run | environment doctor and mission-first onboarding |
| Landing | product video in the hero, real product media, responsive layout, reduced-motion handling, and browser QA |
| Project isolation | strict `.nagi/project.toml`, explicit setup/check/cleanup consent, exact ignored-file copying, collision-free port leases, bounded service supervision, health checks, restart adoption, and digest-bound orphan cleanup |
| Provider handoff | redacted Git snapshot artifact, digest binding, API, and `nagi mission handoff ... --preview` |
| Plugin v2 runtime | strict version routing, bounded WASI components, capability grants, checksum-bound locks, revocation, fail-closed updates, and explicit native trust |

This does **not** complete the full best-in-class program below. A public v1 is
still blocked by these larger tracks:

1. Same-mission provider continuation after handoff. ACP v1 framing and
   negotiation are implemented and tested, but the configurable ACP endpoint
   is not yet exposed to users.
2. Automatic mission recipe execution. The consent-gated CLI, allocator,
   supervisor, restart adoption and cleanup lifecycle are real, but missions do
   not start project services implicitly until that authority boundary is
   represented in the persisted mission contract.
3. The plugin capability host broker, a native-trust manager UI, signed
   packages and a moderated public registry. Zero-capability manifest v2
   components now run inside bounded Wasmtime/WASI isolation. Grants are
   persisted, revocable and bound to the exact manifest and package; host
   capabilities without an implemented binding remain unavailable.
4. A visual golden matrix, contrast gates, long-session performance budgets,
   and chaos coverage across supported terminals.
5. Signed macOS/Linux artifacts, provenance, installer/updater hardening, and a
   reproducible public release pipeline.

The local verification checkpoint is green: Rust formatting, all-target check,
warnings-as-errors lint, 3,053 deterministic serial Rust tests, 105 maintenance
tests, 6 integration-asset tests, 12 marketplace tests, a release build, the
docs build, the landing build, HTML assertions, and responsive browser tests.
The M2 Pro smoke budget also passes at 63.013 ms startup p95, 21.348 ms render
p95, 218.913 ms warm-reattach p95, 0.008% idle CPU and 26.172 MiB process-tree
RSS. Parallel legacy tests still
share process-global environment state, so deterministic release verification
uses one test thread until that isolation debt is removed.

## 1. The decision

Nagi must not be “Herdr with prettier colors”. That would be easy to compare,
easy to copy, and difficult to justify as a new project.

The product to build is:

> **The terminal-native mission control that takes parallel coding agents from
> an objective to fresh, inspectable proof.**

The terminal stays the primary interface. Nagi does not replace Codex, Claude
Code, OpenCode or the user's shell. It gives them a durable runtime, isolates
their work, collects the moments that need a human, and refuses to call work
done until its declared checks have produced fresh evidence.

The primary ICP for v1 is intentionally narrow:

- an experienced developer who runs 3 to 12 agent sessions at once;
- works across one or more repositories and frequently uses worktrees;
- alternates between Codex, Claude Code and OpenCode;
- wants to keep the terminal, but no longer wants to babysit terminal tabs;
- cares more about reviewable, finished work than maximum agent count.

Non-developers are not the v1 ICP. The interface must be learnable and calm,
but broadening the message to “everyone” before the developer workflow is
excellent would dilute the product and its credibility.

## 2. What is already real

This is not a greenfield project and the plan must preserve the strong base.

| Capability | Current truth | Consequence |
|---|---|---|
| PTYs, panes, tabs, sessions, SSH reattach | Working | Keep and harden, do not rebuild |
| Agent detection and cockpit filters | Working | Redesign the presentation and interaction model |
| Typed CLI and local socket API | Working | Make this the stable automation contract |
| Mission journal, definitions and worktree claims | Working | Use as the source of truth for the product wedge |
| Managed Codex, Claude Code and OpenCode paths | Guided launch works; public socket is intentionally read-only | Harden handoff, version negotiation and crash recovery |
| Proof model | Public runner, evidence packs, review UI and guarded closure work | Add remote attestations and release-scale soak coverage |
| Themes | Nagi tokens, Dawn/Night, theme files and Ghostty import work | Add the complete visual golden and contrast release matrix |
| Plugins | Actions, hooks, panes, link handlers, GitHub install | Evolve, do not restart. Add capabilities, sandbox and discovery |
| Plugin marketplace | Worker scaffold exists, public registry not live | Add verification, provenance, abuse flow and signed lock data |
| CI | macOS, Linux, Windows and Nix checks | Extend with visual, performance, chaos and release security gates |
| Public releases | No signed Nagi binary | A signed, inspectable release pipeline is a v1 blocker |

Evidence in the current tree:

- [`README.md`](../../README.md) separates the working mission loop and bounded
  plugin sandbox from the unfinished capability broker and signed release program.
- [`src/app/state.rs`](../../src/app/state.rs) already contains a broad palette
  catalog and semantic colors.
- [`src/config/model.rs`](../../src/config/model.rs) already exposes sidebar,
  density-adjacent and interaction settings.
- [`src/api/schema/plugins.rs`](../../src/api/schema/plugins.rs) already models
  plugin actions, events, panes and link handlers.
- [`src/app/api/plugins/runtime.rs`](../../src/app/api/plugins/runtime.rs)
  routes legacy native commands through explicit trust and manifest v2 commands
  through the bounded component runtime.
- [`website/src/content/docs/plugins.mdx`](../../website/src/content/docs/plugins.mdx)
  documents the exact sandbox limits, grant lifecycle and native trust boundary.
- [`src/mission/executor.rs`](../../src/mission/executor.rs) runs bounded proof
  commands and [`src/mission/evidence_pack.rs`](../../src/mission/evidence_pack.rs)
  stores portable content-addressed evidence.
- [`src/server/headless.rs`](../../src/server/headless.rs) keeps public managed
  launches read-only so an agent cannot forge human write consent.
- [`src/ui/mission_inspector.rs`](../../src/ui/mission_inspector.rs) and
  [`src/ui/proof_review.rs`](../../src/ui/proof_review.rs) expose mission truth
  and fresh proof without leaving the terminal.

## 3. Competitive truth

### Herdr

Herdr already owns the proposition “agent-aware terminal multiplexer”: real
terminal panes, persistence, SSH, socket API, mouse, keyboard and plugins. Nagi
inherits much of this base. Matching those features is table stakes, never the
headline.

### cmux

cmux owns native macOS polish: Ghostty rendering, browser panes, notification
rings, vertical workspaces, remote SSH conveniences and a native settings
surface. Nagi should not chase an embedded browser or become a macOS GUI.
Nagi's advantage is cross-terminal persistence plus a managed, verifiable work
lifecycle.

### Agent Deck, dmux and Agent Orchestrator

These products already cover parallel agents, worktrees, status detection,
merge/PR flows and automation. Agent Orchestrator in particular follows an
issue through worktree, PR, CI, review and cleanup. A worktree launcher alone
is therefore not differentiated.

### Zellij

Zellij sets the extensibility bar with first-class WebAssembly plugins and a
capability permission model. Nagi's current unrestricted process plugins are
powerful, but not safe enough for a public marketplace.

### The opening

No reference above combines all of these as one terminal-native contract:

1. a human-declared objective and explicit acceptance criteria;
2. provider-neutral agent execution in an isolated worktree;
3. one semantic attention inbox for questions and permissions;
4. fresh evidence bound to the current revision and relevant files;
5. a closure gate that distinguishes “the agent stopped” from “the work is
   proven ready”.

That full loop is Nagi's defensible wedge.

## 4. Product contract

### North-star workflow

```text
objective
  -> acceptance criteria
  -> provider + isolation recipe
  -> managed run
  -> attention and consent
  -> reviewable diff
  -> fresh checks + artifacts
  -> proof receipt
  -> human close, PR or merge
  -> safe cleanup
```

### One-minute first mission

From a clean installation inside a Git repository:

1. `nagi` opens the current project and runs a quiet doctor check.
2. `n` opens “New mission”.
3. The user enters an objective and one or more acceptance criteria.
4. Nagi proposes checks from the repository, but never silently invents the
   closure contract.
5. The user selects Codex, Claude Code or OpenCode and confirms the requested
   write scope.
6. Nagi creates or claims the worktree, applies the project recipe, allocates
   ports and launches the provider.
7. The cockpit returns immediately. The mission card shows useful current
   activity, not raw model output.
8. Questions and permission requests appear in the attention inbox.
9. On completion, Nagi runs the frozen checks itself, captures evidence, marks
   stale results when relevant files change, and opens the proof review.
10. The user closes, opens a PR, merges or sends a follow-up. Cleanup is never
    destructive without a clear preview.

### Non-negotiable semantics

- “Working”, “needs you”, “review”, “failed” and “proven” are distinct states.
- A provider process exiting successfully is not proof.
- An agent cannot forge a check result by printing text that looks like one.
- Evidence is tied to mission, run, repository, worktree, base revision,
  current head, declared check and timestamp.
- Any relevant mutation after a check makes that evidence stale.
- Provider-specific protocol code stays behind one adapter boundary.
- The durable server remains the single writer. UI clients never become
  recovery authority.
- Plugins are extensions, not a path around consent or proof authority.

### Anti-goals for v1

- no graphical chat application;
- no built-in model or replacement coding agent;
- no embedded web browser competing with cmux;
- no cloud agent execution platform;
- no team SaaS control plane;
- no automatic merge without an explicit user policy and proof gate;
- no public plugin listing that equates a GitHub topic with trust;
- no “supports every agent” claim based only on screen scraping.

## 5. Best-in-class TUI design system

The current cockpit is readable but feels like a debug table: large blank
areas, a harsh selected-row fill, low information hierarchy, and the terminal
behind it remains visually noisy. The redesign must feel authored, calm and
fast while remaining a true TUI.

### 5.1 Visual direction

Nagi's default identity is “quiet Japanese editorial instrument”, not cyberpunk
dashboard:

- warm ivory and ink for `nagi-dawn`;
- deep indigo-black and warm paper text for `nagi-night`;
- restrained vermilion only for human attention or destructive risk;
- mint for fresh proof, not generic success everywhere;
- blue for active work;
- amber for uncertainty, stale evidence and recovery;
- whitespace as grouping, not empty decoration;
- one accent rail or state glyph instead of full-row neon fills;
- rounded Unicode panel borders when supported, plain ASCII fallback when not;
- no Nerd Font dependency for essential meaning.

### 5.2 Token model

Move from raw color fields to versioned semantic design tokens:

```toml
[meta]
name = "nagi dawn"
schema = 1
appearance = "light"

[palette]
paper = "#f4f0e8"
ink = "#171918"
indigo = "#17365d"
vermilion = "#e34b3f"
mint = "#72c9bb"

[semantic]
canvas = "paper"
panel = "#ece6da"
text = "ink"
text_muted = "#77746d"
focus = "indigo"
attention = "vermilion"
working = "#3977b8"
proof_fresh = "mint"
proof_stale = "#b17837"

[components]
border = "soft"
selection = "rail"
density = "comfortable"
motion = "subtle"
```

Required theme behavior:

- built-in `nagi-dawn` and `nagi-night` become the default pair;
- import any current built-in palette as a base;
- load named theme files from the Nagi config directory;
- light/dark pairing and host appearance sync;
- live reload with diagnostics and rollback to the last valid theme;
- preview every semantic state before applying;
- contrast validation for primary text, selected text and state labels;
- shareable theme packages that contain no executable configuration;
- optional Ghostty palette import, without requiring Ghostty or trusting its
  non-color settings.

### 5.3 Component kit

Create reusable primitives before redrawing screens:

- `Surface`: canvas, panel, elevated and danger variants;
- `FocusRail`: one-cell selection rail with active/inactive styles;
- `StateBadge`: icon, short label and accessible text fallback;
- `Metric`: value plus label with narrow-terminal collapse behavior;
- `Card`: header, body, metadata and action slots;
- `Section`: labelled grouping with optional divider;
- `KeyHint`: consistent keycap and action rendering;
- `ActionBar`: primary, secondary and destructive actions;
- `EmptyState`: explanation plus one concrete action;
- `Skeleton`: bounded loading indicator for async status only;
- `TimelineItem`: actor, event, time and evidence link;
- `ProgressSteps`: criteria and proof completion, never fake percentage;
- `Toast`: deduplicated, severity-aware and non-blocking;
- `CommandPalette`: searchable actions with provider/plugin provenance;
- `Inspector`: progressive detail panel for the selected item.

Every component needs golden render tests in dawn, night, terminal-16 and one
custom theme, at 60×20, 80×24, 120×35 and 200×60.

### 5.4 Responsive cockpit

#### 120 columns and wider

```text
╭ missions ───────────────────╮ ╭ selected mission ───────────────────────╮
│ 2 need you  4 working       │ │ objective                                │
│                             │ │ acceptance criteria          2 / 3 fresh │
│ ▏api auth           NEED YOU│ │                                          │
│  web polish          WORKING│ │ current activity                          │
│  docs                PROVEN │ │ provider · branch · elapsed · cost*      │
│                             │ │                                          │
│ / filter                    │ │ evidence · diff · timeline                │
╰─────────────────────────────╯ ╰──────────────────────────────────────────╯
  n new   a attention   / search   p palette   : commands   ? help
```

`cost` appears only when a provider exposes trustworthy usage data.

#### 80 to 119 columns

- list-first layout;
- selected mission expands inline into a three-to-five row summary;
- full inspector opens with Enter;
- counts collapse to icons plus numbers.

#### Below 80 columns

- single-column cards;
- one primary action per screen;
- no side-by-side tables;
- footer key hints scroll or reduce to the three current actions;
- SSH phone usage remains fully functional.

### 5.5 Required TUI surfaces

1. **Cockpit:** missions and raw panes, grouped by project, with a toggle for
   people who only want session management.
2. **Attention inbox:** questions, permissions, failed delivery and expired
   requests ordered by risk and age.
3. **Mission inspector:** objective, criteria, provider, worktree, activity,
   diff, check state and proof.
4. **Proof review:** fresh/stale/failed checks, command logs, artifacts and the
   exact close decision.
5. **Command palette:** core, provider and plugin actions in one searchable
   surface.
6. **Plugin manager:** installed state, trust mode, capabilities, update diff,
   logs and disable/uninstall.
7. **Appearance studio:** theme, density, borders, icons, motion and preview.
8. **Project recipe editor:** detected commands and isolation settings with a
   raw-file escape hatch.
9. **Doctor:** provider, shell, terminal capability, Git and release integrity
   diagnostics with copyable fixes.
10. **Timeline:** compact audit of mission lifecycle, human decisions, provider
    recovery and proof invalidation.

### 5.6 Interaction quality

- mouse and keyboard are equal, neither is a second-class emulation;
- every screen has visible current actions, no hidden prefix-key guessing;
- `:` opens the command palette from anywhere;
- `/` always means filter/search in the current scope;
- `Esc` closes one layer, never exits the application unexpectedly;
- destructive actions use a preview, target identity and explicit verb;
- confirmation prompts distinguish once, session and mission scope;
- selection persists across refreshes by stable ID, never row index;
- background updates cannot steal focus or reorder the selected item;
- animations stop under reduced motion and while the terminal is unfocused;
- CJK width, emoji clusters, combining marks and ASCII fallback are tested;
- screen-reader-friendly plain text is available through `nagi status --plain`
  and every icon has a text equivalent.

## 6. Differentiating product capabilities

### 6.1 Mission-to-proof, P0

This is the reason Nagi should exist.

Required behavior:

- create and edit a mission before start;
- freeze criteria and checks at start, with an explicit reconfiguration event;
- run managed agents read-only or workspace-write after scoped consent;
- collect provider events without trusting terminal prose;
- execute command checks with bounded time, output and process trees;
- support manual review checks with reviewer identity;
- attach diff summary, changed paths and declared artifacts;
- invalidate stale proof after relevant changes;
- distinguish `review_required`, `ready_to_close` and `archived`;
- emit a portable JSON proof receipt through CLI and socket API;
- let a plugin add evidence, but only the core verifier can mint closure proof.

### 6.2 Attention OS, P0

The user should answer five agents without opening five panes.

- normalize permission requests, questions, delivery failures and review needs;
- include why the request exists, exact scope, risk and expiry;
- batch only semantically identical low-risk items;
- preserve provider-native choices and multi-question forms;
- support approve once, approve for session, deny and answer;
- never offer mission-wide approval for critical actions;
- allow snooze without losing urgency;
- send terminal/system notifications with meaningful context and redaction;
- jump to the originating pane and return to the inbox with one key;
- keep a tamper-evident decision trail in the mission journal.

### 6.3 Provider-neutral runtime, P0

- extract the hard-coded provider match into a versioned adapter trait;
- ship first-party conformance for Codex, Claude Code and OpenCode;
- add an ACP adapter for compatible future agents instead of one file per tool;
- keep screen detection as a transparent fallback, labelled `observed` rather
  than `managed`;
- expose capability negotiation: resume, permissions, questions, diffs,
  interruption, usage and streaming;
- quarantine unsupported provider versions and explain the exact failed
  contract instead of guessing;
- persist provider session identity without storing secrets or full prompts in
  generic logs.

### 6.4 Isolation recipes, P0

Add a project-owned `.nagi/project.toml` contract:

```toml
schema = 1

[worktree]
location = ".worktrees"
base = "main"
copy_ignored = [".env.example"]

[setup]
command = ["bun", "install", "--frozen-lockfile"]
timeout_seconds = 180

[services.web]
command = ["bun", "run", "dev"]
port_env = "PORT"
health = "http://127.0.0.1:{port}/health"

[[checks]]
id = "quality"
command = ["bun", "run", "check"]
covers = ["code compiles and tests pass"]
```

Required behavior:

- detect common package managers and propose, never silently write, a recipe;
- allocate collision-free ports and show their owner;
- copy only explicit ignored files and never copy secrets by broad glob;
- support setup, service, check and cleanup lifecycles with timeouts;
- cache dependencies without sharing mutable build output between worktrees;
- show disk use and orphaned resources before cleanup;
- recover claims after crashes without double ownership;
- offer PR/merge through plugins so GitHub is not hard-coded into the core.

### 6.5 Context handoff, P1 but required for v1 quality

Provider switching is valuable only if it is honest about what transfers.

- create a compact handoff artifact from objective, criteria, worktree, diff,
  decisions, check state and selected logs;
- never pretend a provider's hidden reasoning or proprietary session can move;
- allow “continue with another provider” from blocked, failed or review states;
- bind the new provider session to the same mission and a new run ID;
- preserve audit history and proof freshness rules;
- add `nagi mission handoff --to <provider> --preview`.

### 6.6 Review recipes, P1

Ship opt-in recipes, not magical autonomous teams:

- second-provider diff review;
- security review constrained to the changed surface;
- test-failure triage;
- documentation parity review;
- compare two provider proposals before allowing writes;
- cost and concurrency guardrails with confirmation before multi-provider runs.

## 7. Customization and plugin architecture v2

### 7.1 Preserve the useful v1 surface

The current manifest already supports:

- declared actions;
- event hooks;
- terminal panes and popups;
- link handlers;
- context injection;
- per-plugin config/state directories;
- direct GitHub install and local linking;
- command logs and platform checks.

These contracts need migration, not replacement.

### 7.2 Two explicit trust modes

1. **Sandboxed component, default for marketplace installs**
   - WebAssembly Component Model on Wasmtime/WASI;
   - no inherited environment;
   - no filesystem, network, process or Nagi mutation without capabilities;
   - fuel, memory, output and wall-time limits;
   - deterministic shutdown and crash isolation.

2. **Trusted native process, explicit**
   - keeps Bash, Node, Python, Rust and existing command plugins working;
   - installation displays `FULL USER ACCESS` and the exact commands;
   - permissions cannot be represented as a sandbox guarantee;
   - disabled for unattended marketplace install unless policy allows it;
   - the user can pin author, repository and commit.

### 7.3 Capability manifest

```toml
manifest_version = 2
id = "example.review"
version = "1.2.0"
runtime = "wasi-component"

capabilities = [
  "nagi.state.read",
  "mission.evidence.propose",
  "workspace.files.read:changed",
  "network:https://api.github.com",
]

[[contributions.commands]]
id = "review-current"
title = "Review current mission"
contexts = ["mission"]

[[contributions.inspector_tabs]]
id = "risk"
title = "Risk"
source = "review-risk"
```

Capability families:

- `nagi.state.read`;
- `nagi.layout.write`;
- `pane.content.read`;
- `pane.input.write`;
- `workspace.files.read:<scope>`;
- `workspace.files.write:<scope>`;
- `process.spawn:<allowlist>`;
- `network:<origin>`;
- `clipboard.write`;
- `notifications.send`;
- `mission.read`;
- `mission.attention.propose`;
- `mission.evidence.propose`;
- `secrets.read:<named-secret>`.

Capabilities are granted per plugin version, inspectable, revocable and logged.
An update that requests more capabilities is blocked until approved.

### 7.4 Stable contribution points

Plugins should extend workflows without destroying visual coherence:

- command palette commands;
- cockpit state badges;
- mission inspector tabs using structured rows, lists and metrics;
- evidence collectors;
- project recipe detectors;
- issue/SCM adapters;
- notification adapters;
- provider adapters only behind the provider conformance suite;
- terminal panes for fully custom TUI programs;
- status tokens and link handlers.

Nagi renders structured contributions with its own component kit. Arbitrary
cell drawing is limited to terminal panes, so a theme remains coherent.

### 7.5 SDK and development experience

- generate Rust and TypeScript types from the socket/schema contract;
- `nagi plugin new` scaffolds a sandboxed or trusted-native plugin;
- `nagi plugin dev` links, reloads and tails logs;
- `nagi plugin validate` checks manifest, capabilities and compatibility;
- `nagi plugin test` runs a deterministic host fixture;
- `nagi plugin pack` produces checksums, SBOM and provenance metadata;
- `nagi plugin inspect` shows source, commit, capabilities and update diff;
- ship three excellent reference plugins:
  - GitHub PR/CI lifecycle;
  - local dev services and preview URLs;
  - evidence exporter for Markdown/JSON.

### 7.6 Marketplace launch gate

The registry is not a generic GitHub-topic search. A listed version needs:

- a parsed v2 manifest;
- immutable commit and content checksum;
- declared runtime and capabilities;
- compatible Nagi version range;
- source and publisher identity;
- automated malware, secret and dependency scans;
- reproducible package metadata where possible;
- report/abuse workflow and kill switch;
- visible review status: `official`, `verified metadata`, or `unreviewed`;
- update history and capability diff;
- no misleading star-based trust score.

## 8. Architecture target

```text
thin TUI / CLI / SSH clients
             │
             ▼
      single-writer Nagi server
             │
   ┌─────────┼────────────┬────────────────┐
   │         │            │                │
sessions   mission      attention       event hub
+ PTYs     journal      + consent       + socket API
   │         │            │                │
   │    evidence runner    │                │
   │    + proof verifier   │                │
   │         │            │                │
   └─────────┼────────────┴────────────────┘
             │
      provider adapter registry
      ├── first-party Codex
      ├── first-party Claude Code
      ├── first-party OpenCode
      └── ACP compatible adapter
             │
      plugin capability broker
      ├── sandboxed components
      └── explicit trusted processes
```

### Authority boundaries

- **Server:** session state, mission state, attention decisions and proof
  authority.
- **Provider adapter:** protocol translation and provider session lifecycle.
- **Evidence runner:** executes frozen checks and captures bounded artifacts.
- **Plugin broker:** grants capabilities and translates contributions, but
  cannot mint proof.
- **TUI:** renders projections and submits typed intents.
- **CLI/socket clients:** same contract as TUI, with no hidden privileged path.

### Data contracts to version before UI work

- `MissionViewV1` including criteria/check/evidence summaries;
- `AttentionItemV1` with risk, scope, expiry and delivery state;
- `ProofReceiptV1` with identity, fresh evidence and closure decision;
- `ProviderCapabilitiesV1`;
- `PluginManifestV2` and `PluginGrantV1`;
- `ThemeManifestV1`;
- `ProjectRecipeV1`;
- `UiContributionV1`.

All persisted contracts need fixture migration tests for the previous two
versions and a fail-safe path that preserves user data on unknown future data.

## 9. Performance, reliability and security bar

Targets must be measured on named hardware and recorded in CI. These are v1
budgets, not current claims.

### Performance budgets

| Scenario | v1 budget |
|---|---|
| Cold start, empty project | p95 below 200 ms on Apple M2 and current Linux CI reference |
| Local key-to-frame, 8 active panes | p95 below 16.7 ms at 120×40 |
| Local key-to-frame, 32 panes | p95 below 33 ms at 200×60 |
| Idle server CPU, no animated plugin | below 1% after 30 seconds |
| Cockpit filter over 500 missions/panes | p95 below 50 ms |
| Local reattach to warm server | p95 below 500 ms |
| Attention event to visible badge | p95 below 100 ms locally |
| Base resident memory | establish baseline, then gate regressions above 10% |
| Per-pane memory | establish at 10 MB scrollback, gate regressions above 10% |

### Reliability scenarios

- kill the TUI during input, server and panes remain intact;
- kill the server during journal append, restart yields the last complete event;
- provider disconnect transitions once and recovers without duplicate turns;
- permission delivery timeout remains blocked until reconciled;
- evidence process and its descendants are terminated on timeout;
- plugin trap, timeout or memory exhaustion cannot stall the render loop;
- full disk produces a clear degraded state, never a false durable write;
- clock changes do not reorder authority or extend expired consent;
- SSH packet loss and resize storms do not corrupt layout or input;
- two clients cannot both become mission authority;
- stale worktree claims are inspectable and recoverable, never silently stolen.

### Security gates

- local sockets and state directories use least-permission defaults;
- plugin install pins immutable source and verifies checksums;
- marketplace plugins default to sandboxed runtime;
- native plugins show unrestricted trust and receive a scrubbed environment;
- named secrets are injected only after capability approval and are redacted
  from logs, events, crash bundles and proof receipts;
- command checks use argv, explicit cwd, timeout, output cap and process-tree
  cleanup;
- remote access binds locally by default and requires explicit authenticated
  exposure;
- release artifacts are signed, checksummed, include an SBOM and provenance;
- CI runs dependency audit, license policy, secret scan and unsafe-code review;
- security reports have a private intake and documented response policy.

## 10. Verification strategy

### Unit and property tests

- state-machine transitions and illegal transitions;
- journal replay, deduplication and corruption boundaries;
- worktree claim ownership and path canonicalization;
- evidence freshness under exact, prefix and ignored-path mutations;
- consent expiry, retry and delivery-unknown behavior;
- provider frame decoders with size and sequence fuzzing;
- theme parsing, inheritance, rollback and contrast;
- plugin manifest/capability validation;
- responsive layout geometry and hit-testing;
- Unicode width and truncation invariants.

### Golden TUI tests

Use Ratatui `TestBackend` snapshots for every primary surface:

- four terminal dimensions;
- four representative themes;
- empty, loading, normal, attention, stale, failed and overflow states;
- ASCII-only mode;
- long repository, branch, mission and provider labels;
- 1, 8, 50 and 500 mission/pane datasets.

Golden updates require human review and a before/after image in the PR.

### Integration tests

- real PTY detach/reattach and multi-client behavior;
- real Git repository/worktree creation, setup, proof and cleanup;
- provider contract fixtures plus canary tests against pinned supported
  provider versions;
- socket API end-to-end mission flow;
- sandbox capability denial and grant persistence;
- trusted-native plugin warning and environment scrub;
- package install, first launch and update on clean VMs.

### Manual terminal matrix

- Ghostty, WezTerm, Alacritty, Kitty, iTerm2 and macOS Terminal;
- tmux-nested and SSH-nested sessions;
- zsh, bash, fish and Nushell;
- light/dark host switching;
- mouse enabled/disabled;
- macOS and Linux at minimum;
- phone-sized SSH terminal.

## 11. Executable implementation plan

The steps below are internal implementation milestones. None is marketed as an
MVP and there is no public v1 release until section 12 is green.

### Track 0: freeze contracts and measurement

#### Task 0.1: record the baseline

**Files:**

- Add `docs/benchmarks/2026-07-baseline.md`
- Add `scripts/bench_startup.sh`
- Add `scripts/bench_render.py`
- Update `justfile`

**Steps:**

1. Capture startup, idle CPU, memory, render latency and reattach time.
2. Record hardware, terminal size, pane count, toolchain and exact commit.
3. Add `just bench-smoke` with non-flaky generous regression ceilings.
4. Verify the benchmark fails on an intentionally tiny ceiling.

**Atomic commits:**

- `test(perf): add reproducible startup benchmark`
- `test(perf): record initial runtime baseline`

#### Task 0.2: version the product contracts

**Files:**

- Modify `src/api/schema/missions.rs`
- Add `src/api/schema/attention.rs`
- Add `src/api/schema/proof.rs`
- Add `src/api/schema/providers.rs`
- Update `src/api/schema.rs`
- Update `docs/next/api/nagi-api.schema.json`

**Tests:** serialization snapshots, unknown-field behavior, schema parity and
old-fixture migration.

**Atomic commits:**

- `feat(api): version mission projection contracts`
- `feat(api): expose typed attention projections`
- `feat(api): expose portable proof receipts`
- `feat(api): declare provider capabilities`

### Track A: Nagi design system and calm cockpit

#### Task A.1: introduce semantic UI tokens

**Files:**

- Add `src/ui/design/mod.rs`
- Add `src/ui/design/tokens.rs`
- Add `src/ui/design/icons.rs`
- Modify `src/ui.rs`
- Modify `src/app/state.rs`

**Tests:** token completeness, ASCII fallback, contrast pairs and built-in
palette compatibility.

**Atomic commits:**

- `refactor(ui): introduce semantic design tokens`
- `feat(ui): add portable icon fallbacks`

#### Task A.2: add Nagi themes and theme files

**Files:**

- Add `src/theme/manifest.rs`
- Add `src/theme/loader.rs`
- Add `src/theme/builtins.rs`
- Add `assets/themes/nagi-dawn.toml`
- Add `assets/themes/nagi-night.toml`
- Modify `src/config/theme.rs`
- Modify `src/app/theme_sync.rs`
- Modify `src/ui/settings.rs`

**Tests:** live reload, invalid rollback, light/dark sync, custom file lookup,
Ghostty color-only import and contrast diagnostics.

**Atomic commits:**

- `feat(theme): add versioned theme manifests`
- `feat(theme): ship Nagi dawn and night`
- `feat(theme): reload custom themes safely`
- `feat(theme): preview and validate appearance`

#### Task A.3: build reusable components

**Files:**

- Add `src/ui/components/surface.rs`
- Add `src/ui/components/card.rs`
- Add `src/ui/components/state_badge.rs`
- Add `src/ui/components/action_bar.rs`
- Add `src/ui/components/inspector.rs`
- Add `src/ui/components/timeline.rs`
- Modify `src/ui/widgets.rs`

**Tests:** golden snapshots, bounds under tiny rectangles, hit targets and
theme matrix.

**Atomic commits:** one commit per component family, for example
`feat(ui): add themed card primitives` and
`feat(ui): standardize action bars and key hints`.

#### Task A.4: command palette

**Files:**

- Add `src/ui/command_palette.rs`
- Add `src/app/input/command_palette.rs`
- Modify `src/app/state.rs`
- Modify `src/app/input/mod.rs`
- Modify `src/config/keybinds.rs`

**Tests:** fuzzy matching, stable selection during refresh, disabled action
reasons, plugin provenance and mouse activation.

**Atomic commits:**

- `feat(ui): add global command palette`
- `feat(plugins): surface plugin actions in command palette`

#### Task A.5: responsive cockpit redesign

**Files:**

- Split `src/ui/navigator.rs` into `src/ui/cockpit/{mod,layout,list,inspector,footer}.rs`
- Add `src/ui/cockpit/model.rs`
- Modify `src/app/state.rs`
- Modify `src/app/input/overlays.rs`
- Modify `src/app/input/mouse.rs`

**Tests:** 60/80/120/200-column goldens, 500-item filtering, no selection jump,
state-count correctness and mouse hit-testing.

**Atomic commits:**

- `refactor(cockpit): separate projection from rendering`
- `feat(cockpit): add adaptive mission cards`
- `feat(cockpit): add contextual mission inspector`
- `feat(cockpit): preserve focus across live updates`
- `feat(cockpit): polish compact terminal layout`

#### Task A.6: attention inbox and proof review

**Files:**

- Add `src/ui/attention/{mod,list,detail,actions}.rs`
- Add `src/ui/proof/{mod,criteria,evidence,actions}.rs`
- Add `src/app/input/attention.rs`
- Add `src/app/input/proof.rs`
- Modify `src/ui.rs`

**Tests:** risk ordering, expiry, batch safety, answer forms, stale evidence,
failed checks, no-close path and narrow terminals.

**Atomic commits:**

- `feat(attention): add unified request inbox`
- `feat(attention): add scoped consent actions`
- `feat(proof): add criteria and evidence review`
- `feat(proof): gate closure on fresh evidence`

#### Task A.7: first-run and doctor experience

**Files:**

- Rewrite `src/ui/onboarding.rs`
- Add `src/doctor/mod.rs`
- Add `src/ui/doctor.rs`
- Add `src/cli/doctor.rs`
- Modify `src/cli.rs`

**Tests:** clean machine fixtures, missing provider, unsupported version,
terminal capability fallbacks and copyable remediation.

**Atomic commits:**

- `feat(doctor): inspect local runtime readiness`
- `feat(onboarding): guide the first mission`

### Track B: complete mission-to-proof

#### Task B.1: evidence command runner

**Files:**

- Add `src/mission/check_runner.rs`
- Add `src/mission/process_tree.rs`
- Modify `src/mission/evidence.rs`
- Modify `src/mission/runtime.rs`
- Modify `src/server/mission_bridge.rs`

**Tests:** argv safety, cwd enforcement, timeout, output caps, descendant kill,
artifact rules, ignored paths and disk errors.

**Atomic commits:**

- `feat(mission): execute bounded command checks`
- `feat(mission): capture check artifacts`
- `fix(mission): terminate timed out process trees`

#### Task B.2: public proof transitions

**Files:**

- Modify `src/mission/proof.rs`
- Modify `src/mission/model.rs`
- Modify `src/mission/runtime.rs`
- Modify `src/mission/journal.rs`
- Modify `src/api/schema/missions.rs`
- Modify `src/server/mission_bridge.rs`

**Tests:** complete happy path, stale-after-mutation, manual review, override
audit, replay, duplicate requests and forged evidence rejection.

**Atomic commits:**

- `feat(mission): evaluate readiness from fresh evidence`
- `feat(mission): mint durable proof receipts`
- `feat(mission): expose reviewed closure transitions`

#### Task B.3: wire provider consent responses

**Files:**

- Modify `src/server/consent.rs`
- Modify `src/server/headless.rs`
- Modify `src/managed_provider/mod.rs`
- Modify `src/managed_provider/codex.rs`
- Modify `src/managed_provider/claude.rs`
- Modify `src/mission/attention.rs`

**Tests:** approve once/session, deny, multi-question answers, expiry,
delivery-unknown reconciliation, restart and secret redaction.

**Atomic commits:**

- `feat(consent): route managed provider requests`
- `feat(consent): reconcile provider response delivery`
- `feat(mission): persist scoped human decisions`

#### Task B.4: provider registry and OpenCode parity

**Files:**

- Add `src/managed_provider/adapter.rs`
- Add `src/managed_provider/registry.rs`
- Modify `src/managed_provider/mod.rs`
- Modify `src/managed_provider/opencode.rs`
- Modify `src/server/headless.rs`
- Add `tests/provider_conformance.rs`

**Tests:** start, resume, turn, interrupt, attention, response, completion,
disconnect and unsupported-version behavior for all first-party providers.

**Atomic commits:**

- `refactor(providers): introduce adapter registry`
- `feat(providers): complete OpenCode mission starts`
- `test(providers): enforce managed adapter conformance`

#### Task B.5: ACP adapter

**Files:**

- Add `src/managed_provider/acp.rs`
- Modify `src/managed_provider/registry.rs`
- Modify `src/api/schema/providers.rs`
- Add `tests/fixtures/acp/`

**Tests:** initialization, session resume, permission mapping, diff/tool events,
malformed frames, capability negotiation and remote-not-supported diagnostics.

**Atomic commits:**

- `feat(providers): add ACP compatibility adapter`
- `test(providers): cover ACP capability negotiation`

#### Task B.6: project recipes and resources

**Files:**

- Add `src/project_recipe/{mod,model,detect,validate}.rs`
- Add `src/resources/{ports,services,cleanup}.rs`
- Modify `src/worktree.rs`
- Modify `src/mission/runtime.rs`
- Add `src/api/schema/projects.rs`
- Add `src/cli/project.rs`

**Tests:** package-manager fixtures, port races, setup failure, health checks,
explicit ignored-file copy, secret rejection, crash recovery and safe cleanup.

**Atomic commits:**

- `feat(projects): add versioned isolation recipes`
- `feat(worktrees): apply bounded setup recipes`
- `feat(resources): allocate mission service ports`
- `feat(resources): preview orphan cleanup`

#### Task B.7: provider handoff

**Files:**

- Add `src/mission/handoff.rs`
- Modify `src/mission/runtime.rs`
- Modify `src/api/schema/missions.rs`
- Add `src/cli/mission.rs` if mission CLI is still inline

**Tests:** preview, same-mission new-run identity, redaction, failed source run,
freshness preservation and unsupported target capabilities.

**Atomic commits:**

- `feat(mission): build provider-neutral handoff artifacts`
- `feat(mission): continue work with another provider`

### Track C: plugin v2 and ecosystem

#### Task C.1: manifest v2 and grants

**Files:**

- Add `src/api/schema/plugin_v2.rs`
- Add `src/plugin_capabilities.rs`
- Modify `src/api/schema/plugins.rs`
- Modify `src/app/api/plugins/manifest.rs`
- Modify `src/persist/plugin_registry.rs`

**Tests:** v1 migration, capability parsing, grant version binding, escalation
block, revocation and invalid scope rejection.

**Atomic commits:**

- `feat(plugins): add versioned capability manifests`
- `feat(plugins): persist revocable grants`
- `feat(plugins): migrate legacy native plugins`

#### Task C.2: sandboxed component runtime

**Files:**

- Add `src/plugin_sandbox/{mod,host,limits,bindings}.rs`
- Add WIT contracts under `wit/nagi-plugin/`
- Modify `Cargo.toml`
- Modify `src/app/api/plugins/runtime.rs`

**Tests:** no capabilities, read-only scope, network origin filter, fuel/memory
limits, timeout, trap, hostile output and host restart.

**Atomic commits:**

- `feat(plugins): host sandboxed components`
- `feat(plugins): enforce runtime capability grants`
- `fix(plugins): bound component resources`

#### Task C.3: harden trusted native plugins

**Files:**

- Modify `src/cli/plugin.rs`
- Modify `src/app/api/plugins/runtime.rs`
- Add `src/app/api/plugins/trust.rs`
- Modify `website/src/content/docs/plugins.mdx`

**Tests:** explicit trust, environment scrub, immutable commit pin, capability
label honesty, noninteractive refusal and update preview.

**Atomic commits:**

- `feat(plugins): require explicit native trust`
- `fix(plugins): scrub inherited command environment`
- `feat(plugins): preview source and capability changes`

#### Task C.4: structured UI contributions

**Files:**

- Add `src/api/schema/ui_contributions.rs`
- Add `src/app/api/plugins/contributions.rs`
- Add `src/ui/plugin_contributions.rs`
- Modify `src/ui/command_palette.rs`
- Modify `src/ui/cockpit/inspector.rs`

**Tests:** schema validation, theme conformity, oversized payloads, missing
plugin, disabled plugin and deterministic ordering.

**Atomic commits:**

- `feat(plugins): register structured UI contributions`
- `feat(cockpit): render plugin inspector tabs`

#### Task C.5: plugin SDK and tooling

**Files:**

- Add `sdk/rust/`
- Add `sdk/typescript/`
- Add `templates/plugin-wasi/`
- Add `templates/plugin-native/`
- Extend `src/cli/plugin.rs`

**Tests:** generated-code parity, scaffold build, deterministic fixture host and
package validation.

**Atomic commits:**

- `feat(sdk): generate typed plugin contracts`
- `feat(cli): scaffold and validate plugins`
- `feat(cli): add live plugin development loop`

#### Task C.6: verified marketplace pipeline

**Files:**

- Rewrite `workers/plugin-marketplace/src/index.ts`
- Add `workers/plugin-marketplace/src/manifest.ts`
- Add `workers/plugin-marketplace/src/scans.ts`
- Add `workers/plugin-marketplace/src/submissions.ts`
- Add `website/src/pages/plugins/`

**Tests:** immutable source, manifest fetch limits, checksum, blacklist, abuse
status, scan failure, capability diff and malicious metadata.

**Atomic commits:**

- `feat(marketplace): ingest immutable plugin versions`
- `feat(marketplace): publish provenance and capabilities`
- `feat(marketplace): add reporting and emergency disable`
- `feat(site): launch verified plugin directory`

### Track D: production hardening and release

#### Task D.1: visual regression suite

**Files:**

- Add `tests/ui_golden.rs`
- Add `tests/golden/`
- Add `scripts/render_ui_goldens.py`
- Update `justfile`

**Atomic commits:**

- `test(ui): add cross-theme golden renders`
- `test(ui): cover responsive cockpit states`

#### Task D.2: chaos and recovery suite

**Files:**

- Add `tests/mission_recovery.rs`
- Add `tests/provider_recovery.rs`
- Add `tests/plugin_isolation.rs`
- Add `scripts/chaos_runtime.py`

**Atomic commits:**

- `test(runtime): exercise crash recovery boundaries`
- `test(mission): verify authority after interruption`
- `test(plugins): isolate hostile runtimes`

#### Task D.3: enforce performance budgets

**Files:**

- Extend `src/render_prof.rs`
- Add `benches/`
- Add `.github/workflows/performance.yml`
- Update `docs/benchmarks/`

**Atomic commits:**

- `perf(ui): instrument input-to-frame latency`
- `test(perf): gate sustained runtime regressions`
- follow-up performance commits must name the measured bottleneck, not use a
  generic `perf` bucket.

#### Task D.4: release supply chain

**Files:**

- Restore and rewrite `.github/disabled-workflows/release.yml` into
  `.github/workflows/release.yml`
- Add `deny.toml`
- Add `.github/workflows/security.yml`
- Add `scripts/verify_release.py`
- Update `website/install.sh`
- Update `website/install.ps1`
- Update `website/latest.json`

**Tests:** clean VM install, signature failure, checksum failure, wrong
architecture, rollback and update from previous release.

**Atomic commits:**

- `build(security): enforce dependency policy`
- `build(release): produce signed artifacts`
- `test(release): verify clean machine installs`
- `feat(release): publish package manager metadata`

#### Task D.5: documentation and launch-quality examples

**Files:**

- Rewrite `README.md` around mission-to-proof once it works
- Update `website/src/content/docs/`
- Mirror `docs/next/`
- Add `examples/project-recipes/`
- Add `examples/plugins/`
- Add `docs/architecture/authority.md`
- Add `docs/architecture/plugin-security.md`

**Atomic commits:**

- `docs(product): explain mission-to-proof workflow`
- `docs(plugins): document capabilities and trust`
- `docs(recipes): add production project examples`
- `docs(release): publish support and security policy`

## 12. Nagi v1 release gates

Every item is mandatory. “Mostly works” is not a v1 result.

### Product

- [ ] A fresh user installs Nagi and starts a first mission in under three
  minutes without reading long documentation.
- [ ] Codex, Claude Code and OpenCode pass the same managed provider conformance
  suite on pinned supported versions.
- [x] The complete objective-to-proof flow works from both TUI and socket API.
- [x] A mission cannot close with missing, failed or stale required evidence.
- [x] Attention requests can be answered centrally and reconcile after failure.
- [x] Worktree, port, setup and cleanup lifecycle survives restart.
- [x] Provider handoff produces an inspectable, redacted artifact.

### Design

- [ ] Nagi dawn and night are polished defaults, not reskinned Catppuccin.
- [x] All primary surfaces pass golden review at 60, 80, 120 and 200 columns.
- [x] No essential state is communicated by color alone.
- [x] Theme files live reload with rollback and contrast diagnostics.
- [ ] Keyboard-only, mouse-only and phone-over-SSH flows are complete.
- [x] The cockpit remains useful with 1, 8, 50 and 500 items.

### Plugins

- [x] Existing v1 plugins migrate or receive an exact compatibility error.
- [x] Marketplace installs default to sandboxed components.
- [x] Native plugins require explicit unrestricted trust.
- [x] Capability escalation on update is blocked pending approval.
- [x] Three first-party reference plugins are useful, tested and documented.
- [x] Registry entries expose immutable source, checksum, runtime, capabilities
  and review status.

### Quality

- [ ] Full serial and nextest suites pass on supported platforms.
- [ ] CI has no known flaky test masked by retries.
- [x] Performance budgets pass on named reference machines.
- [x] Crash, disk-full, timeout and disconnect tests pass.
- [x] Security audit has no open critical or high finding.
- [ ] Installation and update work on clean macOS and Linux VMs.
- [ ] Binaries are signed, checksummed, reproducible where documented, and ship
  SBOM plus provenance.
- [x] No inherited Herdr update or release channel can install under the Nagi
  name.

### Open-source readiness

- [x] Provenance remains explicit and correct.
- [x] Contribution guide includes architecture boundaries and local checks.
- [ ] At least ten issues are independently completable, with five marked good
  first issue and no fake busywork.
- [x] RFC path exists for provider, plugin and persisted-contract changes.
- [x] Security intake, code of conduct and governance expectations are clear.
- [ ] Two external developers have completed the install and mission flow from
  the public docs without maintainer intervention.

### Local evidence snapshot, 2026-07-20

- Serial Rust suite: 2,997 unit tests plus every integration binary passed on
  Apple M2 Pro, macOS. `cargo-nextest` and the other supported platforms remain
  release blockers until CI records them.
- Visual matrix: 11 real Ratatui surfaces, four themes, and 60/80/120/200-column
  snapshots, including 1/8/50/500-session cockpit states.
- Runtime recovery: hard kill with torn journal tail, storage exhaustion between
  journal and head writes, provider disconnect, plugin fuel exhaustion, and
  timeout paths all pass without false completion.
- Supply chain: dependency policy, audit, workflow semantics, checksums, SBOM,
  Cosign bundle verification, and provenance verification pass locally. No
  public signed artifact is claimed until the repository workflow runs and the
  result installs on clean macOS and Linux machines.
- Open-source policy: authority and plugin-security boundaries, contribution
  checks, RFCs, security intake, conduct, governance, and upstream provenance are
  documented. Public issue curation and external first-run evidence remain human
  launch tasks, not boxes to fabricate in source.

## 13. Dogfood and evidence of market fit

Stars are a lagging signal. The launch decision should use behavior:

- recruit 20 developers who currently run at least three agent sessions;
- observe ten first-run sessions without coaching;
- dogfood Nagi on its own repository for every material change;
- require every Nagi change to link a mission and proof receipt once stable;
- track locally and opt-in only:
  - time to first mission;
  - missions started per active week;
  - percentage reaching proof review;
  - attention items answered from inbox rather than pane hunting;
  - crash-free session hours;
  - provider recovery success;
  - plugins installed and retained after seven days;
- conduct a short interview after first proof review, not after installation;
- treat fewer than 30% of activated users running a second mission within a
  week as a product problem, not a marketing problem.

Telemetry is off by default. `nagi diagnostics export` produces a user-reviewed
bundle, and opt-in usage metrics contain no prompts, code, paths, secrets or
terminal content.

## 14. Opportunity scorecard

| Capability | User value | Differentiation | Build risk | Decision |
|---|---:|---:|---:|---|
| Fresh proof and closure gate | 10 | 10 | 8 | Core v1 |
| Unified attention and consent | 10 | 9 | 7 | Core v1 |
| Calm adaptive cockpit | 9 | 7 | 5 | Core v1 |
| Provider-neutral managed runtime | 9 | 8 | 9 | Core v1 |
| Project recipes, ports and services | 9 | 6 | 6 | Core v1 |
| Safe plugin capabilities | 8 | 8 | 9 | Core v1 |
| Provider handoff artifact | 8 | 9 | 7 | Core v1 |
| PR/CI lifecycle plugin | 8 | 4 | 5 | First-party plugin |
| Second-provider review recipe | 7 | 7 | 6 | Opt-in v1 recipe |
| Embedded browser | 6 | 2 | 10 | Do not build |
| Cloud agents | 6 | 2 | 10 | Post-v1, only with demand |
| Non-developer mode | 4 | 3 | 9 | Do not target in v1 |
| Graphical chat UI | 3 | 1 | 8 | Explicit anti-goal |

## 15. Effort and strongest risk

This is not a one-week polish pass. A credible estimate is:

- one senior Rust engineer working alone: roughly 16 to 22 focused weeks;
- three experienced engineers plus product-design QA: roughly 8 to 12 weeks;
- provider protocol drift and the sandbox are the highest-variance items.

The strongest product risk is not technical. It is spending months building a
beautiful multiplexer while users actually choose provider-native subagents or
lighter tmux wrappers. The mitigation is to dogfood the proof loop early,
observe whether it changes review behavior, and keep generic multiplexer work
strictly subordinate to that outcome.

The strongest alternative is not Herdr or cmux. It is a smaller, provider-
neutral proof layer that attaches to existing multiplexers. If developers love
proof receipts and the attention inbox but resist switching multiplexers,
Nagi should make its socket/server layer embeddable instead of forcing terminal
ownership. The architecture above deliberately keeps that option open.

## Research references

- [Herdr repository and current positioning](https://github.com/ogulcancelik/herdr)
- [cmux repository and current feature set](https://github.com/manaflow-ai/cmux)
- [Agent Client Protocol introduction](https://agentclientprotocol.com/get-started/introduction)
- [Zellij WebAssembly plugins](https://zellij.dev/documentation/plugins.html)
- [Zellij plugin permissions](https://zellij.dev/documentation/plugin-api-permissions)
- [Ghostty theme system](https://ghostty.org/docs/features/theme)
- [OpenCode plugin hooks](https://opencode.ai/docs/plugins/)
- [Agent Deck repository](https://github.com/asheshgoplani/agent-deck)
- [dmux repository](https://github.com/standardagents/dmux)
- [Agent Orchestrator architecture and lifecycle](https://aoagents.dev/docs)
