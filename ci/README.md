# CI for orca-harness

`harness-test.yml` is a ready-to-use GitHub Actions workflow (fmt, clippy
-D warnings, tests, bench smoke), path-scoped to `orca-harness/**`.

It lives here instead of `.github/workflows/` because the automation that
authored this branch cannot push workflow files (no `workflows`
permission). To enable it:

    git mv orca-harness/ci/harness-test.yml .github/workflows/harness-test.yml
