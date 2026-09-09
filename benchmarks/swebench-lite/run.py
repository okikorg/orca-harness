#!/usr/bin/env python3
"""Generate Orcacode patches for a small SWE-bench Lite development smoke run."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path


DATASET = "SWE-bench/SWE-bench_Lite"
SPLIT = "dev"
DEFAULT_COUNT = 3
DEFAULT_MODEL = "anthropic/claude-haiku-4.5"
DEFAULT_MAX_STEPS = 64
DEFAULT_RESULTS = Path(__file__).resolve().parents[1] / "results" / "swebench-lite"
DATASET_ROWS_URL = "https://datasets-server.huggingface.co/rows"
TOOLS = "read_file,list_dir,grep,glob,shell,edit_file,write_file"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--count", type=int, default=DEFAULT_COUNT)
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--orcacode", help="Path to the Orcacode binary")
    parser.add_argument("--output", type=Path, help="Result directory")
    parser.add_argument("--max-steps", type=int, default=DEFAULT_MAX_STEPS)
    parser.add_argument("--max-output-tokens", type=int, default=8192)
    parser.add_argument("--timeout", type=int, default=900, help="Seconds per instance")
    parser.add_argument("--dry-run", action="store_true")
    return parser.parse_args()


def fetch_instances(count: int) -> list[dict]:
    if count < 1:
        raise ValueError("count must be positive")
    query = urllib.parse.urlencode(
        {
            "dataset": DATASET,
            "config": "default",
            "split": SPLIT,
            "offset": 0,
            "length": count,
        }
    )
    url = f"{DATASET_ROWS_URL}?{query}"
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            payload = json.load(response)
    except OSError as urllib_error:
        # Some macOS Python installations do not have a usable CA bundle even
        # though the system curl does. Keep certificate verification enabled by
        # falling back to curl rather than creating an unverified SSL context.
        curl = shutil.which("curl")
        if not curl:
            raise urllib_error
        response = subprocess.run(
            [curl, "--fail", "--location", "--silent", "--show-error", url],
            text=True,
            capture_output=True,
            check=True,
        )
        payload = json.loads(response.stdout)
    rows = [entry["row"] for entry in payload["rows"]]
    if len(rows) != count:
        raise RuntimeError(f"requested {count} instances but received {len(rows)}")
    return rows


def select_instances(instances: list[dict], count: int) -> list[dict]:
    """Select deterministically so repeated smoke runs use identical tasks."""
    return instances[:count]


def resolve_orcacode(explicit: str | None) -> str:
    candidates = [explicit, "target/release/orcacode", shutil.which("orcacode")]
    for candidate in candidates:
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return str(Path(candidate).resolve())
    raise FileNotFoundError("orcacode not found; build it or pass --orcacode")


def build_prompt(instance: dict) -> str:
    return f"""You are solving SWE-bench instance {instance['instance_id']}.

{instance['problem_statement'].strip()}

Work directly in the checked-out repository. Diagnose the issue, make the smallest
correct production-code change, and run focused tests when practical. Do not modify
tests, create scratch/debug files, alter benchmark metadata, or change git history.
Keep the investigation focused: use no more than 30 tool calls, remove any temporary
artifacts, and reserve time to inspect the final diff. Finish only after the working
tree contains the proposed fix. Your final response should briefly state the change
and tests run.
"""


def build_command(binary: str, workspace: Path, instance: dict, args: argparse.Namespace) -> list[str]:
    return [
        binary,
        "--openrouter",
        "--model",
        args.model,
        "--effort",
        "low",
        "--workspace",
        str(workspace),
        "--max-steps",
        str(args.max_steps),
        "--max-output-tokens",
        str(args.max_output_tokens),
        "--bare",
        "--tools",
        TOOLS,
        "--yolo",
        "--no-session",
        "--json",
        "--prompt",
        build_prompt(instance),
    ]


def run_checked(command: list[str], cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, cwd=cwd, text=True, capture_output=True, check=True)


def prepare_checkout(instance: dict, workspace: Path) -> None:
    run_checked(
        [
            "git",
            "clone",
            "--quiet",
            f"https://github.com/{instance['repo']}.git",
            str(workspace),
        ]
    )
    run_checked(["git", "checkout", "--quiet", instance["base_commit"]], cwd=workspace)


def capture_patch(workspace: Path) -> str:
    return run_checked(["git", "diff", "--binary", "HEAD", "--"], cwd=workspace).stdout


def decoded_output(value: str | bytes | None) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode(errors="replace")
    return value


def parse_summary(stdout: str) -> dict:
    for line in reversed(stdout.splitlines()):
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "summary":
            return {
                "model_steps": event.get("modelSteps"),
                "tool_calls": event.get("toolCalls"),
                "usage": event.get("usage", {}),
            }
    return {}


def prediction(instance: dict, model: str, patch: str) -> dict:
    return {
        "instance_id": instance["instance_id"],
        "model_name_or_path": f"orcacode+{model}",
        "model_patch": patch,
    }


def output_directory(requested: Path | None) -> Path:
    if requested:
        return requested
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_RESULTS / stamp


def main() -> int:
    args = parse_args()
    try:
        instances = select_instances(fetch_instances(args.count), args.count)
        binary = resolve_orcacode(args.orcacode)
    except (OSError, ValueError, RuntimeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    if args.dry_run:
        for instance in instances:
            command = build_command(binary, Path("<checkout>"), instance, args)
            print(f"{instance['instance_id']}: {command[:-1]} <problem-prompt>")
        return 0

    output = output_directory(args.output)
    output.mkdir(parents=True, exist_ok=False)
    predictions: list[dict] = []
    runs: list[dict] = []

    for index, instance in enumerate(instances, 1):
        instance_id = instance["instance_id"]
        print(f"[{index}/{len(instances)}] {instance_id}", flush=True)
        with tempfile.TemporaryDirectory(prefix=f"swebench-{instance_id}-") as temporary:
            workspace = Path(temporary) / "repo"
            started = time.monotonic()
            status = "completed"
            returncode: int | None = None
            stdout = ""
            stderr = ""
            patch_text = ""
            try:
                prepare_checkout(instance, workspace)
                completed = subprocess.run(
                    build_command(binary, workspace, instance, args),
                    text=True,
                    capture_output=True,
                    timeout=args.timeout,
                )
                returncode = completed.returncode
                stdout, stderr = completed.stdout, completed.stderr
                status = "completed" if returncode == 0 else "nonzero_exit"
                patch_text = capture_patch(workspace)
            except subprocess.TimeoutExpired as error:
                status = "timeout"
                stdout = decoded_output(error.stdout)
                stderr = decoded_output(error.stderr)
                if workspace.exists():
                    patch_text = capture_patch(workspace)
            except subprocess.CalledProcessError as error:
                status = "setup_error"
                returncode = error.returncode
                stdout, stderr = error.stdout or "", error.stderr or ""

            duration = time.monotonic() - started
            (output / f"{instance_id}.stdout.jsonl").write_text(stdout, encoding="utf-8")
            (output / f"{instance_id}.stderr.txt").write_text(stderr, encoding="utf-8")
            (output / f"{instance_id}.patch").write_text(patch_text, encoding="utf-8")
            predictions.append(prediction(instance, args.model, patch_text))
            runs.append(
                {
                    "instance_id": instance_id,
                    "repo": instance["repo"],
                    "base_commit": instance["base_commit"],
                    "status": status,
                    "returncode": returncode,
                    "duration_seconds": round(duration, 3),
                    "patch_bytes": len(patch_text.encode()),
                    **parse_summary(stdout),
                }
            )

    predictions_path = output / "predictions.jsonl"
    predictions_path.write_text(
        "".join(json.dumps(item, sort_keys=True) + "\n" for item in predictions),
        encoding="utf-8",
    )
    manifest = {
        "dataset": DATASET,
        "split": SPLIT,
        "model": args.model,
        "harness": "orcacode",
        "runs": runs,
    }
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Predictions: {predictions_path}")
    return 0 if all(run["status"] == "completed" and run["patch_bytes"] for run in runs) else 1


if __name__ == "__main__":
    raise SystemExit(main())
