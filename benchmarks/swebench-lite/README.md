# SWE-bench Lite

This adapter generates official-format SWE-bench patch predictions with
Orcacode. Start with three deterministic instances from Lite's 23-instance
development split before spending money on the 300-instance test split.

## Generate the smoke predictions

Review the commands without cloning repositories or calling a model:

```bash
python3 benchmarks/swebench-lite/run.py --dry-run
```

Generate three patches using the same low-cost model as the local harness
comparison suite:

```bash
python3 benchmarks/swebench-lite/run.py
```

The smoke profile permits 64 model steps and 8,192 output tokens per instance;
the external timeout is 15 minutes. The model may finish earlier.

The timestamped result directory contains `predictions.jsonl`, a manifest,
the patch for each instance, and raw Orcacode stdout/stderr. The dataset's gold
patch and hidden test patch are never written to the result or shown to the
agent. Lite excludes tasks whose reference fix creates or removes files, so the
runner intentionally omits untracked scratch files from submitted patches.

## Grade with the official harness

Install SWE-bench in a separate environment and pass the generated predictions
to its Docker evaluator:

```bash
python -m swebench.harness.run_evaluation \
  --dataset_name SWE-bench/SWE-bench_Lite \
  --predictions_path benchmarks/results/swebench-lite/<run-id>/predictions.jsonl \
  --instance_ids \
    marshmallow-code__marshmallow-1343 \
    marshmallow-code__marshmallow-1359 \
    pvlib__pvlib-python-1072 \
  --max_workers 2 \
  --cache_level base \
  --clean True \
  --run_id orcacode-lite-smoke-<run-id>
```

SWE-bench recommends x86_64, at least 120 GB free storage, 16 GB RAM, and 8
CPUs. ARM64 support is experimental. Do not treat patch generation alone as a
score: only the official evaluator's resolved count is a SWE-bench result.

## Tests

```bash
python3 -m unittest discover -s benchmarks/swebench-lite -p '*_test.py' -v
```
