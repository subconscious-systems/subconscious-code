#!/usr/bin/env python3
"""Pinned real-task evaluation via the public Marathon CLI (Python 3.12+)."""
from __future__ import annotations

import argparse
from collections import deque
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import fnmatch
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from urllib.parse import urlsplit
import uuid

ROOT = Path(__file__).resolve().parents[1]
PUBLIC_CHECK = "cargo test --offline --locked -p rc-tools --lib"
GRADE_TARGET = "independent_evaluation_contract"
TEST_RESULT = re.compile(r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;")
SHA = re.compile(r"[0-9a-f]{40}\Z")
IDENTIFIER = re.compile(r"[a-z][a-z0-9_]{0,63}\Z")


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def relative_path(value: str) -> bool:
    if not isinstance(value, str):
        return False
    path = PurePosixPath(value)
    return bool(value) and "\\" not in value and ":" not in value and not path.is_absolute() and ".." not in path.parts


def load_suite(path: Path) -> dict:
    suite = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(suite, dict) or type(suite.get("schema_version")) is not int or suite["schema_version"] != 1:
        raise ValueError("unsupported suite schema")
    if not isinstance(suite.get("suite_id"), str) or not IDENTIFIER.fullmatch(suite["suite_id"].replace("-", "_")):
        raise ValueError("unsupported suite schema or id")
    if not isinstance(suite.get("source_revision"), str) or not SHA.fullmatch(suite["source_revision"]) or not relative_path(suite.get("grader", "")):
        raise ValueError("suite must pin a full source commit and a relative grader path")
    if type(suite.get("preserve_tests")) is not int or suite["preserve_tests"] < 1:
        raise ValueError("suite requires pass-to-pass checks")
    if not isinstance(suite.get("preserve_filter"), str) or not suite["preserve_filter"] or not isinstance(suite.get("tasks"), list) or not suite["tasks"]:
        raise ValueError("suite requires nonempty tasks and test filters")
    ids = set()
    for task in suite["tasks"]:
        if not isinstance(task, dict):
            raise ValueError("task descriptors must be objects")
        task_id = task.get("id", "")
        if not isinstance(task_id, str) or not IDENTIFIER.fullmatch(task_id) or task_id in ids:
            raise ValueError("invalid or duplicate task id")
        ids.add(task_id)
        if not isinstance(task.get("prompt"), str) or not task["prompt"] or not isinstance(task.get("goal_filter"), str) or not task["goal_filter"] or type(task.get("goal_tests")) is not int or task["goal_tests"] < 1:
            raise ValueError("task must specify a prompt and nonempty acceptance checks")
        if not isinstance(task.get("editable"), list) or not task["editable"] or not all(relative_path(p) for p in task["editable"]):
            raise ValueError("invalid editable paths")
        if not isinstance(task.get("oracle_revision"), str) or not SHA.fullmatch(task["oracle_revision"]) or not isinstance(task.get("oracle_files"), list) or not task["oracle_files"]:
            raise ValueError("task must pin its validation oracle")
        if not relative_path(task.get("oracle_patch", "")) or not isinstance(task.get("oracle_sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", task["oracle_sha256"]):
            raise ValueError("task must pin a local oracle patch hash")
        if not all(relative_path(p) and any(fnmatch.fnmatchcase(p, pattern) for pattern in task["editable"]) for p in task["oracle_files"]):
            raise ValueError("oracle changes must be inside editable paths")
    return suite


def endpoint(value: str) -> str:
    try:
        parsed = urlsplit(value)
        valid = parsed.scheme in ("http", "https") and parsed.hostname and parsed.port != 0
        valid = valid and not (parsed.username or parsed.password or parsed.query or parsed.fragment)
    except ValueError:
        valid = False
    if not valid:
        raise ValueError("base URL must be HTTP(S), without credentials, query, or fragment")
    return value.rstrip("/")


def minimal_env(profile: Path | None = None) -> dict[str, str]:
    keep = ("PATH", "SystemRoot", "TEMP", "TMP", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
            "LIB", "LIBPATH", "INCLUDE", "VCToolsInstallDir", "WindowsSdkDir", "WindowsSDKVersion",
            "VSINSTALLDIR", "ProgramFiles", "ProgramFiles(x86)", "ProgramData", "COMSPEC", "windir",
            "PROCESSOR_ARCHITECTURE", "PROCESSOR_ARCHITEW6432")
    env = {key: os.environ[key] for key in keep if key in os.environ}
    env.setdefault("CARGO_HOME", str(Path.home() / ".cargo"))
    env.setdefault("RUSTUP_HOME", str(Path.home() / ".rustup"))
    if profile is not None:
        env.update(HOME=str(profile), USERPROFILE=str(profile))
    env.update(CARGO_TERM_COLOR="never", CARGO_BUILD_JOBS="2")
    return env


def compiler_env(env: dict[str, str]) -> dict[str, str]:
    if os.name != "nt" or (shutil.which("link.exe", path=env.get("PATH")) and env.get("LIB")):
        return env
    installer = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Microsoft Visual Studio" / "Installer" / "vswhere.exe"
    if not installer.is_file():
        raise RuntimeError("MSVC developer environment unavailable; initialize it before evaluation")
    probe = subprocess.run([str(installer), "-latest", "-products", "*", "-utf8", "-property", "installationPath"],
                           env=env, capture_output=True, check=True, creationflags=subprocess.CREATE_NO_WINDOW)
    installation = probe.stdout.decode("utf-8", errors="replace").strip()
    batch = Path(installation) / "VC" / "Auxiliary" / "Build" / "vcvars64.bat"
    if not installation or not batch.is_file():
        raise RuntimeError("MSVC x64 compiler environment unavailable")
    # This command runs only the installed SDK initializer, with an already
    # credential-free environment; evaluated source never constructs it.
    comspec = env.get("COMSPEC", r"C:\Windows\System32\cmd.exe")
    command = f'"{comspec}" /d /s /c ""{batch}" >nul && set"'
    initialized = subprocess.run(command, env=env, capture_output=True, check=True,
                                 creationflags=subprocess.CREATE_NO_WINDOW)
    result = {key.upper(): value for key, value in env.items()}
    allowed = ("PATH", "LIB", "LIBPATH", "INCLUDE", "VC", "VS", "WINDOWS", "UNIVERSALCRT", "UCRT", "FRAMEWORK")
    for line in initialized.stdout.decode(errors="replace").splitlines():
        key, separator, value = line.partition("=")
        if separator and key.upper().startswith(allowed):
            result[key.upper()] = value
    return result


class Capture:
    """Bounded head/tail capture, including credentials split across chunks."""
    def __init__(self, secret: str = "", limit: int = 1024 * 1024):
        self.secret = secret.encode()
        self.limit = limit
        self.pending = b""
        self.head = bytearray()
        self.tail = deque()
        self.tail_bytes = 0
        self.total = 0

    def _store(self, data: bytes):
        self.total += len(data)
        take = min(len(data), self.limit // 2 - len(self.head))
        self.head.extend(data[:take])
        data = data[take:]
        if data:
            self.tail.append(data)
            self.tail_bytes += len(data)
            while self.tail_bytes > self.limit // 2:
                excess = self.tail_bytes - self.limit // 2
                first = self.tail.popleft()
                removed = min(excess, len(first))
                self.tail_bytes -= removed
                if removed < len(first):
                    self.tail.appendleft(first[removed:])

    def feed(self, chunk: bytes, final: bool = False):
        if not self.secret:
            self._store(chunk)
            return
        data = self.pending + chunk
        end = len(data) if final else max(0, len(data) - len(self.secret) + 1)
        position = 0
        while True:
            found = data.find(self.secret, position)
            if found < 0 or found >= end:
                break
            self._store(data[position:found] + b"[credential redacted]")
            position = found + len(self.secret)
        end = max(end, position)
        self._store(data[position:end])
        self.pending = data[end:]

    def text(self) -> str:
        self.feed(b"", final=True)
        body = bytes(self.head)
        if self.total > self.limit:
            body += b"\n[log middle omitted]\n"
        body += b"".join(self.tail)
        return body.decode("utf-8", errors="replace")


class WindowsJob:
    """Kill owned descendants on job close, including after a parent exit."""
    def __init__(self, process: subprocess.Popen):
        import ctypes
        from ctypes import wintypes

        class Basic(ctypes.Structure):
            _fields_ = [("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64),
                        ("flags", wintypes.DWORD), ("min_ws", ctypes.c_size_t),
                        ("max_ws", ctypes.c_size_t), ("active", wintypes.DWORD),
                        ("affinity", ctypes.c_size_t), ("priority", wintypes.DWORD),
                        ("scheduling", wintypes.DWORD)]

        class Extended(ctypes.Structure):
            _fields_ = [("basic", Basic), ("io", ctypes.c_uint64 * 6),
                        ("process_memory", ctypes.c_size_t), ("job_memory", ctypes.c_size_t),
                        ("peak_process", ctypes.c_size_t), ("peak_job", ctypes.c_size_t)]

        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
        self.kernel.CreateJobObjectW.restype = wintypes.HANDLE
        self.kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
        self.kernel.SetInformationJobObject.restype = wintypes.BOOL
        self.kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        self.kernel.AssignProcessToJobObject.restype = wintypes.BOOL
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        self.kernel.CloseHandle.restype = wintypes.BOOL
        self.kernel.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
        self.kernel.TerminateJobObject.restype = wintypes.BOOL
        self.handle = self.kernel.CreateJobObjectW(None, None)
        limits = Extended()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not self.handle or not self.kernel.SetInformationJobObject(self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            self.close()
            raise ctypes.WinError(ctypes.get_last_error())
        if not self.kernel.AssignProcessToJobObject(self.handle, int(process._handle)):
            self.close()
            if process.poll() is not None:
                return  # A short version probe may have already exited.
            raise ctypes.WinError(ctypes.get_last_error())

    def close(self):
        if getattr(self, "handle", None):
            self.kernel.CloseHandle(self.handle)
            self.handle = None

    def terminate(self):
        if self.handle:
            self.kernel.TerminateJobObject(self.handle, 1)


@dataclass
class ProcessResult:
    returncode: int
    timed_out: bool
    wall_ms: int
    log: str


def execute(command: list[str], cwd: Path, env: dict, seconds: int, secret: str = "", log_path: Path | None = None) -> ProcessResult:
    started = time.monotonic()
    capture = Capture(secret)
    flags = subprocess.CREATE_NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0
    process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               start_new_session=os.name != "nt", creationflags=flags)
    job = None
    timed_out = False

    def read():
        for chunk in iter(lambda: process.stdout.read(4096), b""):
            capture.feed(chunk)

    reader = threading.Thread(target=read, daemon=True)
    try:
        if os.name == "nt":
            job = WindowsJob(process)
        reader.start()
        try:
            process.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            timed_out = True
    finally:
        if job is not None:
            if timed_out:
                job.terminate()
            job.close()
        elif os.name != "nt":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if process.poll() is None:
            process.kill()
        process.wait()
        if reader.ident is not None:
            reader.join(timeout=5)
        process.stdout.close()
    if reader.is_alive():
        raise RuntimeError("subprocess capture did not terminate")
    text = capture.text()
    if log_path:
        log_path.write_text(text, encoding="utf-8")
    return ProcessResult(process.returncode, timed_out, round((time.monotonic() - started) * 1000), text)


def inventory(root: Path) -> dict[str, str]:
    if root.is_symlink() or root.is_junction():
        raise ValueError("workspace must be a regular directory")
    files = {}
    for path in root.rglob("*"):
        name = path.relative_to(root).as_posix()
        if path.is_symlink() or path.is_junction():
            raise ValueError("workspace contains a link")
        if name == "target" or name.startswith("target/"):
            continue
        if path.is_file():
            files[name] = digest(path)
    return files


def changed_files(before: dict, after: dict) -> list[str]:
    return sorted(name for name in before.keys() | after.keys() if before.get(name) != after.get(name))


def forbidden_changes(changes: list[str], task: dict) -> list[str]:
    reserved = f"crates/rc-tools/tests/{GRADE_TARGET}.rs"
    return [name for name in changes if name == reserved or not any(fnmatch.fnmatchcase(name, pattern) for pattern in task["editable"])]


def atomic_json(path: Path, data: dict):
    temporary = path.with_suffix(path.suffix + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(data, stream, indent=2, ensure_ascii=False, allow_nan=False)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def parse_grade(result: ProcessResult, expected: int) -> dict:
    matches = TEST_RESULT.findall(result.log)
    if result.timed_out:
        return {"status": "grade_error", "reason": "grader_timeout"}
    if not matches:
        if result.returncode != 0 and "error[E" in result.log:
            return {"status": "unresolved", "reason": "candidate_compile_error"}
        return {"status": "grade_error", "reason": "missing_test_receipt"}
    status, passed, failed, ignored = matches[-1]
    passed, failed, ignored = int(passed), int(failed), int(ignored)
    if passed + failed != expected or ignored:
        return {"status": "grade_error", "reason": "incorrect_test_count"}
    resolved = result.returncode == 0 and status == "ok" and failed == 0
    return {"status": "resolved" if resolved else "unresolved", "passed": passed, "failed": failed}


def read_accounting(path: Path) -> dict:
    if not path.is_file() or path.stat().st_size > 1024 * 1024:
        return {"report_status": "missing_or_oversized"}
    try:
        data = json.loads(path.read_text(encoding="utf-8"), parse_constant=reject_json_constant)
    except (ValueError, OSError):
        return {"report_status": "invalid"}
    if not isinstance(data, dict) or data.get("schema_version") != 1:
        return {"report_status": "unsupported"}
    result = {"report_status": "valid"}
    allowed = ("stop", "length", "iteration_limit", "no_progress", "incomplete", "time_limit", "cancelled", "repeated_failure")
    if data.get("outcome") in allowed:
        result["outcome"] = data["outcome"]
    for name in ("wall_time_ms", "request_count", "tool_call_count", "tool_error_count", "tool_denied_count", "retry_count"):
        value = data.get(name)
        if type(value) is int and value >= 0:
            result[name] = value
    usage = data.get("usage", {})
    result["usage"] = {name: usage[name] for name in ("input_tokens", "cached_input_tokens", "output_tokens", "total_tokens")
                       if isinstance(usage, dict) and type(usage.get(name)) is int and usage[name] >= 0}
    return result


def reject_json_constant(_: str):
    raise ValueError("non-finite JSON number")


def summarize(records: list[dict], planned: int) -> dict:
    resolved = sum(record["task_status"] == "resolved" for record in records)
    clean = sum(record.get("runtime_clean", False) for record in records)
    latencies = [record["agent_wall_ms"] for record in records if "agent_wall_ms" in record]
    complete = len(records) == planned
    return {"planned_trials": planned, "completed_trials": len(records), "complete": complete,
            "resolved_trials": resolved, "resolved_rate": resolved / planned if complete else None,
            "runtime_clean_trials": clean,
            "grade_error_trials": sum(record["task_status"] == "grade_error" for record in records),
            "invalid_patch_trials": sum(record["task_status"] == "invalid_patch" for record in records),
            "median_agent_wall_ms": statistics.median(latencies) if latencies else None}


class Evaluator:
    def __init__(self, repo: Path, suite_path: Path, output: Path, grade_seconds: int):
        self.repo = repo.resolve()
        self.suite_path = suite_path.resolve()
        self.suite = load_suite(suite_path)
        self.grader = (suite_path.parent / self.suite["grader"]).resolve()
        if not self.grader.is_relative_to(suite_path.parent.resolve()):
            raise ValueError("grader path escapes suite directory")
        output.mkdir(parents=True, exist_ok=False)
        self.output = output.resolve()
        # Outside the checkout: inherited .gitignore/AGENTS files and access to
        # the contributor's solution history must not shape a trial.
        self.owned_workspace = tempfile.TemporaryDirectory(prefix="marathon-task-eval-")
        self.workspace_parent = Path(self.owned_workspace.name).resolve()
        self.workspace = self.workspace_parent / "workspace"
        self.profile = self.output / "profile"
        self.profile.mkdir()
        self.env = compiler_env(minimal_env(self.profile))
        self.env["CARGO_TARGET_DIR"] = str(self.output / "cargo-target")
        self.grade_seconds = grade_seconds
        self.archive = self.output / "source.tar"
        subprocess.run(["git", "-C", str(self.repo), "archive", "--format=tar", "--output", str(self.archive), self.suite["source_revision"]], check=True, capture_output=True)
        self.metadata = {"schema_version": 1, "suite_id": self.suite["suite_id"],
                         "source_revision": self.suite["source_revision"], "suite_sha256": digest(suite_path),
                         "grader_sha256": digest(self.grader), "source_archive_sha256": digest(self.archive),
                         "created_utc": datetime.now(timezone.utc).isoformat(), "platform": sys.platform,
                         "python_version": sys.version.split()[0]}
        self.metadata["oracle_sha256"] = {task["id"]: task["oracle_sha256"] for task in self.suite["tasks"]}
        for tool in ("cargo", "rustc"):
            result = execute([tool, "--version"], self.repo, self.env, 30)
            if result.returncode:
                raise RuntimeError(f"{tool} is unavailable")
            self.metadata[f"{tool}_version"] = result.log.strip()
        atomic_json(self.output / "provenance.json", self.metadata)

    def reset(self):
        # Verify the final target before a recursive removal, including links
        # installed by evaluated code. Never remove the output root itself.
        target = self.workspace.resolve()
        if target == self.workspace_parent or target.parent != self.workspace_parent or self.workspace.is_symlink() or self.workspace.is_junction():
            raise ValueError("unsafe workspace reset target")
        if self.workspace.exists():
            shutil.rmtree(self.workspace)
        self.workspace.mkdir()
        with tarfile.open(self.archive) as archive:
            if any(not (member.isfile() or member.isdir()) or not relative_path(member.name.rstrip("/")) for member in archive.getmembers()):
                raise ValueError("source archive contains unsupported paths or links")
            archive.extractall(self.workspace, filter="data")
        settings = self.workspace / ".sc" / "settings.json"
        settings.parent.mkdir(exist_ok=True)
        allow = [f"Bash({PUBLIC_CHECK})", f"PowerShell(exact:{PUBLIC_CHECK.encode().hex()})"]
        atomic_json(settings, {"permissions": {"default_mode": "acceptEdits", "allow": allow}})
        return inventory(self.workspace)

    def oracle(self, task: dict):
        patch = (self.suite_path.parent / task["oracle_patch"]).resolve()
        if not patch.is_relative_to(self.suite_path.parent) or digest(patch) != task["oracle_sha256"]:
            raise ValueError("oracle patch failed provenance validation")
        before = inventory(self.workspace)
        subprocess.run(["git", "apply", str(patch)], cwd=self.workspace, check=True, capture_output=True)
        changes = changed_files(before, inventory(self.workspace))
        if not changes or set(changes) - set(task["oracle_files"]) or forbidden_changes(changes, task):
            raise ValueError("oracle patch changed unexpected files")

    def grade(self, task: dict, directory: Path) -> dict:
        destination = self.workspace / "crates" / "rc-tools" / "tests" / f"{GRADE_TARGET}.rs"
        destination.parent.mkdir(exist_ok=True)
        shutil.copyfile(self.grader, destination)
        checks = {}
        for name, test_filter, count in (("preserve", self.suite["preserve_filter"], self.suite["preserve_tests"]),
                                         ("goal", task["goal_filter"], task["goal_tests"])):
            command = ["cargo", "test", "--offline", "--locked", "-p", "rc-tools", "--test", GRADE_TARGET,
                       test_filter, "--", "--test-threads=1"]
            result = execute(command, self.workspace, self.env, self.grade_seconds, log_path=directory / f"{name}.log")
            checks[name] = parse_grade(result, count)
        statuses = [check["status"] for check in checks.values()]
        status = "grade_error" if "grade_error" in statuses else "resolved" if all(s == "resolved" for s in statuses) else "unresolved"
        return {"task_status": status, "checks": checks}

    def validate(self, tasks: list[dict]) -> list[dict]:
        records = []
        for task in tasks:
            trial = self.output / f"control-{task['id']}"
            trial.mkdir()
            self.reset()
            baseline = trial / "baseline"
            baseline.mkdir()
            before = self.grade(task, baseline)
            self.reset()
            self.oracle(task)
            oracle = trial / "oracle"
            oracle.mkdir()
            after = self.grade(task, oracle)
            valid = before["checks"]["preserve"]["status"] == "resolved" and before["checks"]["goal"]["status"] == "unresolved" and after["task_status"] == "resolved"
            record = {"task_id": task["id"], "baseline": before, "oracle": after, "valid": valid}
            records.append(record)
            atomic_json(trial / "result.json", record)
            atomic_json(self.output / "validation.json", {**self.metadata, "mode": "grader_validation", "controls": records, "valid": all(r["valid"] for r in records), "complete": len(records) == len(tasks)})
            print(f"{task['id']}: control {'valid' if valid else 'FAILED'}", flush=True)
        return records

    def agent(self, task: dict, args, directory: Path, key: str) -> dict:
        env = dict(self.env)
        env.update(SC_API_KEY=key, SC_DEFAULT_MODE="acceptEdits", SC_DLR_ENABLED="false",
                   SC_REQUEST_GZIP="false", SC_MAX_RETRIES="0", SC_RESOURCE_LIMITS="0",
                   SC_MAX_ITERS=str(args.max_iters), SC_TURN_TIMEOUT_MS=str(args.seconds * 1000),
                   SC_TOOL_RESULT_CAP="4096", SC_BASH_OUTPUT_CAP="8192")
        report_path = directory / "accounting.json"
        prompt = task["prompt"] + f"\nOnly change rc-tools Rust source or tests. Keep Cargo manifests, lockfiles, and settings unchanged. The permitted verification command is exactly: {PUBLIC_CHECK}"
        command = [str(args.binary), "--model", args.model, "--base-url", args.base_url,
                   "--max-tokens", str(args.max_tokens), "--reasoning-effort", args.reasoning_effort,
                   "--benchmark-report", str(report_path), "-p", prompt]
        result = execute(command, self.workspace, env, args.seconds + 10, key, directory / "agent.log")
        accounting = read_accounting(report_path)
        return {"agent_returncode": result.returncode, "process_timeout": result.timed_out,
                "agent_wall_ms": result.wall_ms, "accounting": accounting,
                "runtime_clean": not result.timed_out and result.returncode == 0 and accounting.get("outcome") == "stop"}

    def live(self, tasks: list[dict], args, key: str) -> list[dict]:
        # Validate pass-to-pass behavior and warm the offline compiler before
        # spending API tokens. Missing dependencies are setup failures.
        self.reset()
        preflight = self.output / "compiler-preflight"
        preflight.mkdir()
        checks = self.grade(tasks[0], preflight)
        if checks["checks"]["preserve"]["status"] != "resolved":
            raise RuntimeError("compiler preflight failed; inspect preserve.log")
        records = []
        planned = len(tasks) * args.trials
        metadata = {**self.metadata, "mode": "agent_trials", "model": args.model,
                    "base_url": args.base_url, "binary_sha256": digest(args.binary),
                    "budgets": {"completion_tokens_per_request": args.max_tokens, "max_iters": args.max_iters,
                                "turn_seconds": args.seconds, "outer_seconds": args.seconds + 10,
                                "grader_seconds": self.grade_seconds, "reasoning_effort": args.reasoning_effort},
                    "trials_per_task": args.trials}
        atomic_json(self.output / "provenance.json", metadata)
        for trial_number in range(args.trials):
            rotation = trial_number % len(tasks)
            for task in tasks[rotation:] + tasks[:rotation]:
                directory = self.output / f"{task['id']}-{trial_number + 1}"
                directory.mkdir()
                before = self.reset()
                record = {"task_id": task["id"], "trial": trial_number + 1}
                record.update(self.agent(task, args, directory, key))
                try:
                    changes = changed_files(before, inventory(self.workspace))
                    forbidden = forbidden_changes(changes, task)
                    record.update(changed_files=changes, forbidden_changes=forbidden)
                    if forbidden:
                        record["task_status"] = "invalid_patch"
                    else:
                        for name in changes:
                            source = self.workspace / name
                            if source.is_file():
                                target = directory / "candidate" / name
                                target.parent.mkdir(parents=True, exist_ok=True)
                                shutil.copyfile(source, target)
                        record.update(self.grade(task, directory))
                except ValueError:
                    record.update(task_status="invalid_patch", reason="workspace_link_detected")
                records.append(record)
                atomic_json(directory / "result.json", record)
                with (self.output / "trials.jsonl").open("a", encoding="utf-8") as journal:
                    journal.write(json.dumps(record, ensure_ascii=False, allow_nan=False) + "\n")
                    journal.flush()
                    os.fsync(journal.fileno())
                atomic_json(self.output / "summary.json", {**metadata, **summarize(records, planned), "trials": records})
                print(f"{task['id']} trial {trial_number + 1}: {record['task_status']}; runtime_clean={record['runtime_clean']}", flush=True)
        return records


def positive(value: str) -> int:
    result = int(value)
    if result < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("list", "validate", "run"))
    parser.add_argument("--repo", type=Path, default=ROOT)
    parser.add_argument("--suite", type=Path, default=Path(__file__).parent / "tasks.json")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--task", action="append", default=[])
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--model")
    parser.add_argument("--base-url")
    parser.add_argument("--trials", type=positive, default=1)
    parser.add_argument("--max-tokens", type=positive, default=2048)
    parser.add_argument("--max-iters", type=positive, default=8)
    parser.add_argument("--seconds", type=positive, default=180)
    parser.add_argument("--grader-seconds", type=positive, default=600)
    parser.add_argument("--reasoning-effort", default="high", choices=("off", "high", "max"))
    args = parser.parse_args(argv)
    try:
        suite = load_suite(args.suite)
        tasks = [task for task in suite["tasks"] if not args.task or task["id"] in args.task]
        if not tasks or set(args.task) - {task["id"] for task in tasks}:
            raise ValueError("unknown or empty task selection")
        if args.mode == "list":
            print(json.dumps({"suite_id": suite["suite_id"], "source_revision": suite["source_revision"],
                              "tasks": [{"id": task["id"], "issue": task["issue"]} for task in tasks]}, indent=2))
            return 0
        key = os.environ.get("SC_API_KEY", "")
        if args.mode == "run":
            if not args.binary or not args.model or not args.base_url:
                raise ValueError("run requires --binary, --model, and --base-url")
            if not key:
                raise ValueError("set SC_API_KEY for a live run; do not put credentials in arguments")
            if key in args.model or key in args.base_url:
                raise ValueError("credentials must appear only in SC_API_KEY")
            args.binary = args.binary.resolve(strict=True)
            args.base_url = endpoint(args.base_url)
        output = args.out or ROOT / "benchmark-results" / (datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8])
        evaluator = Evaluator(args.repo, args.suite, output, args.grader_seconds)
        if args.mode == "validate":
            records = evaluator.validate(tasks)
            status = 0 if all(record["valid"] for record in records) else 1
        else:
            records = evaluator.live(tasks, args, key)
            status = 1 if any(record["task_status"] == "grade_error" for record in records) else 0
        print(f"Artifacts: {evaluator.output}")
        return status
    except (ValueError, OSError, RuntimeError, subprocess.CalledProcessError) as error:
        # subprocess command strings or output could contain model credentials;
        # report only controlled failures and use per-trial redacted logs.
        if isinstance(error, subprocess.CalledProcessError):
            print("Evaluation setup failed: a pinned git object is unavailable; fetch the suite commits.", file=sys.stderr)
        else:
            print(f"Evaluation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
