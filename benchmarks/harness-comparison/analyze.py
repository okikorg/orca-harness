#!/usr/bin/env python3
"""Analyze timestamped raw streams from the harness-comparison runner."""

from __future__ import annotations

import argparse
import csv
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable


HARNESS_ORDER = ("orca", "pi", "omp", "claude", "kiss")
HARNESS_NAMES = {
    "orca": "Orcacode",
    "pi": "Pi",
    "omp": "Oh My Pi",
    "kiss": "KISS",
    "claude": "Claude Code",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Analyze a harness benchmark run directory")
    parser.add_argument("run_dir", type=Path)
    return parser.parse_args()


def percentile(values: Iterable[float], percentile_value: float) -> float | None:
    ordered = sorted(values)
    if not ordered:
        return None
    index = max(0, math.ceil(percentile_value * len(ordered)) - 1)
    return ordered[index]


def median(values: Iterable[float | int | None]) -> float | None:
    present = [float(value) for value in values if value is not None]
    return statistics.median(present) if present else None


def sum_optional(values: Iterable[float | int | None]) -> float | None:
    present = [float(value) for value in values if value is not None]
    return sum(present) if present else None


def difference(left: float | None, right: float | None) -> float | None:
    return left - right if left is not None and right is not None else None


def milliseconds(value: float | None) -> str:
    return f"{value:.1f} ms" if value is not None else "n/a"


def last_nonempty_line(value: str) -> str:
    return next((line.strip() for line in reversed(value.splitlines()) if line.strip()), "")


def content_text(message: dict[str, Any]) -> str:
    content = message.get("content", [])
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    return "".join(
        item.get("text", "")
        for item in content
        if isinstance(item, dict) and item.get("type") == "text"
    )


def embedded_events(records: list[dict[str, Any]]) -> list[tuple[float, dict[str, Any]]]:
    events: list[tuple[float, dict[str, Any]]] = []
    for record in records:
        if record.get("record") != "line" or record.get("stream") != "stdout":
            continue
        try:
            event = json.loads(record.get("text", ""))
        except (json.JSONDecodeError, TypeError):
            continue
        if isinstance(event, dict):
            events.append((float(record.get("elapsed_ms", 0)), event))
    return events


def claude_stream_event(event: dict[str, Any]) -> dict[str, Any]:
    nested = event.get("event")
    return nested if event.get("type") == "stream_event" and isinstance(nested, dict) else {}


def tool_timings(
    harness: str, events: list[tuple[float, dict[str, Any]]]
) -> tuple[float | None, float | None, float | None, float | None]:
    if harness == "claude":
        # Claude streams model-side tool_use and a later tool_result, but no
        # execution start/end boundary equivalent to Orcacode or Pi.
        return None, None, None, None
    starts: dict[str, float] = {}
    durations: list[float] = []
    first_start: float | None = None
    first_end: float | None = None
    for elapsed, event in events:
        if harness == "orca":
            if event.get("type") == "tool_call":
                tool_id = event.get("tool_call_id")
                if isinstance(tool_id, str):
                    starts[tool_id] = elapsed
                    first_start = elapsed if first_start is None else first_start
            elif event.get("type") == "tool_finished":
                tool_id = event.get("tool_call_id")
                if isinstance(tool_id, str) and tool_id in starts:
                    durations.append(max(0.0, elapsed - starts.pop(tool_id)))
                    first_end = elapsed if first_end is None else first_end
        elif harness in {"pi", "omp", "kiss"}:
            if event.get("type") == "tool_execution_start":
                tool_id = event.get("toolCallId")
                if isinstance(tool_id, str):
                    starts[tool_id] = elapsed
                    first_start = elapsed if first_start is None else first_start
            elif event.get("type") == "tool_execution_end":
                tool_id = event.get("toolCallId")
                if isinstance(tool_id, str) and tool_id in starts:
                    durations.append(max(0.0, elapsed - starts.pop(tool_id)))
                    first_end = elapsed if first_end is None else first_end
    return first_start, first_end, sum(durations), max(durations, default=0.0)


def first_delta_times(
    harness: str, events: list[tuple[float, dict[str, Any]]]
) -> tuple[float | None, float | None, float | None]:
    agent_start: float | None = None
    first_model: float | None = None
    first_answer: float | None = None
    for elapsed, event in events:
        if harness == "orca":
            event_type = event.get("type")
            if event_type == "agent_start":
                agent_start = elapsed if agent_start is None else agent_start
            if event_type in {"reasoning_delta", "assistant_delta", "tool_input_delta"}:
                first_model = elapsed if first_model is None else first_model
            if event_type == "assistant_delta":
                first_answer = elapsed if first_answer is None else first_answer
        elif harness in {"pi", "omp", "kiss"}:
            if event.get("type") == "turn_start":
                agent_start = elapsed if agent_start is None else agent_start
            if event.get("type") == "message_update":
                update = event.get("assistantMessageEvent", {})
                update_type = update.get("type") if isinstance(update, dict) else None
                if update_type in {"thinking_delta", "text_delta", "toolcall_delta"}:
                    first_model = elapsed if first_model is None else first_model
                if update_type == "text_delta":
                    first_answer = elapsed if first_answer is None else first_answer
        else:
            if event.get("type") == "system" and event.get("subtype") == "init":
                agent_start = elapsed if agent_start is None else agent_start
            nested = claude_stream_event(event)
            if nested.get("type") != "content_block_delta":
                continue
            delta = nested.get("delta", {})
            delta_type = delta.get("type") if isinstance(delta, dict) else None
            if delta_type in {"thinking_delta", "text_delta", "input_json_delta"}:
                first_model = elapsed if first_model is None else first_model
            if delta_type == "text_delta":
                first_answer = elapsed if first_answer is None else first_answer
    return agent_start, first_model, first_answer


def orca_metrics(events: list[tuple[float, dict[str, Any]]]) -> dict[str, Any]:
    summary = next(
        (event for _, event in reversed(events) if event.get("type") == "summary"), {}
    )
    usage = summary.get("usage", {}) if isinstance(summary.get("usage"), dict) else {}
    answer = next(
        (
            str(event.get("message", ""))
            for _, event in reversed(events)
            if event.get("type") == "result"
        ),
        "",
    )
    return {
        "answer": answer,
        "input_tokens": int(usage.get("inputTokens", 0)),
        "output_tokens": int(usage.get("outputTokens", 0)),
        "cache_read_tokens": int(usage.get("cacheReadTokens", 0)),
        "cache_write_tokens": int(usage.get("cacheCreateTokens", 0)),
        "tool_calls": int(summary.get("toolCalls", 0)),
        "turns": int(summary.get("modelSteps", 0)),
        "model_retries": int(summary.get("modelRetries", 0)),
        "reported_provider": summary.get("provider"),
    }


def pi_metrics(events: list[tuple[float, dict[str, Any]]]) -> dict[str, Any]:
    usage = {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
    turns = 0
    answer = ""
    provider = None
    for _, event in events:
        if event.get("type") != "message_end":
            continue
        message = event.get("message", {})
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        provider = message.get("provider") or provider
        message_usage = message.get("usage", {})
        if isinstance(message_usage, dict):
            for key in usage:
                usage[key] += int(message_usage.get(key, 0) or 0)
            turns += 1
        text = content_text(message)
        if text:
            answer = text
    return {
        "answer": answer,
        "input_tokens": usage["input"],
        "output_tokens": usage["output"],
        "cache_read_tokens": usage["cacheRead"],
        "cache_write_tokens": usage["cacheWrite"],
        "tool_calls": sum(1 for _, event in events if event.get("type") == "tool_execution_start"),
        "turns": turns,
        "model_retries": None,
        "reported_provider": provider,
    }


def claude_metrics(events: list[tuple[float, dict[str, Any]]]) -> dict[str, Any]:
    result = next(
        (event for _, event in reversed(events) if event.get("type") == "result"), {}
    )
    result_usage = result.get("usage", {}) if isinstance(result.get("usage"), dict) else {}
    fallback_usage = {
        "input_tokens": 0,
        "output_tokens": 0,
        "cache_read_input_tokens": 0,
        "cache_creation_input_tokens": 0,
    }
    fallback_turns = 0
    fallback_answer = ""
    provider = None
    tool_ids: set[str] = set()
    for _, event in events:
        if event.get("type") == "assistant":
            message = event.get("message", {})
            if isinstance(message, dict):
                provider = message.get("provider") or provider
            items = message.get("content", []) if isinstance(message, dict) else []
            text = content_text(message) if isinstance(message, dict) else ""
            if text:
                fallback_answer = text
            if isinstance(items, list):
                tool_ids.update(
                    item["id"]
                    for item in items
                    if isinstance(item, dict)
                    and item.get("type") == "tool_use"
                    and isinstance(item.get("id"), str)
                )
        nested = claude_stream_event(event)
        nested_usage = nested.get("usage", {})
        if nested.get("type") == "message_delta" and isinstance(nested_usage, dict):
            for key in fallback_usage:
                fallback_usage[key] += int(nested_usage.get(key, 0) or 0)
            fallback_turns += 1
        content = nested.get("content_block", {})
        if (
            nested.get("type") == "content_block_start"
            and isinstance(content, dict)
            and content.get("type") == "tool_use"
            and isinstance(content.get("id"), str)
        ):
            tool_ids.add(content["id"])
    usage = result_usage or fallback_usage
    return {
        "answer": str(result.get("result", "")) or fallback_answer,
        "input_tokens": int(usage.get("input_tokens", 0) or 0),
        "output_tokens": int(usage.get("output_tokens", 0) or 0),
        "cache_read_tokens": int(usage.get("cache_read_input_tokens", 0) or 0),
        "cache_write_tokens": int(usage.get("cache_creation_input_tokens", 0) or 0),
        "tool_calls": len(tool_ids),
        "turns": int(result.get("num_turns", 0) or 0) if result else fallback_turns,
        "model_retries": None,
        "reported_provider": provider,
    }


def cost_breakdown(metrics: dict[str, Any], pricing: dict[str, float]) -> dict[str, float]:
    costs = {
        "input_cost_usd": (
            metrics["input_tokens"] * pricing["input_usd_per_million"] / 1_000_000
        ),
        "output_cost_usd": (
            metrics["output_tokens"] * pricing["output_usd_per_million"] / 1_000_000
        ),
        "cache_read_cost_usd": (
            metrics["cache_read_tokens"]
            * pricing["cache_read_usd_per_million"]
            / 1_000_000
        ),
        "cache_write_cost_usd": (
            metrics["cache_write_tokens"]
            * pricing["cache_write_usd_per_million"]
            / 1_000_000
        ),
    }
    costs["normalized_cost_usd"] = sum(costs.values())
    return costs


def parse_raw(path: Path, manifest: dict[str, Any]) -> dict[str, Any]:
    records = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
    start = next(record for record in records if record.get("record") == "run_start")
    end = next(record for record in records if record.get("record") == "run_end")
    validation = next(
        (record for record in records if record.get("record") == "workspace_validation"),
        {"passed": False, "errors": ["missing workspace validation"]},
    )
    task = next(task for task in manifest["tasks"] if task["id"] == start["task"])
    events = embedded_events(records)
    harness = start["harness"]
    parsers = {
        "orca": orca_metrics,
        "pi": pi_metrics,
        "omp": pi_metrics,
        "kiss": pi_metrics,
        "claude": claude_metrics,
    }
    metrics = parsers[harness](events)
    agent_start, first_model, first_answer = first_delta_times(harness, events)
    first_tool, first_tool_result, tool_sum, tool_max = tool_timings(harness, events)
    total_prompt = (
        metrics["input_tokens"]
        + metrics["cache_read_tokens"]
        + metrics["cache_write_tokens"]
    )
    total_tokens = total_prompt + metrics["output_tokens"]
    answer = last_nonempty_line(metrics.pop("answer"))
    answer_correct = answer == task["expected"]
    workspace_correct = bool(validation.get("passed"))
    success = (
        not end["timed_out"]
        and end["exit_code"] == 0
        and answer_correct
        and workspace_correct
    )
    return {
        "source": path.name,
        "repetition": start["repetition"],
        "task": start["task"],
        "category": start["category"],
        "mode": start["mode"],
        "harness": harness,
        "expected": task["expected"],
        "answer": answer,
        "answer_correct": answer_correct,
        "workspace_correct": workspace_correct,
        "success": success,
        "exit_code": end["exit_code"],
        "timed_out": end["timed_out"],
        "wall_ms": end["wall_ms"],
        "agent_start_ms": agent_start,
        "ttft_ms": first_model,
        "model_ttft_ms": (
            max(0.0, first_model - agent_start)
            if first_model is not None and agent_start is not None
            else None
        ),
        "time_to_answer_ms": first_answer,
        "time_to_first_tool_ms": first_tool,
        "time_to_first_tool_result_ms": first_tool_result,
        "tool_duration_sum_ms": round(tool_sum, 3) if tool_sum is not None else None,
        "tool_duration_max_ms": round(tool_max, 3) if tool_max is not None else None,
        **metrics,
        "total_prompt_tokens": total_prompt,
        "total_tokens": total_tokens,
        "cache_read_share": metrics["cache_read_tokens"] / total_prompt if total_prompt else 0.0,
        "output_tokens_per_turn": (
            metrics["output_tokens"] / metrics["turns"] if metrics["turns"] else 0.0
        ),
        **cost_breakdown(metrics, manifest["pricing"]),
        "changed_paths": validation.get("changed_paths", []),
        "validation_errors": validation.get("errors", []),
    }


def summarize_harness(rows: list[dict[str, Any]]) -> dict[str, Any]:
    successful = [row for row in rows if row["success"]]
    wall_values = [row["wall_ms"] for row in rows]
    agent_start_values = [
        row["agent_start_ms"] for row in rows if row["agent_start_ms"] is not None
    ]
    ttft_values = [row["ttft_ms"] for row in rows if row["ttft_ms"] is not None]
    model_ttft_values = [
        row["model_ttft_ms"] for row in rows if row["model_ttft_ms"] is not None
    ]
    answer_values = [
        row["time_to_answer_ms"] for row in rows if row["time_to_answer_ms"] is not None
    ]
    first_tool_values = [
        row["time_to_first_tool_ms"]
        for row in rows
        if row["time_to_first_tool_ms"] is not None
    ]
    first_tool_result_values = [
        row["time_to_first_tool_result_ms"]
        for row in rows
        if row["time_to_first_tool_result_ms"] is not None
    ]
    total_cost = sum(row["normalized_cost_usd"] for row in rows)
    total_prompt = sum(row["total_prompt_tokens"] for row in rows)
    return {
        "runs": len(rows),
        "successful": len(successful),
        "success_rate": len(successful) / len(rows) if rows else 0.0,
        "answer_correct": sum(row["answer_correct"] for row in rows),
        "workspace_correct": sum(row["workspace_correct"] for row in rows),
        "timeouts": sum(row["timed_out"] for row in rows),
        "nonzero_exits": sum(row["exit_code"] != 0 for row in rows),
        "reported_providers": sorted(
            {
                row["reported_provider"]
                for row in rows
                if isinstance(row.get("reported_provider"), str)
            }
        ),
        "wall_total_ms": round(sum(wall_values), 3),
        "wall_median_ms": median(wall_values),
        "wall_workload_p95_ms": percentile(wall_values, 0.95),
        "agent_start_median_ms": median(agent_start_values),
        "ttft_median_ms": median(ttft_values),
        "ttft_workload_p95_ms": percentile(ttft_values, 0.95),
        "model_ttft_median_ms": median(model_ttft_values),
        "time_to_answer_median_ms": median(answer_values),
        "time_to_first_tool_median_ms": median(first_tool_values),
        "time_to_first_tool_result_median_ms": median(first_tool_result_values),
        "tool_duration_sum_ms": sum_optional(row["tool_duration_sum_ms"] for row in rows),
        "tool_duration_max_median_ms": median(row["tool_duration_max_ms"] for row in rows),
        "input_tokens": sum(row["input_tokens"] for row in rows),
        "output_tokens": sum(row["output_tokens"] for row in rows),
        "cache_read_tokens": sum(row["cache_read_tokens"] for row in rows),
        "cache_write_tokens": sum(row["cache_write_tokens"] for row in rows),
        "total_prompt_tokens": total_prompt,
        "total_tokens": sum(row["total_tokens"] for row in rows),
        "cache_read_share": (
            sum(row["cache_read_tokens"] for row in rows) / total_prompt if total_prompt else 0.0
        ),
        "tool_calls": sum(row["tool_calls"] for row in rows),
        "turns": sum(row["turns"] for row in rows),
        "normalized_cost_usd": total_cost,
        "input_cost_usd": sum(row["input_cost_usd"] for row in rows),
        "output_cost_usd": sum(row["output_cost_usd"] for row in rows),
        "cache_read_cost_usd": sum(row["cache_read_cost_usd"] for row in rows),
        "cache_write_cost_usd": sum(row["cache_write_cost_usd"] for row in rows),
        "successful_cost_usd": sum(
            row["normalized_cost_usd"] for row in rows if row["success"]
        ),
        "failed_cost_usd": sum(
            row["normalized_cost_usd"] for row in rows if not row["success"]
        ),
        "normalized_cost_per_success_usd": total_cost / len(successful) if successful else None,
        "successful_attempts_per_usd": len(successful) / total_cost if total_cost else None,
        "tokens_per_success": (
            sum(row["total_tokens"] for row in rows) / len(successful) if successful else None
        ),
        "tool_calls_per_success": (
            sum(row["tool_calls"] for row in rows) / len(successful) if successful else None
        ),
        "turns_per_success": (
            sum(row["turns"] for row in rows) / len(successful) if successful else None
        ),
    }


def task_comparison(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    by_task: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        by_task[row["task"]].append(row)
    comparisons: list[dict[str, Any]] = []
    for task, task_rows in by_task.items():
        by_harness = {
            harness: [row for row in task_rows if row["harness"] == harness]
            for harness in {row["harness"] for row in task_rows}
        }
        item: dict[str, Any] = {
            "task": task,
            "category": task_rows[0]["category"],
            "mode": task_rows[0]["mode"],
        }
        for harness, harness_rows in by_harness.items():
            item[harness] = {
                "success_rate": sum(row["success"] for row in harness_rows) / len(harness_rows),
                "wall_median_ms": median(row["wall_ms"] for row in harness_rows),
                "ttft_median_ms": median(row["ttft_ms"] for row in harness_rows),
                "cost_median_usd": median(row["normalized_cost_usd"] for row in harness_rows),
                "tools_median": median(row["tool_calls"] for row in harness_rows),
                "turns_median": median(row["turns"] for row in harness_rows),
            }
        for peer in ("pi", "omp", "claude", "kiss"):
            if "orca" not in item or peer not in item:
                continue
            item[f"orca_minus_{peer}_wall_ms"] = difference(
                item["orca"]["wall_median_ms"], item[peer]["wall_median_ms"]
            )
            item[f"orca_minus_{peer}_ttft_ms"] = difference(
                item["orca"]["ttft_median_ms"], item[peer]["ttft_median_ms"]
            )
            item[f"orca_minus_{peer}_cost_usd"] = difference(
                item["orca"]["cost_median_usd"], item[peer]["cost_median_usd"]
            )
        comparisons.append(item)
    return comparisons


def markdown(summary: dict[str, Any]) -> str:
    present_harnesses = [
        harness for harness in HARNESS_ORDER if harness in summary["harnesses"]
    ]
    lines = [
        "# "
        + " vs ".join(HARNESS_NAMES[harness] for harness in present_harnesses)
        + " benchmark summary",
        "",
        f"Model: `{summary['model']}` · repetitions: {summary['repetitions']}",
        "",
        "Normalized cost uses the price schedule recorded in `manifest.json`; it is not an invoice.",
        "TTFT is process start to the first model-generated delta, including hidden reasoning or tool arguments.",
        "Model TTFT subtracts the observed agent/turn-start timestamp from that end-to-end TTFT.",
        "Workload P95 is a percentile across this workload, not a latency confidence bound.",
        "",
        "| Harness | Correct | Timeouts | Wall median | E2E TTFT | Startup | Model TTFT | Answer median | Total tokens | Tools | Turns | Normalized cost |",
        "| :-- | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: |",
    ]
    for harness in present_harnesses:
        item = summary["harnesses"][harness]
        lines.append(
            f"| {harness} | {item['successful']}/{item['runs']} | {item['timeouts']} | "
            f"{milliseconds(item['wall_median_ms'])} | {milliseconds(item['ttft_median_ms'])} | "
            f"{milliseconds(item['agent_start_median_ms'])} | "
            f"{milliseconds(item['model_ttft_median_ms'])} | "
            f"{milliseconds(item['time_to_answer_median_ms'])} | {item['total_tokens']:,} | "
            f"{item['tool_calls']} | {item['turns']} | ${item['normalized_cost_usd']:.6f} |"
        )
    lines.extend(
        [
            "",
            "## Per-task median results",
            "",
            "| Task | Harness | Category | Mode | Success | Wall | E2E TTFT | Cost | Tools | Turns |",
            "| :-- | :-- | :-- | :-- | --: | --: | --: | --: | --: | --: |",
        ]
    )
    for item in summary["tasks"]:
        for harness in HARNESS_ORDER:
            if harness not in item:
                continue
            values = item[harness]
            lines.append(
                f"| {item['task']} | {harness} | {item['category']} | {item['mode']} | "
                f"{values['success_rate']:.0%} | {milliseconds(values['wall_median_ms'])} | "
                f"{milliseconds(values['ttft_median_ms'])} | "
                f"${values['cost_median_usd']:.6f} | {values['tools_median']:.1f} | "
                f"{values['turns_median']:.1f} |"
            )
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    run_dir = args.run_dir.resolve()
    manifest_path = run_dir / "manifest.json"
    if not manifest_path.is_file():
        raise FileNotFoundError(f"missing manifest: {manifest_path}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    raw_paths = sorted(run_dir.glob("rep-*.jsonl"))
    if not raw_paths:
        raise ValueError(f"no raw benchmark streams found in {run_dir}")
    rows = [parse_raw(path, manifest) for path in raw_paths]

    harness_rows: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in rows:
        harness_rows[row["harness"]].append(row)
    summary = {
        "schema_version": 4,
        "model": manifest["model"],
        "provider": manifest["provider"],
        "provider_routing": manifest.get("provider_routing"),
        "effort": manifest["effort"],
        "repetitions": manifest["repetitions"],
        "pricing": manifest["pricing"],
        "source": manifest.get("source"),
        "binaries": manifest.get("harnesses"),
        "harnesses": {
            harness: summarize_harness(items) for harness, items in sorted(harness_rows.items())
        },
        "tasks": task_comparison(rows),
    }
    (run_dir / "runs.json").write_text(json.dumps(rows, indent=2) + "\n", encoding="utf-8")
    (run_dir / "summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8"
    )
    (run_dir / "summary.md").write_text(markdown(summary), encoding="utf-8")
    with (run_dir / "runs.csv").open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(
            handle,
            fieldnames=[
                key
                for key in rows[0]
                if key not in {"changed_paths", "validation_errors"}
            ],
        )
        writer.writeheader()
        writer.writerows(
            {
                key: value
                for key, value in row.items()
                if key not in {"changed_paths", "validation_errors"}
            }
            for row in rows
        )
    print(markdown(summary), end="")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (FileNotFoundError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}")
        raise SystemExit(2)
