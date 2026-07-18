[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

Write-Error @"
Nagi does not publish signed Windows binaries yet.

The inherited installer is intentionally disabled. Build from source only after
reviewing the current platform limitations in the repository README:
https://github.com/Cod-Hash-Studios/nagi
"@

exit 1
