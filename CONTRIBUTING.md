# Contributing to Orca Harness

Contributions are welcome: bug reports, docs fixes, new providers, tools and
extensions.

## Workflow

1. Open an [issue](https://github.com/okikorg/orca-harness/issues) to report a
   bug or discuss a change before you build it.
2. Fork, branch, and keep the change focused. Match the surrounding code and
   add tests for behavior you change.
3. Run `cargo test --workspace`, `cargo clippy --workspace --all-targets` and
   `cargo fmt --all` before opening a pull request.

## Using AI tools and agents

Using AI coding tools and agents (Claude Code, Orcacode, Copilot, Cursor and
similar) is fine for most contributions, including providers, tools,
extensions, the CLI, workflows, tests and docs. The usual bar still applies:
you are the author of the pull request, so read and understand every line you
submit, and make sure it builds and passes the checks above.

### Exception: `crates/harness-core`

`crates/harness-core` is the kernel every other crate depends on. Changes to
it need human review:

- A human must review and understand every change to `crates/harness-core`
  before it is submitted. Do not open a pull request with agent-written core
  changes that nobody has read.
- Say in the pull request description when AI tools were used for core
  changes, and what you checked by hand.
- Core changes are reviewed by a maintainer before merge, regardless of how
  they were written.
- If you are running an agent on this repo, configure it to ask before editing
  `crates/harness-core`.

## License

By contributing you agree that your contributions are licensed under the
Apache License 2.0.
