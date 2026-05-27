# Security policy

heso runs untrusted JavaScript inside a sandboxed QuickJS engine and
issues HTTP requests on behalf of the operator. The sandbox is the
trust boundary; bugs that let JS escape it, exfiltrate operator
state, or forge a plat signature are in scope.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting on this repository
(**Settings → Security → Report a vulnerability**) rather than opening
a public issue.

Include, where possible:

- The heso version (`heso --version`).
- A minimal reproduction: a URL, command, or script that demonstrates
  the behavior.
- The observed outcome and why it is a vulnerability rather than an
  ordinary bug.

We aim to acknowledge reports within a few business days. Coordinated
disclosure is appreciated; we'll work with you on a timeline.

## Out of scope

- CAPTCHA evasion, bot-detect bypass, or anti-fingerprinting features.
  heso surfaces these as structured failures
  (`partial_reason: "bot_challenge"`) by design.
- Sites the engine doesn't render correctly because QuickJS lacks a V8
  feature. These are honest engine limitations; report them as bugs.
- Anything that requires an attacker to already control the local
  filesystem or the heso binary itself.
