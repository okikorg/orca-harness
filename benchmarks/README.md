# Benchmarks

Two suites, because the harness has two costs worth defending:

| Suite      | Script                    | Measures                                                     |
| :--------- | :------------------------ | :----------------------------------------------------------- |
| **kernel** | `./benchmarks/kernel.sh`  | overhead between a model emitting tool calls and tools running |
| startup    | `./benchmarks/startup.sh` | fixed cost the `orcacode` host pays before accepting input     |

Plus `./benchmarks/compare.sh`, which puts orcacode next to `fx` on the
same host — see [Comparing against fx](#comparing-against-fx), and read it
before quoting a number from it.

The kernel suite is the one that matters. The harness exists to add as
little as possible between the model and the work; startup is a host
concern that is measured here so it cannot quietly grow.

Both write machine-readable results under `results/<suite>/summary.json`
in the shape `github-action-benchmark`'s `customSmallerIsBetter` consumes,
and both end with a budget check that fails the run on regression.

## Running them

```bash
./benchmarks/kernel.sh                # dispatch probes + real-tool probe
./benchmarks/kernel.sh --quick        # fewer iterations, probes only
./benchmarks/kernel.sh --criterion    # also run cargo bench and record it

./benchmarks/startup.sh               # 100 runs per command
./benchmarks/startup.sh --quick       # 20 runs
```

`startup.sh` needs [hyperfine](https://github.com/sharkdp/hyperfine)
(`brew install hyperfine` / `apt install hyperfine` / `cargo install
hyperfine`). Everything else needs only `python3` and the Rust toolchain.

The reporting layer has its own tests:

```bash
python3 benchmarks/check_budgets_test.py
python3 benchmarks/kernel_report_test.py
```

## The kernel suite

`kernel.sh` runs `examples/fanout_probe` at batch sizes 1, 10 and 100 with
no-op tools — so the numbers are pure harness overhead — and then
`examples/tool_fanout_perf`, which drives the same dispatcher through the
real `write_file`, `read_file` and `shell` tools.

The probe reports three timestamps per batch: T0 dispatch entry, T1 first
tool body started, T2 last tool body started.

Measured on an Apple Silicon Mac, release build, 2 000 iterations:

| Metric                     |     p50 |     p99 |    gate |
| :------------------------- | ------: | ------: | ------: |
| 1 call, dispatch (T1−T0)   |  0.8 µs |  1.0 µs |   20 µs |
| 10 calls, dispatch (T1−T0) |  4.0 µs |  6.5 µs |   50 µs |
| 10 calls, fan-out (T2−T0)  |   13 µs |   22 µs |  150 µs |
| 100 calls, fan-out (T2−T0) |  158 µs |  273 µs |    1 ms |
| 100 calls, round-trip      |  172 µs |  292 µs |  1.5 ms |

Every gated metric is a p99, so the iteration count is what makes it a
percentile instead of the second-worst sample: 2 000 by default, 5 000
under `--ci`, where the runner is shared and noisier. Each iteration costs
microseconds, so this is nearly free.

Real tools, same run: 100 concurrent `write_file` in 2.9 ms, 100
`read_file` in 1.1 ms, and 64 × 20 ms subprocesses in 66 ms — 19× faster
than running them serially. Those are reported, not gated: they measure
the machine's filesystem and process spawn as much as the harness.

`--criterion` additionally runs `cargo bench --workspace` and folds
criterion's own estimates into the summary, so the microbenchmarks in
`crates/*/benches/` produce trend data instead of only local HTML. It
reads whatever `target/criterion` holds — which is why it runs the full
workspace bench first rather than trusting what is already there.

## The startup suite

`startup.sh` benchmarks the `orcacode` binary against a fixture tree built
by `fixtures.py` — a private HOME, config dir, sessions and skills — so the
numbers do not depend on what the developer happens to have installed.

| Command                           | What it exercises                                                |
| :-------------------------------- | :--------------------------------------------------------------- |
| `process baseline`                | `/usr/bin/true`: the process-launch floor on this host           |
| `orcacode --help`                 | image load, dynamic linking, tokio runtime construction          |
| `orcacode (startup)`              | config load, six skill roots scanned, prompt, registries, agent  |
| `orcacode (startup, new session)` | the same, plus creating a session file and writing its header    |
| `orcacode (startup, skills)`      | the same, against a workspace holding 64 skills                  |
| `orcacode (resume)`               | `--continue`: list 32 sessions, replay a 2 000-message transcript |

Measured on the same machine, 100 runs each:

| Command                           |   mean | median |    min | budget |
| :-------------------------------- | -----: | -----: | -----: | -----: |
| process baseline                  | 1.05ms | 1.02ms | 0.96ms |      — |
| orcacode --help                   | 2.44ms | 2.42ms | 2.14ms |   10ms |
| orcacode (startup)                | 2.79ms | 2.74ms | 2.42ms |   12ms |
| orcacode (startup, new session)   | 3.13ms | 3.07ms | 2.73ms |   12ms |
| orcacode (startup, skills)        | 4.64ms | 4.57ms | 4.21ms |   16ms |
| orcacode (resume)                 | 4.89ms | 4.71ms | 4.26ms |   20ms |

Roughly 1.7 ms of real work over the process floor for a cold start; 64
skills cost another 1.9 ms, and resuming a 2 000-message transcript 2.1 ms.

### The `ORCA_BENCH` hook

`orcacode` is a TUI: normally it never exits on its own. `ORCA_BENCH=1`
makes it stop at the end of `run_mode` in `crates/cli/src/main.rs`, right
after the agent is built and just before the terminal is claimed. That is
the entire cold-start path and nothing else — no model call, no TTY. The
variable is read in exactly one place and does nothing else.

### Fixture isolation

`config_path()` resolves `ORCA_CONFIG_DIR`, then `XDG_CONFIG_HOME`, then
`HOME/.config`, and skills discovery reads `HOME` directly for its
`~/.claude/skills` compatibility roots. `startup.sh` therefore pins
`ORCA_CONFIG_DIR` *and* `HOME`, unsets `XDG_CONFIG_HOME`, and unsets every
API-key variable so provider resolution cannot differ between machines.

Two rules keep the measurements honest, and both are load-bearing:

- **The resume fixture is never written.** Its transcripts end on complete
  user/assistant/tool triplets, so `SessionHandler::resume` finds nothing
  to repair and opens the file for append instead of rewriting it.
- **The session-creating run gets its own workspace.** It writes a session
  file every run, so hyperfine's `--prepare` wipes that workspace's session
  directory between runs — never the directory the resume benchmark reads.

## Comparing against fx

`compare.sh` runs orcacode and [fx](https://github.com/vercel-labs/fx)
side by side on the same host, from the same shell, each with its own
fixture home, against the same `/usr/bin/true` baseline.

```bash
./benchmarks/compare.sh                          # whatever `fx` is on PATH
FX_BIN=/path/to/fx ./benchmarks/compare.sh       # a specific build
```

**The obvious pairing is wrong, and the script exists partly to say so.**
Both binaries have a benchmark environment variable, but they stop at
different places:

| Hook                    | Stops after                                                        |
| :---------------------- | :----------------------------------------------------------------- |
| `ORCA_BENCH=1 orcacode` | config, six skill roots, system prompt, registries, agent build     |
| `FX_BENCH=1 fx`         | argv parsed — no settings read at all                              |

That is not a guess: point fx at a corrupt `settings.json` and
`FX_BENCH=1 fx` still exits 0, while `fx status --json` reports
`malformed_settings`. In fx's source it is `shouldRunBenchmarkNoArgRaw`
(`src/main.zig`), which calls `cli_surface.parse` and `exitFast(0)`.
Pairing the two hooks compares orcacode's entire startup against fx's
argument parser, and makes fx look roughly 2.5× faster than anything
measured here supports.

So commands are grouped by the work they do. Measured on an Apple Silicon
Mac, 100 runs, orcacode release (fat LTO, `codegen-units=1`, stripped)
against fx `main` built `-Doptimize=ReleaseFast` (fx's `build.zig` strips
at any non-Debug optimize level). Work above a 1.22 ms process floor:

| Tier                              | Command             |    work |
| :-------------------------------- | :------------------ | ------: |
| argv parsed, no config read       | `fx (arg parse)`    | 0.65 ms |
|                                   | `orcacode --help`   | 0.97 ms |
| settings loaded, filesystem, exit | `fx status --json`  | 55.7 ms |
|                                   | `fx doctor --json`  | 54.0 ms |
|                                   | `orcacode (startup)`| 1.78 ms |

Read tier 2 carefully. fx has no command that exits after a full
interactive-launch startup, so `status`/`doctor` stand in — and they probe
auth and the system, which orcacode's startup does not. fx's own
`check_budgets.py` evaluates those commands on Linux only; on macOS they
cost ~50 ms here, ~35 ms of it in-process CPU rather than I/O. It is a data
point about those commands, not a verdict about startup.

The defensible summary: **at the argument-parsing floor fx is ~0.3 ms
cheaper; orcacode's entire cold start is 1.78 ms of work; fx has no
directly comparable full-startup number.**

Binary size, both current `main`, both stripped and optimized for speed:

| Binary                   |   size |
| :----------------------- | -----: |
| orcacode                 | 8.2 MB |
| fx (built here)          | 11.6 MB |
| fx 0.0.5 (as shipped)    | 6.4 MB |

The shipped 0.0.5 is an older, smaller fx — not a different build mode.
Building fx from source needs Zig 0.16 (`minimum_zig_version` in
`build.zig.zon`).

## Budgets

`check_budgets.py` gates the mean of each startup command and the p99 of
each kernel metric. Ceilings live at the top of that file.

They are enforced on **Linux only** — that is what CI runs on — and are
informational elsewhere. `ORCA_BENCH_ENFORCE=1` forces the gate anywhere.

The ceilings sit 3–10× above the measured numbers on purpose. A shared CI
runner cannot honestly measure 20% drift, so these catch the regressions
that matter: a blocking call added to the dispatch path, a directory walk
added to startup. Use the trend chart for drift, the gate for cliffs.

To re-baseline after an intentional change: run both suites on a quiet
machine, update the budget tables in `check_budgets.py` and the measured
tables above.

## Files

```text
benchmarks/
├── startup.sh              hyperfine suite over the orcacode binary
├── kernel.sh               dispatch and real-tool probes
├── compare.sh              orcacode vs fx, same host, same baseline
├── fixtures.py             hermetic HOME / config / sessions / skills tree
├── summarize.py            hyperfine JSON → table + summary.json
├── kernel_report.py        probe stdout + criterion → table + summary.json
├── compare_report.py       the comparison, grouped by work done
├── check_budgets.py        the gate, for both suites
├── check_budgets_test.py   tests for the gate
├── kernel_report_test.py   tests for the probe parser
└── results/                generated; git-ignored except .gitkeep
```
