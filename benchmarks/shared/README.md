# Shared benchmark support

Utilities used by more than one benchmark suite:

- `fixtures.py` — creates isolated HOME, config, sessions, workspaces, and skills.
- `summarize.py` — converts Hyperfine JSON into benchmark-action summary JSON.
- `check_budgets.py` — applies startup and kernel regression ceilings.
- `check_budgets_test.py` — tests gate behavior and exit-code contracts.

Run shared tests with:

```bash
python3 benchmarks/shared/check_budgets_test.py
```

Suite-specific probes, runners, parsers, and tests belong in their own sibling
folder rather than here.
