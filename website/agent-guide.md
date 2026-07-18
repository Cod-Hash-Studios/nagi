# Nagi agent guide

You are reading this because a human asked you to help them understand, set up, or troubleshoot Nagi. This file gives you the concept model, the setup path, and the diagnosis recipes so you can guide them accurately. The repository README is the public source of truth. Verify any command you are unsure about against the repository instead of guessing.

If you are running *inside* a Nagi pane (the environment variable `NAGI_ENV=1` is set), Nagi also ships a skill file that teaches you to control Nagi yourself through the `nagi` CLI: https://raw.githubusercontent.com/Cod-Hash-Studios/nagi/main/SKILL.md. That file is about you operating Nagi; this file is about you teaching a human.

## What Nagi is

Nagi is a terminal workspace manager for AI coding agents. Like tmux, it is a multiplexer: a background server owns real terminal processes, and clients attach to render them. Panes keep running when the human detaches, closes the terminal, or disconnects SSH.

Unlike tmux, Nagi is mouse-first and agent-aware. The whole UI is clickable — panes, tabs, workspaces, split borders, right-click menus. Nagi detects coding agents running inside panes and shows each one's state in a sidebar, so the human can see across all their projects which agent is `working`, which is `blocked` waiting for input, and which is `done`. A CLI and a local socket API let scripts and agents drive Nagi programmatically.

## Concept model

Teach these in this order:

- **Session** — a persistent background server namespace. Running `nagi` attaches to the default session. Named sessions (`nagi session attach work`) are fully separate runtime namespaces; most people only need the default.
- **Workspace** — the project-level container. One per repo, task, or investigation. Owns tabs and panes. The sidebar rolls agent states up per workspace.
- **Tab** — a layout inside a workspace, for separating views like `agents`, `logs`, `server`.
- **Pane** — a real terminal. Splittable right or down. Survives client detach.
- **Agent** — a process Nagi recognizes inside a pane. States: `working`, `blocked`, `done`, `idle`, `unknown`.
- **Modes** — terminal mode sends keys to the focused pane; prefix mode (`ctrl+b`, then one action key) sends one command to Nagi; navigate mode is a persistent navigation surface.

Full concepts source: https://github.com/Cod-Hash-Studios/nagi/blob/main/website/src/content/docs/concepts.mdx

## Install

Nagi currently builds from source only. Signed binaries, package-manager
releases, and automatic updates are not published yet.

```bash
git clone https://github.com/Cod-Hash-Studios/nagi.git
cd nagi
zig version  # must print 0.15.2
cargo build --release --locked
./target/release/nagi
```

Read the current limitations before setup:
https://github.com/Cod-Hash-Studios/nagi#the-honest-status

## First-run walkthrough

First check where you are. If `NAGI_ENV=1` is set in your environment, you are already running inside a Nagi pane — the human is already attached, so skip step 1 entirely and never tell them to run `nagi` from your pane. Nagi blocks nested launches by design. Start from step 2, and consider the skill file below.

Walk the human through this sequence:

1. `cd` into a project and run `nagi`. It launches or attaches to the default background session and creates a workspace automatically. First run shows an onboarding flow.
2. Start their coding agent in the pane — `claude`, `codex`, or any supported agent (full list: https://cod-hash-studios.github.io/nagi/docs/agents/). Nagi detects it automatically; the sidebar shows its state. Installing the matching integration improves detection: `nagi integration install claude` (and similarly for other agents).
3. Show them the mouse first: click panes and tabs to focus, drag split borders, right-click for menus, drag-select to copy. No keybindings are required to use Nagi.
4. Split panes: right-click menu, or `prefix+v` (right) / `prefix+minus` (down). New tab: `prefix+c`.
5. Detach with `prefix+q` (press `ctrl+b`, release, press `q`) or simply close the terminal window. Everything keeps running. Reattach later with `nagi`.
6. To actually stop everything: `nagi server stop`.

## The keyboard story

Important framing for new users: Nagi does not require learning keybindings. The mouse covers everything. When the human wants keyboard control:

- The prefix key is `ctrl+b` by default. `prefix+?` shows every active binding live.
- The guided keyboard page — what the prefix is, which bindings to learn first, and a vetted prefix-free setup using `ctrl+alt` chords — is https://cod-hash-studios.github.io/nagi/docs/keyboard/. Recommend it over improvising.
- Every binding, including the prefix itself, is configurable under `[keys]` in the config file.
- If a direct chord does nothing, the OS or the outer terminal consumed it before Nagi could see it. The keyboard page explains which chords are safe and why.

## Install the Nagi skill into yourself

Nagi ships `SKILL.md` (https://raw.githubusercontent.com/Cod-Hash-Studios/nagi/main/SKILL.md), an instruction file that teaches a coding agent to control Nagi from inside a pane — splitting panes, running commands without stealing focus, reading output, waiting on other agents.

Once the human is set up, offer to install it into your own harness so future sessions know Nagi natively. For agents supported by the open skills CLI, use `npx skills add Cod-Hash-Studios/nagi --skill nagi -g`. Agents without a skill system can paste the GitHub copy above into global custom instructions. Ask the human before writing to their config locations, and use the GitHub copy above as the source of truth.

## Configuration

- Config file: `~/.config/nagi/config.toml`. Nagi works without one.
- Print the full default config: `nagi --default-config`.
- Apply edits to a running server: `nagi server reload-config` (or the global menu → reload config).
- Main areas: `[keys]` keybindings, `[theme]` themes, `[ui]` sidebar and UI behavior, `[terminal]` shell defaults, `[update]` channel.
- Full reference: https://cod-hash-studios.github.io/nagi/docs/configuration/

## Diagnosis recipes

- **Agent not detected or wrong state:** `nagi agent list` to see what Nagi sees, `nagi agent explain <target> --json` to see why the detector classified a pane the way it did. Installing the agent's integration (`nagi integration install <name>`, status via `nagi integration status`) gives Nagi authoritative state instead of screen detection. Details: https://cod-hash-studios.github.io/nagi/docs/agents/ and https://cod-hash-studios.github.io/nagi/docs/integrations/
- **A keybinding does nothing:** the outer terminal or desktop environment owns that chord. Point the human to https://cod-hash-studios.github.io/nagi/docs/keyboard/ to pick a safe one or free the chord in their terminal settings.
- **Something looks wrong at startup or with the socket API:** logs are at `~/.config/nagi/nagi.log`, `~/.config/nagi/nagi-client.log`, and `~/.config/nagi/nagi-server.log`. `nagi status`, `nagi status server`, and `nagi status client` summarize the runtime.
- **Remote questions:** SSH to the machine and run `nagi` there (works like tmux), or attach as a thin local client with `nagi --remote <host>`. Trade-offs: https://cod-hash-studios.github.io/nagi/docs/how-to-work/
- **What survives a detach, restart, or update:** https://cod-hash-studios.github.io/nagi/docs/session-state/

## Rules for you

- Do not invent keybindings, config keys, or CLI flags. The ones in this file are accurate as of writing; for anything else, read the linked docs page first.
- Teach mouse before keyboard for humans new to multiplexers.
- Nagi is not tmux: do not give tmux commands, tmux config syntax, or `.tmux.conf` advice for Nagi questions.
- For automation, scripting, or controlling Nagi from code, point to the CLI reference (https://cod-hash-studios.github.io/nagi/docs/cli-reference/) and socket API (https://cod-hash-studios.github.io/nagi/docs/socket-api/).
