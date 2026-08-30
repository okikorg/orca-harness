# Orcacode vs Pi benchmark summary

Model: `anthropic/claude-haiku-4.5` · repetitions: 3

Normalized cost uses the price schedule recorded in `manifest.json`; it is not an invoice.
TTFT is process start to the first model-generated delta, including hidden reasoning or tool arguments.
Model TTFT subtracts the observed agent/turn-start timestamp from that end-to-end TTFT.
Workload P95 is a percentile across this workload, not a latency confidence bound.

| Harness | Correct | Timeouts | Wall median | E2E TTFT | Startup | Model TTFT | Answer median | Total tokens | Tools | Turns | Normalized cost |
| :-- | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: |
| orca | 45/48 | 0 | 3630.5 ms | 697.6 ms | 6.7 ms | 692.0 ms | 2342.9 ms | 288,823 | 193 | 153 | $0.366367 |
| pi | 43/48 | 2 | 4831.1 ms | 1189.6 ms | 447.1 ms | 739.9 ms | 4153.0 ms | 625,758 | 271 | 208 | $0.634736 |

## Per-task median comparison

Negative Orcacode deltas favor Orcacode.

| Task | Category | Mode | Orca success | Pi success | Wall delta | TTFT delta | Cost delta |
| :-- | :-- | :-- | --: | --: | --: | --: | --: |
| api-contract-diff | schema-diff | read | 100% | 100% | -554.0 ms | -416.6 ms | $-0.001521 |
| cache-contract | source-doc-consistency | read | 100% | 100% | -12040.0 ms | -453.5 ms | $-0.013884 |
| component-weights | targeted-retrieval | read | 100% | 100% | -564.3 ms | -535.6 ms | $-0.001529 |
| dependency-budget | cost-aggregation | read | 100% | 67% | +303.5 ms | -573.0 ms | $-0.000711 |
| dependency-order | graph-reasoning | read | 100% | 100% | -910.5 ms | -716.7 ms | $-0.002645 |
| feature-precedence | configuration-precedence | read | 100% | 100% | -5257.2 ms | -660.4 ms | $-0.009065 |
| fix-retry-boundary | single-file-code-edit | edit | 100% | 100% | -890.6 ms | -554.2 ms | $-0.003441 |
| fix-worker-heartbeat | cross-file-config-edit | edit | 100% | 100% | -678.2 ms | -476.0 ms | $-0.003075 |
| incident-correlation | log-diagnosis | read | 100% | 100% | -1993.2 ms | -332.7 ms | $-0.003550 |
| invoice-endpoint | cross-format-join | read | 100% | 67% | -24832.2 ms | -513.1 ms | $-0.020435 |
| invoice-slo | threshold-evaluation | read | 100% | 100% | -4284.2 ms | -674.9 ms | $-0.012912 |
| latest-breaking-change | temporal-reasoning | read | 100% | 100% | -621.3 ms | -342.0 ms | $-0.001586 |
| retry-budgets | repository-search | read | 100% | 67% | -22444.8 ms | -933.6 ms | $-0.003204 |
| role-permissions | structured-data-reasoning | read | 0% | 33% | -280.8 ms | -344.5 ms | $-0.001564 |
| timeout-delta | cross-file-comparison | read | 100% | 100% | -4652.3 ms | -706.2 ms | $-0.004669 |
| traffic-aggregate | structured-aggregation | read | 100% | 100% | -233.4 ms | -440.6 ms | $-0.000946 |
