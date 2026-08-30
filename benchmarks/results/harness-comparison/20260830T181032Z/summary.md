# Orcacode vs Pi vs Oh My Pi vs Claude Code benchmark summary

Model: `anthropic/claude-haiku-4.5` · repetitions: 3

Normalized cost uses the price schedule recorded in `manifest.json`; it is not an invoice.
TTFT is process start to the first model-generated delta, including hidden reasoning or tool arguments.
Model TTFT subtracts the observed agent/turn-start timestamp from that end-to-end TTFT.
Workload P95 is a percentile across this workload, not a latency confidence bound.

| Harness | Correct | Timeouts | Wall median | E2E TTFT | Startup | Model TTFT | Answer median | Total tokens | Tools | Turns | Normalized cost |
| :-- | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: |
| orca | 43/48 | 0 | 3966.2 ms | 732.9 ms | 6.4 ms | 726.3 ms | 2490.3 ms | 396,283 | 232 | 172 | $0.401968 |
| pi | 41/48 | 0 | 5096.0 ms | 1233.3 ms | 424.9 ms | 779.7 ms | 4447.6 ms | 740,176 | 296 | 230 | $0.689187 |
| omp | 44/48 | 0 | 5261.5 ms | 1367.7 ms | 483.3 ms | 872.0 ms | 4266.7 ms | 841,421 | 155 | 139 | $0.838545 |
| claude | 32/48 | 6 | 7101.3 ms | 995.3 ms | 173.0 ms | 821.5 ms | 5871.6 ms | 2,073,645 | 674 | 515 | $1.415837 |

## Per-task median results

| Task | Harness | Category | Mode | Success | Wall | E2E TTFT | Cost | Tools | Turns |
| :-- | :-- | :-- | :-- | --: | --: | --: | --: | --: | --: |
| api-contract-diff | orca | schema-diff | read | 100% | 2417.1 ms | 743.2 ms | $0.003793 | 2.0 | 2.0 |
| api-contract-diff | pi | schema-diff | read | 100% | 3562.9 ms | 1537.3 ms | $0.005411 | 2.0 | 2.0 |
| api-contract-diff | omp | schema-diff | read | 100% | 4006.3 ms | 1309.7 ms | $0.014827 | 2.0 | 2.0 |
| api-contract-diff | claude | schema-diff | read | 100% | 4985.0 ms | 1155.6 ms | $0.005802 | 2.0 | 3.0 |
| cache-contract | orca | source-doc-consistency | read | 100% | 8614.9 ms | 915.9 ms | $0.016948 | 12.0 | 6.0 |
| cache-contract | pi | source-doc-consistency | read | 67% | 15842.5 ms | 1136.7 ms | $0.039748 | 13.0 | 10.0 |
| cache-contract | omp | source-doc-consistency | read | 100% | 9363.6 ms | 1268.7 ms | $0.021831 | 7.0 | 4.0 |
| cache-contract | claude | source-doc-consistency | read | 0% | 34021.2 ms | 1025.8 ms | $0.062986 | 24.0 | 25.0 |
| component-weights | orca | targeted-retrieval | read | 100% | 2264.3 ms | 709.6 ms | $0.003474 | 1.0 | 2.0 |
| component-weights | pi | targeted-retrieval | read | 100% | 2706.5 ms | 1109.9 ms | $0.004990 | 1.0 | 2.0 |
| component-weights | omp | targeted-retrieval | read | 100% | 3440.8 ms | 1416.8 ms | $0.014066 | 1.0 | 2.0 |
| component-weights | claude | targeted-retrieval | read | 100% | 3683.5 ms | 869.9 ms | $0.004490 | 1.0 | 2.0 |
| dependency-budget | orca | cost-aggregation | read | 100% | 3337.0 ms | 702.7 ms | $0.004590 | 1.0 | 2.0 |
| dependency-budget | pi | cost-aggregation | read | 33% | 3126.4 ms | 1248.4 ms | $0.004963 | 1.0 | 2.0 |
| dependency-budget | omp | cost-aggregation | read | 0% | 3548.5 ms | 1346.0 ms | $0.014362 | 1.0 | 2.0 |
| dependency-budget | claude | cost-aggregation | read | 100% | 4683.0 ms | 1008.6 ms | $0.005506 | 1.0 | 2.0 |
| dependency-order | orca | graph-reasoning | read | 100% | 5686.2 ms | 741.6 ms | $0.005286 | 1.0 | 2.0 |
| dependency-order | pi | graph-reasoning | read | 33% | 4881.4 ms | 1223.4 ms | $0.007521 | 2.0 | 3.0 |
| dependency-order | omp | graph-reasoning | read | 100% | 4039.4 ms | 1317.9 ms | $0.014306 | 1.0 | 2.0 |
| dependency-order | claude | graph-reasoning | read | 100% | 5314.1 ms | 1469.8 ms | $0.005615 | 1.0 | 2.0 |
| feature-precedence | orca | configuration-precedence | read | 100% | 5718.3 ms | 663.6 ms | $0.009485 | 10.0 | 3.0 |
| feature-precedence | pi | configuration-precedence | read | 100% | 12505.9 ms | 1170.7 ms | $0.025648 | 10.0 | 7.0 |
| feature-precedence | omp | configuration-precedence | read | 100% | 6775.0 ms | 1464.4 ms | $0.017618 | 5.0 | 3.0 |
| feature-precedence | claude | configuration-precedence | read | 0% | 45029.2 ms | 1062.6 ms | $0.073057 | 54.0 | 22.0 |
| fix-retry-boundary | orca | single-file-code-edit | edit | 100% | 3837.3 ms | 922.7 ms | $0.006347 | 2.0 | 3.0 |
| fix-retry-boundary | pi | single-file-code-edit | edit | 100% | 4131.0 ms | 1258.4 ms | $0.009471 | 2.0 | 3.0 |
| fix-retry-boundary | omp | single-file-code-edit | edit | 100% | 5230.3 ms | 1386.2 ms | $0.022250 | 2.0 | 3.0 |
| fix-retry-boundary | claude | single-file-code-edit | edit | 67% | 6954.3 ms | 860.4 ms | $0.008970 | 2.0 | 3.0 |
| fix-worker-heartbeat | orca | cross-file-config-edit | edit | 100% | 3719.8 ms | 650.0 ms | $0.006961 | 3.0 | 3.0 |
| fix-worker-heartbeat | pi | cross-file-config-edit | edit | 100% | 5112.3 ms | 1343.8 ms | $0.009962 | 3.0 | 3.0 |
| fix-worker-heartbeat | omp | cross-file-config-edit | edit | 100% | 5484.0 ms | 1629.3 ms | $0.022671 | 3.0 | 3.0 |
| fix-worker-heartbeat | claude | cross-file-config-edit | edit | 100% | 6157.9 ms | 828.2 ms | $0.009254 | 3.0 | 4.0 |
| incident-correlation | orca | log-diagnosis | read | 100% | 5442.0 ms | 687.4 ms | $0.008278 | 3.0 | 4.0 |
| incident-correlation | pi | log-diagnosis | read | 100% | 5567.2 ms | 1132.1 ms | $0.010526 | 3.0 | 4.0 |
| incident-correlation | omp | log-diagnosis | read | 100% | 6026.7 ms | 1435.5 ms | $0.016844 | 3.0 | 3.0 |
| incident-correlation | claude | log-diagnosis | read | 100% | 14248.2 ms | 910.7 ms | $0.025776 | 9.0 | 10.0 |
| invoice-endpoint | orca | cross-format-join | read | 67% | 9000.2 ms | 644.0 ms | $0.015781 | 9.0 | 7.0 |
| invoice-endpoint | pi | cross-format-join | read | 100% | 27244.0 ms | 1207.2 ms | $0.049596 | 33.0 | 21.0 |
| invoice-endpoint | omp | cross-format-join | read | 100% | 5979.5 ms | 1464.8 ms | $0.017206 | 5.0 | 3.0 |
| invoice-endpoint | claude | cross-format-join | read | 0% | 45033.9 ms | 1903.9 ms | $0.067909 | 47.0 | 23.0 |
| invoice-slo | orca | threshold-evaluation | read | 100% | 8971.3 ms | 659.7 ms | $0.014931 | 9.0 | 5.0 |
| invoice-slo | pi | threshold-evaluation | read | 100% | 9050.3 ms | 1235.0 ms | $0.017201 | 9.0 | 5.0 |
| invoice-slo | omp | threshold-evaluation | read | 100% | 11026.4 ms | 1286.5 ms | $0.022503 | 8.0 | 6.0 |
| invoice-slo | claude | threshold-evaluation | read | 0% | 43996.7 ms | 937.1 ms | $0.065730 | 24.0 | 25.0 |
| latest-breaking-change | orca | temporal-reasoning | read | 33% | 3215.2 ms | 886.3 ms | $0.005135 | 2.0 | 3.0 |
| latest-breaking-change | pi | temporal-reasoning | read | 100% | 6351.4 ms | 1315.5 ms | $0.007726 | 3.0 | 3.0 |
| latest-breaking-change | omp | temporal-reasoning | read | 100% | 4611.8 ms | 1344.6 ms | $0.015585 | 2.0 | 3.0 |
| latest-breaking-change | claude | temporal-reasoning | read | 100% | 5936.9 ms | 820.7 ms | $0.007284 | 2.0 | 3.0 |
| retry-budgets | orca | repository-search | read | 100% | 4978.2 ms | 728.7 ms | $0.009492 | 5.0 | 4.0 |
| retry-budgets | pi | repository-search | read | 100% | 4893.2 ms | 2080.4 ms | $0.007970 | 4.0 | 3.0 |
| retry-budgets | omp | repository-search | read | 100% | 5396.1 ms | 1416.7 ms | $0.017211 | 4.0 | 3.0 |
| retry-budgets | claude | repository-search | read | 0% | 35367.3 ms | 1025.5 ms | $0.064771 | 24.0 | 25.0 |
| role-permissions | orca | structured-data-reasoning | read | 33% | 2205.5 ms | 714.4 ms | $0.003624 | 1.0 | 2.0 |
| role-permissions | pi | structured-data-reasoning | read | 33% | 3245.8 ms | 1287.5 ms | $0.005228 | 1.0 | 2.0 |
| role-permissions | omp | structured-data-reasoning | read | 67% | 3777.7 ms | 1268.4 ms | $0.014230 | 1.0 | 2.0 |
| role-permissions | claude | structured-data-reasoning | read | 100% | 4179.6 ms | 1008.3 ms | $0.004624 | 1.0 | 2.0 |
| timeout-delta | orca | cross-file-comparison | read | 100% | 3034.1 ms | 782.4 ms | $0.004270 | 2.0 | 2.0 |
| timeout-delta | pi | cross-file-comparison | read | 100% | 6233.7 ms | 1151.3 ms | $0.011891 | 8.0 | 4.0 |
| timeout-delta | omp | cross-file-comparison | read | 100% | 5087.6 ms | 1280.7 ms | $0.016702 | 4.0 | 3.0 |
| timeout-delta | claude | cross-file-comparison | read | 100% | 26561.9 ms | 1018.0 ms | $0.049871 | 18.0 | 19.0 |
| traffic-aggregate | orca | structured-aggregation | read | 100% | 3216.6 ms | 836.3 ms | $0.004530 | 1.0 | 2.0 |
| traffic-aggregate | pi | structured-aggregation | read | 100% | 2911.3 ms | 1284.1 ms | $0.005152 | 1.0 | 2.0 |
| traffic-aggregate | omp | structured-aggregation | read | 100% | 3924.3 ms | 1697.1 ms | $0.014450 | 1.0 | 2.0 |
| traffic-aggregate | claude | structured-aggregation | read | 100% | 4785.4 ms | 999.1 ms | $0.005490 | 1.0 | 2.0 |
