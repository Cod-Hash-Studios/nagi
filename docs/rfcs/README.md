# Request for comments

Use an RFC before implementation when a change affects:

- a provider adapter contract or provider-neutral mission behavior;
- plugin manifests, capabilities, host bindings, trust, or registry policy;
- a persisted journal, snapshot, grant, lock, public ID, or migration;
- a public socket or SDK contract that cannot remain backward compatible;
- an authority boundary described in `docs/architecture/`.

Small fixes, internal refactors, additive documentation, and reversible UI polish
do not need an RFC.

## Process

1. Copy [`0000-template.md`](0000-template.md) to `NNNN-short-title.md`. Use the
   next unused number.
2. Open a pull request containing the RFC only. Link a discussion or issue that
   shows the user problem.
3. Keep it open for at least seven days unless it closes an actively exploited
   security issue. Record objections and alternatives in the document.
4. A maintainer marks the RFC `accepted`, `rejected`, or `withdrawn`. Acceptance
   authorizes implementation, not automatic merge.
5. Implementation pull requests link the accepted RFC and include compatibility,
   migration, security, and rollback tests.

Accepted RFCs are immutable decision records. Corrections use a follow-up RFC.
Security-sensitive details may be developed privately and published after the
fix according to `SECURITY.md`.
