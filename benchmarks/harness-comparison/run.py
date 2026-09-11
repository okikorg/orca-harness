#!/usr/bin/env python3
"""Run the isolated Orcacode, Pi, Oh My Pi, and Claude Code live benchmark.

The runner intentionally owns orchestration only: fresh fixture copies, balanced
ordering, timestamped raw streams, timeouts, and post-run workspace validation.
`analyze.py` turns those raw records into comparable metrics.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import queue
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
DEFAULT_MODEL = "anthropic/claude-haiku-4.5"
OMP_DISCOVERY_PROVIDERS = (
    "native",
    "claude",
    "codex",
    "gemini",
    "opencode",
    "cursor",
    "github",
    "agents",
    "agents-md",
)
SAFETY_SUFFIX = (
    "Do not use shell or code execution and do not start a server. "
    "Use only the registered workspace tools. Your entire final response must be "
    "only the requested terminal line, with no explanation, markdown, code fence, "
    "or trailing punctuation."
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run a balanced, isolated four-harness benchmark."
    )
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--effort", default="low")
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--timeout-seconds", type=float, default=45.0)
    parser.add_argument("--max-steps", type=int, default=24)
    parser.add_argument("--max-output-tokens", type=int, default=1536)
    parser.add_argument(
        "--profile", choices=("all", "read", "edit"), default="all"
    )
    parser.add_argument(
        "--harness",
        choices=("all", "both", "orca", "pi", "omp", "claude"),
        default="all",
        help="'both' retains the Orcacode/Pi-only profile",
    )
    parser.add_argument(
        "--task",
        action="append",
        default=[],
        help="Run only this task id; repeatable",
    )
    cache = parser.add_mutually_exclusive_group()
    cache.add_argument(
        "--prompt-cache",
        dest="prompt_cache",
        action="store_true",
        help="Enable prompt caching where the harness exposes a control (default)",
    )
    cache.add_argument(
        "--no-prompt-cache",
        dest="prompt_cache",
        action="store_false",
        help="Disable prompt caching where the harness exposes a control",
    )
    parser.set_defaults(prompt_cache=True)
    parser.add_argument("--orca-bin", default="target/release/orcacode")
    parser.add_argument("--pi-bin", default="pi")
    parser.add_argument("--omp-bin", default="omp")
    parser.add_argument("--claude-bin", default="claude")
    parser.add_argument("--tasks", type=Path, default=HERE / "tasks.json")
    parser.add_argument("--fixture", type=Path, default=HERE / "fixture")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--input-usd-per-million", type=float, default=1.00)
    parser.add_argument("--output-usd-per-million", type=float, default=5.00)
    parser.add_argument(
        "--cache-read-usd-per-million", type=float, default=0.10
    )
    parser.add_argument(
        "--cache-write-usd-per-million", type=float, default=1.25
    )
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("--repetitions must be at least 1")
    if (
        args.timeout_seconds <= 0
        or args.max_steps < 1
        or args.max_output_tokens < 1
    ):
        parser.error(
            "timeout, max steps, and max output tokens must be positive"
        )
    return args


def load_tasks(
    path: Path, profile: str, selected_ids: set[str]
) -> list[dict[str, Any]]:
    data = json.loads(path.read_text(encoding="utf-8"))
    tasks = data.get("tasks")
    if not isinstance(tasks, list) or not tasks:
        raise ValueError(f"{path} must contain a non-empty tasks array")
    seen: set[str] = set()
    selected: list[dict[str, Any]] = []
    for task in tasks:
        task_id = task.get("id")
        mode = task.get("mode")
        if not isinstance(task_id, str) or not task_id or task_id in seen:
            raise ValueError(
                f"task ids must be unique non-empty strings: {task_id!r}"
            )
        seen.add(task_id)
        if mode not in {"read", "edit"}:
            raise ValueError(f"task {task_id}: mode must be read or edit")
        for field in ("category", "prompt", "expected"):
            if not isinstance(task.get(field), str) or not task[field]:
                raise ValueError(
                    f"task {task_id}: {field} must be a non-empty string"
                )
        if profile != "all" and mode != profile:
            continue
        if selected_ids and task_id not in selected_ids:
            continue
        selected.append(task)
    unknown = selected_ids - seen
    if unknown:
        raise ValueError(f"unknown task ids: {', '.join(sorted(unknown))}")
    if not selected:
        raise ValueError("task filters selected no work")
    return selected


def resolve_binary(value: str) -> str:
    candidate = Path(value)
    if candidate.parent != Path("."):
        resolved = candidate.resolve()
        if not resolved.is_file() or not os.access(resolved, os.X_OK):
            raise FileNotFoundError(f"executable not found: {value}")
        return str(resolved)
    found = shutil.which(value)
    if found is None:
        raise FileNotFoundError(f"executable not found on PATH: {value}")
    return found


def harnesses(choice: str) -> list[str]:
    if choice == "all":
        return ["orca", "pi", "omp", "claude"]
    if choice == "both":
        return ["orca", "pi"]
    return [choice]


def balanced_order(
    selected: list[str], repetition: int, task_index: int
) -> list[str]:
    if len(selected) < 2:
        return selected
    offset = (repetition + task_index) % len(selected)
    return selected[offset:] + selected[:offset]


def task_prompt(task: dict[str, Any]) -> str:
    if task["mode"] == "read":
        permission = "Do not edit, write, create, delete, or rename any file."
    else:
        allowed = ", ".join(task.get("allowed_changes", []))
        permission = f"You may edit only these files: {allowed}. Do not change any other file."
    return f"{task['prompt']} {permission} {SAFETY_SUFFIX}"


def build_command(
    name: str,
    binary: str,
    task: dict[str, Any],
    workspace: Path,
    args: argparse.Namespace,
    omp_config: Path | None = None,
) -> list[str]:
    prompt = task_prompt(task)
    if name == "orca":
        tools = ["read_file", "grep", "glob", "list_dir"]
        if task["mode"] == "edit":
            tools.extend(("edit_file", "write_file"))
        command = [
            binary,
            "--openrouter",
            "--model",
            args.model,
            "--workspace",
            str(workspace),
            "--no-session",
            "--json",
            "--bare",
            "--tools",
            ",".join(tools),
            "--effort",
            args.effort,
            "--max-output-tokens",
            str(args.max_output_tokens),
            "--max-steps",
            str(args.max_steps),
        ]
        command.append(
            "--prompt-cache" if args.prompt_cache else "--no-prompt-cache"
        )
        if task["mode"] == "edit":
            command.append("--auto-approve")
        return [*command, "-p", prompt]

    if name == "pi":
        tools = ["read", "grep", "find", "ls"]
        if task["mode"] == "edit":
            tools.extend(("edit", "write"))
        return [
            binary,
            "--provider",
            "openrouter",
            "--model",
            args.model,
            "--thinking",
            args.effort,
            "--mode",
            "json",
            "--print",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-approve",
            "--tools",
            ",".join(tools),
            prompt,
        ]

    if name == "omp":
        if omp_config is None:
            raise ValueError("Oh My Pi requires an isolated discovery config")
        tools = ["read", "grep", "glob"]
        if task["mode"] == "edit":
            tools.extend(("edit", "write"))
        command = [
            binary,
            "--provider",
            "openrouter",
            "--model",
            args.model,
            "--thinking",
            args.effort,
            "--mode",
            "json",
            "--config",
            str(omp_config),
            "--print",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-rules",
            "--no-title",
            "--no-lsp",
            "--no-pty",
            "--tools",
            ",".join(tools),
        ]
        if task["mode"] == "edit":
            command.append("--auto-approve")
        return [*command, prompt]

    tools = ["Read", "Grep", "Glob"]
    if task["mode"] == "edit":
        tools.extend(("Edit", "Write"))
    return [
        binary,
        "--print",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--bare",
        "--restricted",
        "--strict-mcp-config",
        "--mcp-config",
        '{"mcpServers":{}}',
        "--disable-slash-commands",
        "--no-chrome",
        "--no-session-persistence",
        "--permission-mode",
        "acceptEdits" if task["mode"] == "edit" else "dontAsk",
        "--model",
        args.model,
        "--effort",
        args.effort,
        "--max-turns",
        str(args.max_steps),
        f"--tools={','.join(tools)}",
        prompt,
    ]


def prepare_environment(
    name: str,
    temp_root: Path,
    args: argparse.Namespace,
    omp_runtime_home: Path | None = None,
) -> dict[str, str]:
    environment = os.environ.copy()
    state_name = "omp-state" if name == "omp" else "pi-state"
    environment["PI_CODING_AGENT_DIR"] = str(temp_root / state_name)
    if name in {"pi", "omp"}:
        environment["PI_CACHE_RETENTION"] = (
            "short" if args.prompt_cache else "none"
        )
    if name == "omp":
        environment["HOME"] = str(omp_runtime_home or temp_root / "omp-home")
        Path(environment["HOME"]).mkdir(parents=True, exist_ok=True)
        return environment
    if name != "claude":
        return environment

    environment["HOME"] = str(temp_root / "home")
    environment["CLAUDE_CONFIG_DIR"] = str(temp_root / "claude-state")
    Path(environment["HOME"]).mkdir(parents=True, exist_ok=True)
    Path(environment["CLAUDE_CONFIG_DIR"]).mkdir(parents=True, exist_ok=True)
    environment["ANTHROPIC_BASE_URL"] = "https://openrouter.ai/api"
    environment["ANTHROPIC_AUTH_TOKEN"] = environment["OPENROUTER_API_KEY"]
    environment["ANTHROPIC_API_KEY"] = ""
    environment.pop("CLAUDE_CODE_OAUTH_TOKEN", None)
    environment["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"] = "1"
    environment["CLAUDE_CODE_MAX_OUTPUT_TOKENS"] = str(args.max_output_tokens)
    if args.prompt_cache:
        environment.pop("DISABLE_PROMPT_CACHING", None)
    else:
        environment["DISABLE_PROMPT_CACHING"] = "1"
    return environment


def write_omp_config(root: Path) -> Path:
    path = root / "omp-config.yml"
    providers = "".join(f"  - {name}\n" for name in OMP_DISCOVERY_PROVIDERS)
    path.write_text(f"disabledProviders:\n{providers}", encoding="utf-8")
    return path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def snapshot(root: Path) -> dict[str, str]:
    return {
        path.relative_to(root).as_posix(): sha256(path)
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def validate_workspace(
    task: dict[str, Any], root: Path, before: dict[str, str]
) -> dict[str, Any]:
    after = snapshot(root)
    changed = sorted(
        path
        for path in before.keys() | after.keys()
        if before.get(path) != after.get(path)
    )
    allowed = sorted(task.get("allowed_changes", []))
    errors: list[str] = []
    if changed != allowed:
        errors.append(f"changed paths {changed!r}, expected {allowed!r}")
    for check in task.get("checks", []):
        relative = check["path"]
        target = root / relative
        if not target.is_file():
            errors.append(f"missing checked file: {relative}")
            continue
        content = target.read_text(encoding="utf-8")
        for expected in check.get("contains", []):
            if expected not in content:
                errors.append(f"{relative} does not contain {expected!r}")
        for forbidden in check.get("not_contains", []):
            if forbidden in content:
                errors.append(f"{relative} still contains {forbidden!r}")
    return {"passed": not errors, "changed_paths": changed, "errors": errors}


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def pump(
    stream: Any, name: str, events: queue.Queue[tuple[str, str | None]]
) -> None:
    try:
        for line in stream:
            events.put((name, line.rstrip("\n")))
    finally:
        events.put((name, None))


def run_process(
    command: list[str],
    workspace: Path,
    environment: dict[str, str],
    timeout_seconds: float,
    raw_path: Path,
    metadata: dict[str, Any],
) -> dict[str, Any]:
    started_ns = time.perf_counter_ns()
    process = subprocess.Popen(
        command,
        cwd=workspace,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        bufsize=1,
    )
    assert process.stdout is not None and process.stderr is not None
    events: queue.Queue[tuple[str, str | None]] = queue.Queue()
    threads = [
        threading.Thread(
            target=pump, args=(process.stdout, "stdout", events), daemon=True
        ),
        threading.Thread(
            target=pump, args=(process.stderr, "stderr", events), daemon=True
        ),
    ]
    for thread in threads:
        thread.start()

    streams_closed = 0
    timed_out = False
    deadline = time.monotonic() + timeout_seconds
    with raw_path.open("w", encoding="utf-8") as raw:
        raw.write(
            json.dumps(
                {"record": "run_start", "timestamp": utc_now(), **metadata}
            )
            + "\n"
        )
        while streams_closed < 2 or process.poll() is None:
            if process.poll() is None and time.monotonic() >= deadline:
                timed_out = True
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
            try:
                stream_name, line = events.get(timeout=0.05)
            except queue.Empty:
                continue
            if line is None:
                streams_closed += 1
                continue
            elapsed_ms = (time.perf_counter_ns() - started_ns) / 1_000_000
            raw.write(
                json.dumps(
                    {
                        "record": "line",
                        "stream": stream_name,
                        "elapsed_ms": round(elapsed_ms, 3),
                        "text": line,
                    },
                    ensure_ascii=False,
                )
                + "\n"
            )
            raw.flush()
        exit_code = process.wait()
        wall_ms = (time.perf_counter_ns() - started_ns) / 1_000_000
        result = {
            "record": "run_end",
            "timestamp": utc_now(),
            "exit_code": exit_code,
            "timed_out": timed_out,
            "wall_ms": round(wall_ms, 3),
        }
        raw.write(json.dumps(result) + "\n")
    return result


def command_version(
    binary: str, environment: dict[str, str] | None = None
) -> str:
    try:
        result = subprocess.run(
            [binary, "--version"],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
            env=environment,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"unavailable: {error}"
    value = (result.stdout or result.stderr).strip().splitlines()[0]
    return "unavailable" if value.startswith("unknown flag:") else value


def prepare_omp_runtime(binary: str, home: Path) -> None:
    environment = os.environ.copy()
    environment["HOME"] = str(home)
    environment["PI_CODING_AGENT_DIR"] = str(home / "agent-state")
    home.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(
        [binary, "config", "--help"],
        capture_output=True,
        text=True,
        timeout=20,
        check=False,
        env=environment,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip().splitlines()
        raise RuntimeError(
            "Oh My Pi runtime preparation failed: "
            + (detail[-1] if detail else f"exit {result.returncode}")
        )


def source_identity() -> dict[str, Any]:
    try:
        revision = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=HERE,
            capture_output=True,
            text=True,
            timeout=5,
            check=True,
        ).stdout.strip()
        dirty = bool(
            subprocess.run(
                ["git", "status", "--porcelain", "--untracked-files=normal"],
                cwd=HERE,
                capture_output=True,
                text=True,
                timeout=5,
                check=True,
            ).stdout.strip()
        )
        return {"revision": revision, "dirty": dirty}
    except (OSError, subprocess.SubprocessError):
        return {"revision": "unavailable", "dirty": None}


def main() -> int:
    args = parse_args()
    tasks = load_tasks(args.tasks, args.profile, set(args.task))
    selected_harnesses = harnesses(args.harness)
    planned = [
        {
            "repetition": repetition + 1,
            "task": task["id"],
            "mode": task["mode"],
            "order": balanced_order(selected_harnesses, repetition, index),
        }
        for repetition in range(args.repetitions)
        for index, task in enumerate(tasks)
    ]
    if args.dry_run:
        print(
            json.dumps(
                {
                    "model": args.model,
                    "effort": args.effort,
                    "prompt_cache": args.prompt_cache,
                    "provider_route": "openrouter default; backend endpoint not pinned",
                    "run_count": len(tasks)
                    * args.repetitions
                    * len(selected_harnesses),
                    "plan": planned,
                },
                indent=2,
            )
        )
        return 0

    if not args.fixture.is_dir():
        raise FileNotFoundError(f"fixture directory not found: {args.fixture}")
    if not os.environ.get("OPENROUTER_API_KEY"):
        raise RuntimeError(
            "OPENROUTER_API_KEY is required but must not be passed on the command line"
        )
    binary_args = {
        "orca": args.orca_bin,
        "pi": args.pi_bin,
        "omp": args.omp_bin,
        "claude": args.claude_bin,
    }
    binaries = {
        name: resolve_binary(binary_args[name]) for name in selected_harnesses
    }
    runtime_state = tempfile.TemporaryDirectory(prefix="orca-harness-runtime-")
    runtime_root = Path(runtime_state.name)
    omp_runtime_home = runtime_root / "omp-home"
    if "omp" in selected_harnesses:
        prepare_omp_runtime(binaries["omp"], omp_runtime_home)
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = (
        args.output
        or HERE.parent / "results" / "harness-comparison" / timestamp
    ).resolve()
    output.mkdir(parents=True, exist_ok=False)
    pricing = {
        "input_usd_per_million": args.input_usd_per_million,
        "output_usd_per_million": args.output_usd_per_million,
        "cache_read_usd_per_million": args.cache_read_usd_per_million,
        "cache_write_usd_per_million": args.cache_write_usd_per_million,
    }
    version_environment = os.environ.copy()
    with tempfile.TemporaryDirectory(
        prefix="orca-harness-version-"
    ) as version_state:
        version_environment["PI_CODING_AGENT_DIR"] = version_state
        version_records = {
            name: {
                "binary": binaries[name],
                "version": command_version(binaries[name], version_environment),
                "sha256": sha256(Path(binaries[name])),
            }
            for name in selected_harnesses
        }
    manifest = {
        "schema_version": 4,
        "created_at": utc_now(),
        "model": args.model,
        "provider": "openrouter",
        "provider_routing": {
            "gateway": "openrouter",
            "backend_endpoint_pinned": False,
            "backend_endpoint_observed": False,
            "scope": "complete gateway-routed harness latency",
        },
        "effort": args.effort,
        "prompt_cache": args.prompt_cache,
        "repetitions": args.repetitions,
        "timeout_seconds": args.timeout_seconds,
        "max_steps": args.max_steps,
        "max_output_tokens": args.max_output_tokens,
        "pricing": pricing,
        "source": source_identity(),
        "harnesses": version_records,
        "tasks": tasks,
        "plan": planned,
    }
    (output / "manifest.json").write_text(
        json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
    )

    run_number = 0
    for item in planned:
        task = next(task for task in tasks if task["id"] == item["task"])
        for name in item["order"]:
            run_number += 1
            with tempfile.TemporaryDirectory(
                prefix=f"orca-harness-{task['id']}-"
            ) as temp:
                temp_root = Path(temp)
                workspace = temp_root / "workspace"
                shutil.copytree(args.fixture, workspace)
                before = snapshot(workspace)
                environment = prepare_environment(
                    name, temp_root, args, omp_runtime_home=omp_runtime_home
                )
                omp_config = (
                    write_omp_config(temp_root) if name == "omp" else None
                )
                command = build_command(
                    name,
                    binaries[name],
                    task,
                    workspace,
                    args,
                    omp_config=omp_config,
                )
                raw_path = output / (
                    f"rep-{item['repetition']:02d}-{task['id']}-{name}.jsonl"
                )
                metadata = {
                    "run_number": run_number,
                    "repetition": item["repetition"],
                    "task": task["id"],
                    "category": task["category"],
                    "mode": task["mode"],
                    "harness": name,
                    "command": [*command[:-1], "<benchmark-prompt>"],
                }
                result = run_process(
                    command,
                    workspace,
                    environment,
                    float(task.get("timeout_seconds", args.timeout_seconds)),
                    raw_path,
                    metadata,
                )
                validation = validate_workspace(task, workspace, before)
                with raw_path.open("a", encoding="utf-8") as raw:
                    raw.write(
                        json.dumps(
                            {"record": "workspace_validation", **validation}
                        )
                        + "\n"
                    )
                status = (
                    "timeout"
                    if result["timed_out"]
                    else f"exit={result['exit_code']}"
                )
                print(
                    f"[{run_number}/{len(tasks) * args.repetitions * len(selected_harnesses)}] "
                    f"rep={item['repetition']} task={task['id']} harness={name} {status}",
                    flush=True,
                )

    print(f"Raw benchmark complete: {output}")
    print(
        "Analyze with: "
        f"{shlex.quote(sys.executable)} {shlex.quote(str(HERE / 'analyze.py'))} "
        f"{shlex.quote(str(output))}"
    )
    runtime_state.cleanup()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (FileNotFoundError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
