# Orcacode vs Pi benchmark summary

Model: `anthropic/claude-haiku-4.5` · repetitions: 1

Normalized cost uses the price schedule recorded in `manifest.json`; it is not an invoice.
TTFT is process start to the first model-generated delta, including hidden reasoning or tool arguments.
Model TTFT subtracts the observed agent/turn-start timestamp from that end-to-end TTFT.
Workload P95 is a percentile across this workload, not a latency confidence bound.

| Harness | Correct | Timeouts | Wall median | E2E TTFT | Startup | Model TTFT | Answer median | Total tokens | Tools | Turns | Normalized cost |
| :-- | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: |
| orca | 15/16 | 0 | 3805.9 ms | 691.8 ms | 6.8 ms | 685.2 ms | 2463.5 ms | 96,487 | 72 | 52 | $0.122735 |
| pi | 14/16 | 0 | 4748.1 ms | 1171.3 ms | 428.1 ms | 743.9 ms | 4325.2 ms | 140,617 | 73 | 55 | $0.168769 |

## Per-task median comparison

Negative Orcacode deltas favor Orcacode.

| Task | Category | Mode | Orca success | Pi success | Wall delta | TTFT delta | Cost delta |
| :-- | :-- | :-- | --: | --: | --: | --: | --: |
| api-contract-diff | schema-diff | read | 100% | 100% | -127.2 ms | -531.6 ms | $-0.001178 |
| cache-contract | source-doc-consistency | read | 100% | 100% | -5382.8 ms | -612.5 ms | $-0.004306 |
| component-weights | targeted-retrieval | read | 100% | 100% | -487.2 ms | -190.1 ms | $-0.001441 |
| dependency-budget | cost-aggregation | read | 100% | 0% | +508.1 ms | -438.3 ms | $-0.000403 |
| dependency-order | graph-reasoning | read | 100% | 100% | +130.4 ms | -469.6 ms | $-0.002248 |
| feature-precedence | configuration-precedence | read | 100% | 100% | -3124.9 ms | -392.2 ms | $-0.008920 |
| fix-retry-boundary | single-file-code-edit | edit | 100% | 100% | -924.5 ms | -409.5 ms | $-0.003146 |
| fix-worker-heartbeat | cross-file-config-edit | edit | 100% | 100% | -1335.5 ms | -454.4 ms | $-0.002889 |
| incident-correlation | log-diagnosis | read | 100% | 100% | +1600.4 ms | -372.0 ms | $-0.000393 |
| invoice-endpoint | cross-format-join | read | 100% | 100% | -2234.3 ms | -618.2 ms | $-0.003624 |
| invoice-slo | threshold-evaluation | read | 100% | 100% | -2366.3 ms | -579.3 ms | $-0.002740 |
| latest-breaking-change | temporal-reasoning | read | 100% | 100% | -402.9 ms | -524.5 ms | $-0.001337 |
| retry-budgets | repository-search | read | 100% | 100% | -766.1 ms | -293.7 ms | $-0.002791 |
| role-permissions | structured-data-reasoning | read | 0% | 0% | -318.9 ms | -372.2 ms | $-0.001339 |
| timeout-delta | cross-file-comparison | read | 100% | 100% | -3643.5 ms | -545.8 ms | $-0.008137 |
| traffic-aggregate | structured-aggregation | read | 100% | 100% | -108.9 ms | -455.9 ms | $-0.001142 |
