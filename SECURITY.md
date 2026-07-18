# Security Policy

`imi` interacts directly with Linux kernel block layers and requires elevated privileges. All security boundaries and device-isolation logic are treated with critical priority.

## Supported Versions

Security updates and patches are exclusively applied to the latest `main` branch and the most recent Git release tag.

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.** To report a vulnerability or privilege escalation vector, please use the [Private Vulnerability Reporting](https://github.com/veralvx/imi/security/advisories/new) feature in this repository.

Please include the required technical context:

- Host OS and kernel version.
- Target device specifications.
- Exact reproduction steps or a technical proof-of-concept.
