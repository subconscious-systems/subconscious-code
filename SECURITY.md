# Security policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's private
vulnerability reporting for this repository:

1. Open the repository's **Security** tab.
2. Select **Report a vulnerability**.
3. Include the affected commit or version, impact, reproduction steps, and any
   suggested mitigation.

If private reporting is not enabled, contact the maintainers privately through
the organization profile rather than publishing exploit details. Please allow
a reasonable remediation window before disclosure. We will acknowledge a
complete report as soon as maintainers are available and coordinate status and
disclosure through the private report.

## Supported versions

This project is in active development before a stable 1.0 release. Security
fixes are applied to the latest `main` branch and the newest published release;
older commits and development snapshots are not maintained.

## Deployment guidance

- Keep API keys and DLR ingress tokens in environment variables or a secret
  manager, never in committed settings or session files.
- Treat `--dangerously-skip-permissions`, `auto` mode, shell access, and network
  access as privileged capabilities.
- Run untrusted tasks in an isolated VM or container. Permission prompts and
  the hard-deny floor are useful controls, not complete isolation.
- Bind a development DLR sidecar to loopback. For remote deployment, require
  TLS, an ingress token, strict upstream allowlisting, durable state, and
  network-level access controls.
- Sanitize `sc doctor` output, sessions, traces, and benchmark artifacts before
  sharing them; prompts and tool results may contain sensitive source.

The threat model and hardening details for DLR are documented in
[`integrations/dlr/docs/SIDECAR.md`](integrations/dlr/docs/SIDECAR.md).
