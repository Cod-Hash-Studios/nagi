# Plugin security model

Nagi supports two plugin runtimes with intentionally different trust models.
Sandboxed WASI components are the marketplace default. Native command plugins
remain available for compatibility and local automation, but they are equivalent
to running third-party software on the host.

## Sandboxed components

A manifest v2 `wasi-component` starts with no inherited filesystem, network,
environment, or process access. The host bounds memory, fuel, wall time, and
output. A trap, timeout, or malformed UI document terminates that invocation,
not Nagi's render loop or the next plugin invocation.

Capabilities are explicit strings in the manifest. A requested capability is
unavailable until Nagi has both a host binding for it and an exact grant for the
installed version. Approval binds:

- plugin id and semantic version;
- immutable source commit;
- manifest and component checksums;
- runtime and requested capability set.

Changing any bound value, or adding a capability, disables the plugin until the
user reviews and approves the new grant. Revocation takes effect before the next
host call.

## Native plugins

Legacy and native plugins execute as local processes. They must be installed or
linked with an explicit unrestricted-trust decision before they can be enabled.
Nagi scrubs the inherited environment, supplies only documented Nagi context,
applies process and output limits, and keeps plugin config and state in separate
directories. These controls reduce accidental exposure but do not make native
code a sandbox.

The UI and `nagi plugin inspect` must label this runtime as unrestricted. Native
plugins are never eligible for the verified marketplace install path.

## Packaging and registry

`nagi plugin pack` creates a non-overwriting bundle with SHA-256 checksums, an
SPDX 2.3 SBOM, and provenance metadata. Registry ingestion resolves a public
repository to a 40-character commit SHA before fetching content. Mutable refs,
checksum mismatches, native runtimes, invalid manifests, oversized artifacts,
failed scans, and blocked repositories fail closed.

Registry labels describe evidence, not a blanket safety claim. Stars are never a
security score. A failed refresh preserves the last known-good snapshot, while a
maintainer kill switch can immediately remove a listing.

## UI and proof contributions

Plugins declare structured actions, inspector tabs, widgets, or evidence. Nagi
renders validated data with bounded text and deterministic ordering. Plugins do
not receive raw terminal drawing authority and cannot mint mission closure proof.

## Testing expectations

Every new host binding needs tests for the allowed call, missing grant, revoked
grant, version or checksum drift, malformed input, timeout, and resource
exhaustion. Native changes need tests for explicit trust and environment
scrubbing. Registry changes need immutable-source, checksum, scan-failure, and
emergency-removal coverage.

Report a suspected escape or escalation privately through [`SECURITY.md`](../../SECURITY.md).
