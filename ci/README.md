# ci

Scripts and workflows for orca-harness.

## Releasing orcacode

The release runs in the only order that keeps GitHub the source of truth:
notes, then the version commit, then the tag. The Makefile wraps
`ci/release.sh`, and each step checks the one before it.

```sh
make notes VERSION=0.3.0       # drafts docs/releases/0.3.0.md from the commits since the last tag
$EDITOR docs/releases/0.3.0.md # write the highlights; the draft groups commits by type
make release VERSION=0.3.0     # prepare (bump, Cargo.lock, commit), push, tag
```

`make release` is `make prepare`, `git push origin HEAD`, and `make tag`;
run them one at a time when you want to look between steps. `make help`
lists everything, including the development targets (`check`, `test`,
`size`, `bench`) that mirror CI.

Pushing the tag runs `.github/workflows/release-orcacode.yml`: it builds the
five targets, writes `SHA256SUMS`, creates the GitHub Release with
`docs/releases/0.3.0.md` as its body and the archives attached, then pushes
the same files to the release host (repository variable `RELEASES_URL`,
secret `RELEASES_ADMIN_TOKEN`). The host also mirrors GitHub Releases on a
schedule when it has a `GITHUB_TOKEN`, so the push only makes the version
available at once.

`prepare` refuses a dirty tree or a notes file that still carries the draft
placeholder line. `tag` refuses a version that does not match `Cargo.toml`,
a tag that already exists, or a `HEAD` that is not on `origin/main`.

### Without CI

On a Mac with `zig`, `cargo-zigbuild`, and the five rust targets installed
(`brew install zig cargo-zigbuild`; `rustup target add <target>`), the same
release can be cut by hand after `tag`:

```sh
make dist VERSION=0.3.0        # all five targets into dist/release/0.3.0 plus SHA256SUMS
make publish VERSION=0.3.0     # GitHub Release from dist, then the host if RELEASES_ADMIN_TOKEN is set
```

`publish` creates the GitHub Release first and pushes to the host second,
never the other way round. Set `RELEASES_URL` to point at a different host.

## Checks

- `check-binary-size.sh <path>`: the release binary must stay under 6,000,000 bytes.
- `check-source-size.sh`: every source file under `crates/` stays below 600 lines.

Both run in the release workflow; `release.sh` runs them locally too.

## Parked workflows

`harness-test.yml` and `harness-bench.yml` are ready-to-use test and
benchmark workflows kept here rather than under `.github/workflows/`. They
still scope their paths to `orca-harness/**` from the monorepo layout; drop
that prefix when moving them into place.
