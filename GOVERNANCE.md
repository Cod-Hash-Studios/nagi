# Governance

Nagi is maintained by Cod'Hash Studios as an independent AGPL-licensed project.
The project aims to make parallel agent work persistent, provider-neutral, and
provable without weakening local user authority.

## Roles

- **Contributors** report issues, join discussions, review, document, or submit
  changes.
- **Maintainers** triage, review, merge, publish releases, manage registry safety,
  and enforce the security and conduct policies.
- **Project lead** resolves a decision only when maintainer consensus cannot be
  reached or an urgent security response needs one accountable owner.

Current maintainers are the members with write access in the
`Cod-Hash-Studios/nagi` repository. Access is granted for sustained, sound review
and maintenance work, not commit volume. It may be removed for inactivity,
security risk, or policy violations after a documented maintainer decision.

## Decisions

Normal changes use lazy consensus in a pull request: approval plus no unresolved
blocking review. Provider, plugin, persistence, public-contract, or authority
changes follow [`docs/rfcs/README.md`](docs/rfcs/README.md). A maintainer must not
merge their own security-sensitive change without another maintainer's review,
except for a private emergency fix that is reviewed immediately after release.

Maintainers can reject changes that increase support burden, create provider
lock-in, bypass proof or consent, or conflict with the product direction even
when the code works. The reason must be recorded publicly unless disclosure
would expose a vulnerability.

## Releases and security

Only maintainers may create release tags or change registry emergency controls.
Release artifacts must pass the repository's documented checks and supply-chain
verification. Security reports follow `SECURITY.md`; retaliation against a
good-faith reporter is not tolerated.

## Conflicts of interest

Anyone reviewing work from an employer, client, close collaborator, or competing
commercial interest should disclose that relationship and let another maintainer
make the final decision when practical.

## Changes to governance

Material governance changes use an RFC and a pull request open for at least
seven days. The license and preserved upstream attribution cannot be removed by
governance decision.
