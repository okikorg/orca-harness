# Kernel benchmarks

Measures harness dispatch and fan-out overhead with deterministic no-op probes,
then optionally reports real-tool behavior and Criterion estimates.

```bash
./benchmarks/kernel/run.sh
./benchmarks/kernel/run.sh --quick
./benchmarks/kernel/run.sh --criterion
./benchmarks/kernel/run.sh --ci
python3 benchmarks/kernel/report_test.py
```

Generated outputs are written to `benchmarks/results/kernel/`. Linux CI applies
the shared latency budgets in `benchmarks/shared/check_budgets.py`; real-tool
measurements remain informational because they depend on filesystem and process
spawn performance. See the root [`benchmarks/README.md`](../README.md) for the
measurement definitions and published baseline.
