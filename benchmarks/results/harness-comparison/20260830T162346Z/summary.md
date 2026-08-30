# Orcacode vs Pi vs Claude Code benchmark summary

Model: `anthropic/claude-haiku-4.5` · repetitions: 1

Normalized cost uses the price schedule recorded in `manifest.json`; it is not an invoice.
TTFT is process start to the first model-generated delta, including hidden reasoning or tool arguments.
Model TTFT subtracts the observed agent/turn-start timestamp from that end-to-end TTFT.
Workload P95 is a percentile across this workload, not a latency confidence bound.

| Harness | Correct | Timeouts | Wall median | E2E TTFT | Startup | Model TTFT | Answer median | Total tokens | Tools | Turns | Normalized cost |
| :-- | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: |
| claude | 9/16 | 2 | 6480.9 ms | 835.7 ms | 176.1 ms | 657.4 ms | 5271.2 ms | 766,108 | 256 | 173 | $0.488511 |
| orca | 14/16 | 0 | 3806.4 ms | 712.4 ms | 7.6 ms | 705.0 ms | 2372.7 ms | 95,518 | 72 | 51 | $0.121338 |
| pi | 15/16 | 0 | 4726.6 ms | 1188.9 ms | 460.8 ms | 720.9 ms | 3954.8 ms | 285,537 | 96 | 85 | $0.235478 |

## Per-task median results

| Task | Harness | Category | Mode | Success | Wall | E2E TTFT | Cost | Tools | Turns |
| :-- | :-- | :-- | :-- | --: | --: | --: | --: | --: | --: |
| api-contract-diff | orca | schema-diff | read | 100% | 2377.6 ms | 651.6 ms | $0.004011 | 2.0 | 2.0 |
| api-contract-diff | pi | schema-diff | read | 100% | 2878.4 ms | 1121.1 ms | $0.005484 | 2.0 | 2.0 |
| api-contract-diff | claude | schema-diff | read | 100% | 4505.0 ms | 830.6 ms | $0.005844 | 2.0 | 3.0 |
| cache-contract | orca | source-doc-consistency | read | 100% | 6238.2 ms | 715.3 ms | $0.014299 | 12.0 | 5.0 |
| cache-contract | pi | source-doc-consistency | read | 100% | 15590.2 ms | 1239.1 ms | $0.041841 | 13.0 | 12.0 |
| cache-contract | claude | source-doc-consistency | read | 0% | 39548.4 ms | 840.9 ms | $0.072411 | 46.0 | 25.0 |
| component-weights | orca | targeted-retrieval | read | 100% | 2070.4 ms | 903.3 ms | $0.003353 | 1.0 | 2.0 |
| component-weights | pi | targeted-retrieval | read | 100% | 2448.6 ms | 1217.2 ms | $0.004833 | 1.0 | 2.0 |
| component-weights | claude | targeted-retrieval | read | 100% | 3995.0 ms | 988.8 ms | $0.004834 | 1.0 | 2.0 |
| dependency-budget | orca | cost-aggregation | read | 0% | 3072.1 ms | 635.1 ms | $0.004599 | 1.0 | 2.0 |
| dependency-budget | pi | cost-aggregation | read | 0% | 2738.9 ms | 1160.6 ms | $0.004973 | 1.0 | 2.0 |
| dependency-budget | claude | cost-aggregation | read | 100% | 6304.1 ms | 790.3 ms | $0.006401 | 1.0 | 2.0 |
| dependency-order | orca | graph-reasoning | read | 100% | 4694.4 ms | 629.9 ms | $0.006465 | 2.0 | 3.0 |
| dependency-order | pi | graph-reasoning | read | 100% | 4691.3 ms | 2373.3 ms | $0.007484 | 2.0 | 3.0 |
| dependency-order | claude | graph-reasoning | read | 100% | 5002.0 ms | 808.6 ms | $0.005476 | 1.0 | 2.0 |
| feature-precedence | orca | configuration-precedence | read | 100% | 5715.7 ms | 624.0 ms | $0.009176 | 9.0 | 3.0 |
| feature-precedence | pi | configuration-precedence | read | 100% | 10057.9 ms | 1119.7 ms | $0.019715 | 7.0 | 6.0 |
| feature-precedence | claude | configuration-precedence | read | 0% | 32582.3 ms | 792.0 ms | $0.064464 | 24.0 | 25.0 |
| fix-retry-boundary | orca | single-file-code-edit | edit | 100% | 3579.9 ms | 688.0 ms | $0.006278 | 2.0 | 3.0 |
| fix-retry-boundary | pi | single-file-code-edit | edit | 100% | 4092.2 ms | 1458.6 ms | $0.009069 | 2.0 | 3.0 |
| fix-retry-boundary | claude | single-file-code-edit | edit | 100% | 5967.1 ms | 1048.7 ms | $0.008350 | 2.0 | 3.0 |
| fix-worker-heartbeat | orca | cross-file-config-edit | edit | 100% | 4004.2 ms | 1020.3 ms | $0.007058 | 3.0 | 3.0 |
| fix-worker-heartbeat | pi | cross-file-config-edit | edit | 100% | 4392.1 ms | 1272.0 ms | $0.010210 | 3.0 | 3.0 |
| fix-worker-heartbeat | claude | cross-file-config-edit | edit | 100% | 6657.6 ms | 1064.2 ms | $0.008789 | 3.0 | 4.0 |
| incident-correlation | orca | log-diagnosis | read | 100% | 4118.3 ms | 709.6 ms | $0.007872 | 3.0 | 4.0 |
| incident-correlation | pi | log-diagnosis | read | 100% | 6621.7 ms | 1082.1 ms | $0.015238 | 7.0 | 5.0 |
| incident-correlation | claude | log-diagnosis | read | 100% | 9198.8 ms | 807.6 ms | $0.015793 | 7.0 | 8.0 |
| invoice-endpoint | orca | cross-format-join | read | 100% | 5508.3 ms | 1219.1 ms | $0.012196 | 9.0 | 4.0 |
| invoice-endpoint | pi | cross-format-join | read | 100% | 34143.2 ms | 1109.5 ms | $0.068546 | 44.0 | 30.0 |
| invoice-endpoint | claude | cross-format-join | read | 0% | 45045.9 ms | 822.8 ms | $0.075089 | 58.0 | 21.0 |
| invoice-slo | orca | threshold-evaluation | read | 100% | 9266.8 ms | 753.4 ms | $0.019995 | 13.0 | 7.0 |
| invoice-slo | pi | threshold-evaluation | read | 100% | 9472.1 ms | 1157.9 ms | $0.014572 | 4.0 | 4.0 |
| invoice-slo | claude | threshold-evaluation | read | 0% | 32652.6 ms | 887.5 ms | $0.062914 | 24.0 | 25.0 |
| latest-breaking-change | orca | temporal-reasoning | read | 100% | 3462.8 ms | 688.3 ms | $0.005474 | 3.0 | 3.0 |
| latest-breaking-change | pi | temporal-reasoning | read | 100% | 4817.7 ms | 1118.4 ms | $0.007579 | 2.0 | 3.0 |
| latest-breaking-change | claude | temporal-reasoning | read | 0% | 5743.6 ms | 1044.7 ms | $0.007569 | 2.0 | 3.0 |
| retry-budgets | orca | repository-search | read | 100% | 3749.8 ms | 820.5 ms | $0.006597 | 6.0 | 3.0 |
| retry-budgets | pi | repository-search | read | 100% | 6037.8 ms | 1310.3 ms | $0.010353 | 4.0 | 4.0 |
| retry-budgets | claude | repository-search | read | 0% | 45033.9 ms | 843.6 ms | $0.077586 | 59.0 | 21.0 |
| role-permissions | orca | structured-data-reasoning | read | 0% | 2459.0 ms | 781.7 ms | $0.003629 | 1.0 | 2.0 |
| role-permissions | pi | structured-data-reasoning | read | 100% | 4761.8 ms | 1446.1 ms | $0.005055 | 1.0 | 2.0 |
| role-permissions | claude | structured-data-reasoning | read | 100% | 4683.2 ms | 849.2 ms | $0.005234 | 1.0 | 2.0 |
| timeout-delta | orca | cross-file-comparison | read | 100% | 3863.0 ms | 836.6 ms | $0.006488 | 4.0 | 3.0 |
| timeout-delta | pi | cross-file-comparison | read | 100% | 4113.2 ms | 1683.0 ms | $0.005520 | 2.0 | 2.0 |
| timeout-delta | claude | cross-file-comparison | read | 0% | 31816.6 ms | 810.3 ms | $0.062483 | 24.0 | 25.0 |
| traffic-aggregate | orca | structured-aggregation | read | 100% | 2549.3 ms | 702.9 ms | $0.003848 | 1.0 | 2.0 |
| traffic-aggregate | pi | structured-aggregation | read | 100% | 2851.6 ms | 1114.2 ms | $0.005006 | 1.0 | 2.0 |
| traffic-aggregate | claude | structured-aggregation | read | 100% | 4405.4 ms | 819.1 ms | $0.005274 | 1.0 | 2.0 |
