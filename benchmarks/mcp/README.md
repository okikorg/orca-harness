# MCP search benchmarks

These deterministic benchmarks evaluate the metadata search used by
`mcp_search_tools`. The sanitized corpus snapshots the 18 tools exposed by this
workspace's enabled AWS Docs and read-only GitHub MCP servers; it contains no
server commands, credentials, or full input schemas.

`corpus.py` mirrors the production search: separator-aware tokens, exact-token
preference, basic singular/plural fallback in tool names, weighted
name/description/server fields, exact-name bonuses, shorter-name tie-breaking,
then stable catalog order.

## Run

```bash
python3 benchmarks/mcp/pareto.py
python3 benchmarks/mcp/distribution.py
python3 benchmarks/mcp/accuracy.py
python3 -m unittest discover -s benchmarks/mcp -p '*_test.py' -v
```

## What they measure

- `pareto.py` — the 1–100 cutoff Pareto curve, balancing target coverage
  against average returned-result volume and identifying the measured knee.
- `distribution.py` — target coverage and result volume at limits 10–100.
- `accuracy.py` — target recall, top-1 accuracy, mean reciprocal rank (MRR),
  one-relevant-target precision, average result count, ambiguity, and problem
  queries grouped as deliberate, short, or underspecified.
- `corpus.py` — shared tools, labeled queries, and the exact substring matcher;
  both reports use this one implementation so their results cannot drift.

The corpus is a regression fixture, not a universal MCP workload. Refresh it
only from sanitized metadata and review metric changes deliberately.
