# Native Windows

`sc.exe` supports Windows x64 without WSL or Git Bash. It uses Windows
PowerShell 5.1 or PowerShell 7 for shell tools. Git is only needed when your
project's commands use Git. Node and Rust are not runtime dependencies of the
release executable.

The Windows implementation is separate from the Unix shell implementation.
macOS/Linux keep their existing Bash behavior.

## Install and run

Windows-enabled releases contain `sc-x86_64-pc-windows-msvc.zip` and its
`.sha256` file. The archive contains one root `sc.exe`. Older releases without
those assets cannot be installed on Windows through `subc sc install`.

The Windows preview requires the Windows-enabled `subc` CLI. Pin the preview
release explicitly; the stable release channel is unchanged:

```powershell
$env:SC_CODE_VERSION = '0.1.4-windows.0'
subc.cmd sc install
subc.cmd sc
```

For a local build, use a Rust MSVC toolchain and Visual Studio C++ Build Tools:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --locked --release --target x86_64-pc-windows-msvc --bin sc
./scripts/package-windows.ps1
./target/x86_64-pc-windows-msvc/release/sc.exe --version
```

The static CRT flag avoids requiring a separate Visual C++ redistributable.
The package script validates the PE architecture and archive layout and creates
the SHA-256 checksum. It does not publish anything.

Settings, saved keys, history, global `AGENTS.md`, and sessions use
`%USERPROFILE%\.sc` (with a `HOMEDRIVE`/`HOMEPATH` fallback). A Unix `HOME`
environment variable is not required. Headless `-p` runs remain ephemeral.

## Shell selection

PowerShell is the default. `pwsh.exe` is preferred when installed; otherwise
the Windows PowerShell executable is used. To pin a shell explicitly:

```powershell
$env:SC_WINDOWS_SHELL = 'powershell'
$env:SC_POWERSHELL_PATH = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
sc
```

The model sees a tool named **PowerShell** with PowerShell-specific instructions.
Scripts run without profiles, with UTF-8 output, closed stdin, and no inherited
API-key/token/secret variables whose names end in `_API_KEY`, `_TOKEN`, or
`_SECRET`. As with Unix environment filtering, this is not exhaustive secret
isolation. Scripts are passed through temporary files to avoid command-line
quoting and length limitations.

PowerShell 5.1 has different native-command/pipeline behavior from Bash. Scripts
should explicitly check `$LASTEXITCODE` between native commands when necessary;
there is no promise of Bash-style `pipefail` in PowerShell. A successful
foreground directory change persists only inside the workspace roots.

Git Bash is an explicit alternative:

```powershell
$env:SC_WINDOWS_SHELL = 'bash'
# Only needed for nonstandard installations:
$env:SC_GIT_BASH_PATH = 'C:\Program Files\Git\bin\bash.exe'
sc
```

That selects the **Bash** tool and its existing Bash permission rules, and runs
Git Bash with `--noprofile --norc -o pipefail`. It does not select the System32
WSL `bash.exe` launcher or silently install Git. An invalid shell setting or
missing executable produces an actionable tool error.

## Permissions and cancellation

- PowerShell does not reuse Bash prefix grants. In default, accept-edits, and
  ask modes, ungranted scripts require approval of the complete command.
- Session/always approvals generate `PowerShell(exact:<UTF-8 hex>)` rules.
  Changing or appending to the script invalidates the grant. The hex encoding
  is not encryption; do not treat a saved command rule as secret storage.
- Bare `PowerShell` grants allow the whole tool. Bash-style PowerShell prefix
  rules are rejected rather than interpreted with a POSIX parser.
- Plan mode denies PowerShell even if a grant exists. Auto/bypass permit
  execution without prompting, subject to a conservative destructive-command
  safety floor. These checks are not a general PowerShell security sandbox.
- Each shell starts suspended, is attached to a Windows Job Object, and is then
  resumed. Timeout, cancellation, and session shutdown stop the owned process
  tree. Background stdout and stderr are drained into bounded rotating logs.
- Kernel filesystem/network sandboxing is not implemented on Windows.
  `--sandbox`, `--sandbox-net`, or equivalent configuration cause shell calls
  to fail closed. Job Objects provide lifecycle cleanup, not security isolation.

## Verification

`cargo test --locked --workspace` covers PowerShell output, native exit codes,
Unicode/quoting, cwd persistence, approval isolation, timeouts, cancellation,
descendant cleanup, background logging/rotation, and the end-to-end
Read → Edit → shell compile workflow. The Windows CI additionally exercises
PowerShell 7 and optional Git Bash, then builds and packages the x64 executable.
Windows ARM64 is not part of the current release job.

The native executable smoke test runs a real PowerShell tool call through a
local mock streaming gateway, verifies secret filtering and tool-result replay,
and loads global memory from an isolated Windows profile with no `HOME`:

```powershell
node scripts/windows-smoke.mjs ./target/x86_64-pc-windows-msvc/release/sc.exe
```

Node is needed only for this test harness, not to run `sc.exe`.

Verified on the supplied Windows x64 VM on 2026-09-08: 553 workspace tests
passed (3 pre-existing ignored benchmarks), formatting and Clippy were clean,
all 12 shell tests passed on both Windows
PowerShell 5.1 and PowerShell 7.6.5, and optional Git Bash execution passed.
The static-CRT release candidate was packaged, checksum-verified, installed via
the Windows `subc` installer with local release responses, and exercised through
the mock gateway and interactive TUI launch/quit. Browser login and real model
inference were deliberately not tested. These results describe the local
candidate; release builds also run the automated CI checks before publication.
