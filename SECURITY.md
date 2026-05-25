# Security Policy

heso is pre-alpha, but security reports are still taken seriously.

## Supported Versions

Only the latest published release is supported. If you are testing from
`main`, please include the commit SHA in your report.

## Reporting a Vulnerability

Please report vulnerabilities privately to the maintainer before opening a
public issue. Include:

- the affected heso version or commit SHA,
- the operating system and install channel,
- exact reproduction steps,
- the expected and actual impact.

Do not include private keys, live credentials, session cookies, or third-party
data in a public issue. If a report involves signed plats, receipts, package
publishing, or release artifacts, mention that in the subject so it can be
triaged quickly.

## Scope

In scope:

- incorrect plat, cassette, receipt, or signature verification,
- package-install or binary-resolution vulnerabilities,
- network replay behavior that silently falls back to live network,
- crashes or hangs triggered by untrusted page input.

Out of scope for now:

- CAPTCHA bypass,
- anti-bot evasion,
- visual/browser-fidelity gaps,
- denial of service against a local command run on intentionally huge input.
