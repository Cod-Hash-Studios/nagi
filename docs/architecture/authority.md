# Authority boundaries

Nagi keeps terminal work convenient by making authority deliberately boring.
There is one writer for durable runtime state, and every other surface asks that
writer to act.

## Single-writer runtime

The headless server owns workspaces, panes, missions, attention, worktree claims,
project resources, and plugin grants. TUI clients, CLI commands, SSH clients, and
plugins do not edit journal or snapshot files directly.

Durable mission state is appended to the mission journal and rebuilt through
validated replay. A torn final frame may be truncated during recovery. A valid
historical frame is never rewritten to make a later operation succeed.

## Human authority

Only an interactive local Nagi surface can approve:

- workspace write access for a managed provider;
- project recipe setup, service execution, cleanup, or ignored-file copying;
- a provider permission or attention decision;
- unrestricted native plugin trust;
- a new or expanded plugin capability grant.

The public socket may create and inspect missions, start read-only managed runs,
request fresh proof, and consume events. It cannot turn a read-only run into a
writer, answer a permission prompt, or execute a project recipe by changing a
boolean field.

## Provider boundary

Codex, Claude Code, OpenCode, and ACP are adapters behind one mission contract.
Provider output is evidence, not authority. A provider cannot declare its own
mission complete, approve its own permission request, or silently change the
workspace claim.

Handoffs bind the mission, worktree, redacted context, and timestamp to a SHA-256
digest. Starting a continuation requires that exact artifact. Any relevant state
change invalidates it.

## Proof boundary

A plugin or provider may contribute bounded evidence. Only Nagi core can execute
declared checks, determine freshness and criterion coverage, and mint a closure
proof. Required evidence that is missing, failed, stale, or produced for another
workspace blocks closure.

## Persisted contract changes

Mission journals, snapshots, plugin grants, public API schema, and public IDs are
compatibility surfaces. Changes require fixtures for the previous format, an
explicit migration or rejection path, and an RFC as described in
[`docs/rfcs/README.md`](../rfcs/README.md).

## Review checklist

For a change that crosses an authority boundary, reviewers should be able to
answer all of these from code and tests:

1. Who can request the action?
2. Who can approve it?
3. What exact state is persisted?
4. How is replay, retry, disconnect, and stale input handled?
5. What prevents a provider, plugin, or remote client from escalating itself?
