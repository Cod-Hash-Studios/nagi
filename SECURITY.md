# Security policy

Please do not open public issues for suspected vulnerabilities.

Report them privately through GitHub's **Report a vulnerability** flow for this
repository. Include the affected revision, platform, reproduction steps,
impact, and the smallest useful logs. Remove tokens, prompts, source code, and
other personal data from the report.

Nagi is under active development and has no signed public release channel yet.
Builds from source should be treated as experimental. Automated publishing,
self-update, and remote binary download remain disabled until their security
review is complete.

Security-sensitive areas include:

- local sockets and runtime file permissions;
- mission journals, snapshots, and single-writer handoff;
- worktree ownership and command execution boundaries;
- provider permission requests and response routing;
- plugin installation and remote binary acquisition.

We will acknowledge a complete report as soon as practical and coordinate a
fix and disclosure timeline with the reporter.
