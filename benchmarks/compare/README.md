# Orcacode–fx comparison

Runs cold-start commands for orcacode and fx on the same host and process floor.
This is informational: commands are grouped by comparable work, but the two
products do not expose identical startup boundaries. Read the methodology in
the root [`benchmarks/README.md`](../README.md) before quoting results.

Requires `hyperfine`, Python 3, a release `orcacode`, and an `fx` binary.

```bash
./benchmarks/compare/run.sh
./benchmarks/compare/run.sh --quick
FX_BIN=/path/to/fx ./benchmarks/compare/run.sh
```

Generated data is written to `benchmarks/results/compare/`. `report.py`
subtracts the same-session process-launch baseline and renders work tiers.
