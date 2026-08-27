# Support

Start with the [Getting started](docs/GETTING_STARTED.md),
[Configuration](docs/CONFIGURATION.md), and
[Troubleshooting](docs/TROUBLESHOOTING.md) guides. Run `sc doctor` to collect a
sanitized compatibility report.

Use GitHub Issues for reproducible bugs and feature proposals. Include:

- `sc --version`, operating system, architecture, and terminal;
- the provider type and sanitized base URL (never the key);
- the smallest reproduction and the expected/actual behavior;
- sanitized `sc doctor` output and relevant logs;
- whether the issue reproduces with `SC_DLR_ENABLED=false`.

Use GitHub Discussions, if enabled, for general questions and design ideas.
Please do not post API keys, DLR ingress tokens, private prompts, proprietary
source, or complete session files.

Report potential vulnerabilities privately as described in
[SECURITY.md](SECURITY.md).
