#!/usr/bin/env python3
"""Render a self-contained HTML report from analyzed benchmark artifacts."""

from __future__ import annotations

import argparse
import html
import json
import math
import os
from pathlib import Path
from typing import Any


NAMES = {
    "orca": "Orcacode",
    "pi": "Pi",
    "omp": "Oh My Pi",
    "claude": "Claude Code",
}
ORDER = ("orca", "pi", "omp", "claude")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Render harness benchmark HTML")
    parser.add_argument("run_dir", type=Path)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def milliseconds(value: float | None) -> str:
    return f"{value / 1000:.2f}s" if value is not None else "n/a"


def integer(value: int | float | None) -> str:
    return f"{int(value):,}" if value is not None else "n/a"


def dollars(value: float | None) -> str:
    return f"${value:.6f}" if value is not None else "n/a"


def percentage(value: float | None) -> str:
    return f"{value:.1%}" if value is not None else "n/a"


def cell(value: Any) -> str:
    return html.escape(str(value))


def english_list(values: list[str]) -> str:
    if len(values) < 2:
        return "".join(values)
    if len(values) == 2:
        return " and ".join(values)
    return f"{', '.join(values[:-1])}, and {values[-1]}"


def pareto_frontier(harnesses: dict[str, Any], order: tuple[str, ...]) -> list[str]:
    frontier = []
    best_correct = -1
    for name in sorted(order, key=lambda key: harnesses[key]["normalized_cost_usd"]):
        if harnesses[name]["successful"] > best_correct:
            frontier.append(name)
            best_correct = harnesses[name]["successful"]
    return frontier


def pareto_chart(harnesses: dict[str, Any]) -> str:
    order = tuple(name for name in ORDER if name in harnesses)
    width, height = 920, 430
    left, right, top, bottom = 72, 42, 34, 66
    plot_width = width - left - right
    plot_height = height - top - bottom
    max_cost = math.ceil(
        max(item["normalized_cost_usd"] for item in harnesses.values()) * 20
    ) / 20
    max_correct = max(item["runs"] for item in harnesses.values())

    def x_position(cost: float) -> float:
        return left + (cost / max_cost) * plot_width

    def y_position(correct: int) -> float:
        return top + ((max_correct - correct) / max_correct) * plot_height

    points = {
        name: (
            x_position(harnesses[name]["normalized_cost_usd"]),
            y_position(harnesses[name]["successful"]),
        )
        for name in order
    }
    frontier = pareto_frontier(harnesses, order)

    grid = []
    for correct in range(0, max_correct + 1, 4):
        y = y_position(correct)
        grid.append(
            f"<line x1='{left}' y1='{y:.1f}' x2='{width-right}' y2='{y:.1f}'/>"
            f"<text x='{left-14}' y='{y+4:.1f}' text-anchor='end'>{correct}</text>"
        )
    for index in range(5):
        cost = max_cost * index / 4
        x = x_position(cost)
        grid.append(
            f"<line x1='{x:.1f}' y1='{top}' x2='{x:.1f}' y2='{height-bottom}'/>"
            f"<text x='{x:.1f}' y='{height-bottom+28}' text-anchor='middle'>${cost:.3f}</text>"
        )

    frontier_path = " ".join(
        f"{'M' if index == 0 else 'L'} {points[name][0]:.1f} {points[name][1]:.1f}"
        for index, name in enumerate(frontier)
    )
    markers = []
    for name in order:
        item = harnesses[name]
        x, y = points[name]
        label_y = y + 32 if y < 80 else y - 38
        label_x = x - 12 if x > width - right - 120 else x
        label_anchor = "end" if label_x != x else "middle"
        dominated = " · dominated" if name not in frontier else ""
        markers.append(
            f"<g class='pareto-point {name}'>"
            f"<circle cx='{x:.1f}' cy='{y:.1f}' r='9'/>"
            f"<text x='{label_x:.1f}' y='{label_y:.1f}' text-anchor='{label_anchor}'>{NAMES[name]}</text>"
            f"<text class='point-detail' x='{label_x:.1f}' y='{label_y+17:.1f}' text-anchor='{label_anchor}'>"
            f"{item['successful']}/{item['runs']} · {dollars(item['normalized_cost_usd'])}{dominated}</text>"
            "</g>"
        )

    return (
        f"<svg viewBox='0 0 {width} {height}' role='img' aria-labelledby='pareto-title pareto-desc'>"
        "<title id='pareto-title'>Cost and correctness Pareto frontier</title>"
        "<desc id='pareto-desc'>Lower normalized cost is better on the horizontal axis. "
        "More correct tasks is better on the vertical axis.</desc>"
        f"<g class='pareto-grid'>{''.join(grid)}</g>"
        f"<path class='frontier-line' d='{frontier_path}'/>"
        f"{''.join(markers)}"
        f"<text class='axis-label' x='{left+plot_width/2:.1f}' y='{height-12}' text-anchor='middle'>Normalized cost · lower is better</text>"
        f"<text class='axis-label' x='18' y='{top+plot_height/2:.1f}' text-anchor='middle' transform='rotate(-90 18 {top+plot_height/2:.1f})'>Correct tasks · higher is better</text>"
        "</svg>"
    )


def render(
    summary: dict[str, Any],
    runs: list[dict[str, Any]],
    manifest: dict[str, Any],
    output: Path,
    run_dir: Path,
) -> str:
    harnesses = summary["harnesses"]
    order = tuple(name for name in ORDER if name in harnesses)
    tasks = manifest["tasks"]
    relative_run = Path(os.path.relpath(run_dir, output.parent)).as_posix()
    aggregate_rows = []
    for name in order:
        item = harnesses[name]
        aggregate_rows.append(
            "<tr>"
            f"<th><span class='dot {name}'></span>{NAMES[name]}</th>"
            f"<td>{item['successful']}/{item['runs']}</td>"
            f"<td>{item['timeouts']}</td>"
            f"<td>{milliseconds(item['wall_median_ms'])}</td>"
            f"<td>{milliseconds(item['ttft_median_ms'])}</td>"
            f"<td>{milliseconds(item['model_ttft_median_ms'])}</td>"
            f"<td>{integer(item['total_tokens'])}</td>"
            f"<td>{integer(item['tool_calls'])}</td>"
            f"<td>{integer(item['turns'])}</td>"
            f"<td>{dollars(item['normalized_cost_usd'])}</td>"
            "</tr>"
        )

    token_rows = []
    for name in order:
        item = harnesses[name]
        providers = ", ".join(item.get("reported_providers", [])) or "unavailable"
        token_rows.append(
            "<tr>"
            f"<th>{NAMES[name]}</th>"
            f"<td>{integer(item['input_tokens'])}</td>"
            f"<td>{integer(item['output_tokens'])}</td>"
            f"<td>{integer(item['cache_read_tokens'])}</td>"
            f"<td>{integer(item['cache_write_tokens'])}</td>"
            f"<td>{percentage(item['cache_read_share'])}</td>"
            f"<td>{cell(providers)}</td>"
            "</tr>"
        )

    workload_cards = []
    for task in tasks:
        workload_cards.append(
            "<article class='task-card'>"
            f"<div><code>{cell(task['id'])}</code><span>{cell(task['mode'])}</span></div>"
            f"<h3>{cell(task['category'])}</h3>"
            f"<p>{cell(task['prompt'])}</p>"
            f"<small>Expected: <code>{cell(task['expected'])}</code></small>"
            "</article>"
        )

    run_rows = []
    run_order = {name: index for index, name in enumerate(order)}
    for row in sorted(runs, key=lambda item: (item["task"], run_order[item["harness"]])):
        result = "pass" if row["success"] else "fail"
        run_rows.append(
            "<tr>"
            f"<th>{cell(row['task'])}</th>"
            f"<td>{NAMES[row['harness']]}</td>"
            f"<td><span class='status {result}'>{result}</span></td>"
            f"<td>{milliseconds(row['wall_ms'])}</td>"
            f"<td>{milliseconds(row['ttft_ms'])}</td>"
            f"<td>{integer(row['input_tokens'])}</td>"
            f"<td>{integer(row['output_tokens'])}</td>"
            f"<td>{integer(row['cache_read_tokens'])}</td>"
            f"<td>{integer(row['cache_write_tokens'])}</td>"
            f"<td>{integer(row['tool_calls'])}</td>"
            f"<td>{integer(row['turns'])}</td>"
            f"<td>{dollars(row['normalized_cost_usd'])}</td>"
            "</tr>"
        )

    failure_rows = []
    for row in sorted((item for item in runs if not item["success"]), key=lambda x: (x["harness"], x["task"])):
        if row["timed_out"]:
            reason = "45-second timeout"
        elif row["exit_code"] != 0:
            reason = f"nonzero exit ({row['exit_code']})"
        else:
            reason = f"answer mismatch: {row['answer'] or 'no final answer'}"
        failure_rows.append(
            "<tr>"
            f"<th>{NAMES[row['harness']]}</th>"
            f"<td>{cell(row['task'])}</td>"
            f"<td>{cell(reason)}</td>"
            f"<td>{integer(row['total_tokens'])}</td>"
            f"<td>{dollars(row['normalized_cost_usd'])}</td>"
            "</tr>"
        )

    best = max(order, key=lambda name: (harnesses[name]["successful"], -harnesses[name]["normalized_cost_usd"]))
    fastest = min(order, key=lambda name: harnesses[name]["wall_median_ms"])
    cheapest = min(order, key=lambda name: harnesses[name]["normalized_cost_usd"])
    scores = " · ".join(
        f"{NAMES[name]} {harnesses[name]['successful']}/{harnesses[name]['runs']}"
        for name in order
        if name != best
    )
    costs = " · ".join(
        f"{NAMES[name]} {dollars(harnesses[name]['normalized_cost_usd'])}"
        for name in order
        if name != cheapest
    )
    frontier = pareto_frontier(harnesses, order)
    dominated = [name for name in order if name not in frontier]
    pareto_summary = (
        f"{english_list([NAMES[name] for name in frontier])} form the efficient frontier in this run."
    )
    if dominated:
        pareto_summary += (
            f" {english_list([NAMES[name] for name in dominated])}"
            f" {'is' if len(dominated) == 1 else 'are'} dominated."
        )
    provider_labels = "; ".join(
        f"{NAMES[name]}: {', '.join(harnesses[name].get('reported_providers', [])) or 'unavailable'}"
        for name in order
    )
    pareto = pareto_chart(harnesses)
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{cell(english_list([NAMES[name] for name in order]))} benchmark</title>
<style>
:root{{--bg:#090c10;--panel:#11171e;--panel2:#161e27;--line:#293542;--text:#edf3f8;--muted:#9baaba;--orca:#67d7ad;--pi:#e4b65b;--omp:#8fb3ff;--claude:#df8769;--good:#70d6a5;--bad:#f08787}}
*{{box-sizing:border-box}} body{{margin:0;background:var(--bg);color:var(--text);font:15px/1.55 ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}} a{{color:var(--orca)}} code{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;color:#dbe8f2}} main{{max-width:1440px;margin:auto;padding:56px 28px 80px}} header{{max-width:980px;margin-bottom:38px}} .eyebrow{{color:var(--orca);font-size:12px;font-weight:750;letter-spacing:.14em;text-transform:uppercase}} h1{{font-size:clamp(36px,6vw,76px);line-height:1.02;letter-spacing:-.045em;margin:12px 0 18px}} .lede{{color:var(--muted);font-size:19px;max-width:850px}} section{{margin-top:42px}} h2{{font-size:24px;margin:0 0 8px}} .intro{{color:var(--muted);max-width:900px;margin:0 0 18px}} .cards{{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:14px}} .card,.task-card{{background:linear-gradient(145deg,var(--panel2),var(--panel));border:1px solid var(--line);border-radius:16px;padding:20px}} .card strong{{display:block;font-size:34px;letter-spacing:-.04em}} .card span,.task-card small{{color:var(--muted)}} .pareto{{max-width:1040px;padding:18px 18px 8px;border:1px solid var(--line);border-radius:16px;background:linear-gradient(145deg,var(--panel2),var(--panel))}} .pareto svg{{display:block;width:100%;height:auto}} .pareto-grid line{{stroke:var(--line);stroke-width:1}} .pareto-grid text,.axis-label,.point-detail{{fill:var(--muted);font-size:12px}} .frontier-line{{fill:none;stroke:var(--orca);stroke-width:3;stroke-dasharray:8 7}} .pareto-point circle{{stroke:var(--bg);stroke-width:4}} .pareto-point text{{fill:var(--text);font-size:13px;font-weight:750}} .pareto-point.orca circle{{fill:var(--orca)}} .pareto-point.pi circle{{fill:var(--pi)}} .pareto-point.omp circle{{fill:var(--omp)}} .pareto-point.claude circle{{fill:var(--claude)}} .pareto-point .point-detail{{fill:var(--muted);font-size:11px;font-weight:500}} .table-wrap{{overflow:auto;border:1px solid var(--line);border-radius:14px;background:var(--panel)}} table{{border-collapse:collapse;width:100%;min-width:980px}} th,td{{padding:12px 14px;border-bottom:1px solid var(--line);text-align:right;white-space:nowrap}} th:first-child,td:first-child{{text-align:left}} thead th{{color:var(--muted);font-size:12px;text-transform:uppercase;letter-spacing:.06em;background:var(--panel2);position:sticky;top:0}} tbody tr:last-child th,tbody tr:last-child td{{border-bottom:0}} .dot{{display:inline-block;width:9px;height:9px;border-radius:50%;margin-right:9px}} .dot.orca{{background:var(--orca)}} .dot.pi{{background:var(--pi)}} .dot.omp{{background:var(--omp)}} .dot.claude{{background:var(--claude)}} .status{{font-size:11px;font-weight:800;letter-spacing:.08em;text-transform:uppercase}} .status.pass{{color:var(--good)}} .status.fail{{color:var(--bad)}} .tasks{{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:12px}} .task-card div{{display:flex;justify-content:space-between;gap:12px}} .task-card div span{{color:var(--muted);font-size:12px;text-transform:uppercase}} .task-card h3{{font-size:13px;color:var(--muted);text-transform:uppercase;letter-spacing:.06em;margin:14px 0 8px}} .task-card p{{margin:0 0 12px}} .task-card small{{display:block;overflow-wrap:anywhere}} .method{{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:12px}} .method article{{border-left:2px solid var(--line);padding:2px 18px}} .method h3{{margin:0 0 6px;font-size:15px}} .method p{{margin:0;color:var(--muted)}} footer{{margin-top:48px;padding-top:22px;border-top:1px solid var(--line);color:var(--muted)}} @media(max-width:800px){{main{{padding:36px 16px 60px}}.cards,.tasks,.method{{grid-template-columns:1fr}}.pareto{{padding:8px}}}}
</style>
</head>
<body><main>
<header><div class="eyebrow">Live headless benchmark · run {cell(run_dir.name)}</div><h1>{NAMES[cheapest]} is leanest; {NAMES[best]} is most accurate.</h1><p class="lede">{summary['repetitions']} repetition{'s' if summary['repetitions'] != 1 else ''} across {len(tasks)} synthetic repository tasks using <code>{cell(summary['model'])}</code> at {cell(summary['effort'])} effort. All harnesses were configured for OpenRouter with isolated workspaces, exact-answer scoring, and a rotating execution order.</p></header>

<section><div class="cards">
<article class="card"><span>Best correctness</span><strong>{NAMES[best]} {harnesses[best]['successful']}/{harnesses[best]['runs']}</strong><span>{scores}</span></article>
<article class="card"><span>Lowest median wall time</span><strong>{milliseconds(harnesses[fastest]['wall_median_ms'])}</strong><span>{NAMES[fastest]}</span></article>
<article class="card"><span>Lowest normalized cost</span><strong>{dollars(harnesses[cheapest]['normalized_cost_usd'])}</strong><span>{NAMES[cheapest]} · {costs}</span></article>
</div></section>

<section><h2>Cost–correctness Pareto frontier</h2><p class="intro">Lower cost and higher correctness are better. {pareto_summary}</p><div class="pareto">{pareto}</div></section>

<section><h2>Aggregate performance</h2><p class="intro">Correctness is the primary guardrail. Wall and TTFT are medians across the {len(tasks)}-task workload; totals include failed and timed-out attempts.</p><div class="table-wrap"><table><thead><tr><th>Harness</th><th>Correct</th><th>Timeouts</th><th>Wall median</th><th>E2E TTFT</th><th>Model TTFT</th><th>Total tokens</th><th>Tools</th><th>Turns</th><th>Normalized cost</th></tr></thead><tbody>{''.join(aggregate_rows)}</tbody></table></div></section>

<section><h2>Token economics and caching</h2><p class="intro">Provider-reported token classes are normalized using the recorded Haiku 4.5 schedule. Cache reads are cheaper than uncached input, but still represent context processed and are included in total prompt tokens.</p><div class="table-wrap"><table><thead><tr><th>Harness</th><th>Uncached input</th><th>Output</th><th>Cache read</th><th>Cache write</th><th>Cache-read share</th><th>Reported provider</th></tr></thead><tbody>{''.join(token_rows)}</tbody></table></div></section>

<section><h2>Failures and wasted spend</h2><p class="intro">Every failed, timed-out, or validation-rejected attempt is retained here with its token and normalized-cost impact.</p><div class="table-wrap"><table><thead><tr><th>Harness</th><th>Task</th><th>Reason</th><th>Tokens</th><th>Normalized cost</th></tr></thead><tbody>{''.join(failure_rows)}</tbody></table></div></section>

<section><h2>The {len(tasks)} benchmark tasks</h2><p class="intro">These are the exact workload requests and scoring terminals, not anonymous task numbers.</p><div class="tasks">{''.join(workload_cards)}</div></section>

<section><h2>Every measured attempt</h2><p class="intro">Claude tool execution duration is unavailable because its stream does not expose execution start/end events equivalent to the other harnesses. Tool count remains comparable.</p><div class="table-wrap"><table><thead><tr><th>Task</th><th>Harness</th><th>Result</th><th>Wall</th><th>E2E TTFT</th><th>Input</th><th>Output</th><th>Cache read</th><th>Cache write</th><th>Tools</th><th>Turns</th><th>Cost</th></tr></thead><tbody>{''.join(run_rows)}</tbody></table></div></section>

<section><h2>Method and boundaries</h2><div class="method"><article><h3>Schedule</h3><p>{len(tasks)} tasks × {len(order)} harnesses × {summary['repetitions']} repetition{'s' if summary['repetitions'] != 1 else ''} = {len(runs)} sequential attempts. Harness order rotated by task.</p></article><article><h3>Isolation</h3><p>Fresh fixture and session state per attempt; explicit file-tool allowlists; no shell, web, MCP, plugins, skills, subagents, or servers.</p></article><article><h3>Interpretation</h3><p>This is directional evidence. OpenRouter's underlying endpoint was not pinned across all harnesses. Reported provider labels: {cell(provider_labels)}.</p></article></div></section>

<footer>Artifacts: <a href="{relative_run}/summary.json">summary.json</a> · <a href="{relative_run}/runs.csv">runs.csv</a> · <a href="{relative_run}/summary.md">summary.md</a> · <a href="{relative_run}/manifest.json">manifest.json</a><br>Generated from retained raw streams. The temporary credential handoff was deleted before provider execution.</footer>
</main></body></html>"""


def main() -> int:
    args = parse_args()
    run_dir = args.run_dir.resolve()
    output = (args.output or run_dir / "report.html").resolve()
    summary = json.loads((run_dir / "summary.json").read_text(encoding="utf-8"))
    runs = json.loads((run_dir / "runs.json").read_text(encoding="utf-8"))
    manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(render(summary, runs, manifest, output, run_dir), encoding="utf-8")
    print(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
