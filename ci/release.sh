#!/usr/bin/env bash
# orcacode release commands. Run from anywhere inside the repository, or
# through the Makefile: make notes VERSION=0.3.0, make release VERSION=0.3.0.
#
#   ci/release.sh notes   <version>   draft docs/releases/<version>.md from the commits since the last tag
#   ci/release.sh prepare <version>   bump the workspace version, refresh Cargo.lock, commit with the notes
#   ci/release.sh tag     <version>   tag orcacode-v<version> and push it; CI builds and publishes
#   ci/release.sh build   <version>   local fallback: build all five targets into dist/release/<version>
#   ci/release.sh publish <version>   local fallback: GitHub Release from dist, then the release host
#
# The order is notes, prepare, tag, and the commands enforce it. The GitHub
# Release is the source of truth: the release workflow attaches the binaries
# there and then pushes the same files to the release host, and the host also
# mirrors GitHub Releases on a schedule. build and publish exist for a machine
# without CI; publish keeps the same order, GitHub first and the host second.
set -euo pipefail

PRODUCT=orcacode
TAG_PREFIX="orcacode-v"
REPO_SLUG="okikorg/orca-harness"
HOST_DEFAULT="https://releases-production-7656.up.railway.app"

ASSETS=(
  orcacode-darwin-arm64.tar.gz
  orcacode-darwin-x64.tar.gz
  orcacode-linux-x64.tar.gz
  orcacode-linux-arm64.tar.gz
  orcacode-windows-x64.zip
)

root="$(git rev-parse --show-toplevel)"
cd "$root"

usage() { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-1}"; }
die() { printf 'release: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }

cmd="${1:-}"
version="${2:-}"
[[ -n "$cmd" ]] || usage 1
[[ "$cmd" == "-h" || "$cmd" == "--help" ]] && usage 0
[[ -n "$version" ]] || die "usage: ci/release.sh $cmd <version>   (for example 0.3.0)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "version must look like X.Y.Z, got '$version'"
tag="${TAG_PREFIX}${version}"
notes="docs/releases/${version}.md"
dist="dist/release/${version}"
prerelease=0
[[ "$version" == *-* ]] && prerelease=1

workspace_version() {
  awk '/^\[workspace\.package\]/{f=1; next} /^\[/{f=0} f && /^version *= */{gsub(/.*= *"|".*/, ""); print; exit}' Cargo.toml
}
require_clean() {
  [[ -z "$(git status --porcelain --untracked-files=no)" ]] || die "the working tree has uncommitted changes; commit or stash them first"
}
last_tag() { git tag --list "${TAG_PREFIX}*" --sort=-v:refname | head -n 1; }
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

# ---------------------------------------------------------------------------
cmd_notes() {
  [[ -e "$notes" ]] && die "$notes already exists; edit it, or delete it to draft again"
  local prev range count
  prev="$(last_tag)"
  if [[ -n "$prev" ]]; then range="${prev}..HEAD"; else range="HEAD"; fi
  count="$(git rev-list --count --no-merges "$range")"
  mkdir -p docs/releases

  # One section per conventional-commit type; the type prefix is stripped.
  section() {
    local title="$1" pattern="$2" lines
    lines="$(git log --no-merges --pretty='%s' "$range" | grep -E "$pattern" | sed -E 's/^[a-z]+(\([^)]*\))?!?: //' | sed 's/^/- /' || true)"
    [[ -n "$lines" ]] && printf '## %s\n\n%s\n\n' "$title" "$lines"
    return 0
  }

  {
    printf 'One sentence on what this release is about. Replace this line before running prepare.\n\n'
    section "Highlights" '^feat(\(|:|!)'
    section "Fixes" '^fix(\(|:|!)'
    section "Performance" '^perf(\(|:|!)'
    section "Docs" '^docs(\(|:|!)'
    section "Other changes" '^(refactor|chore|ci|test|build|style)(\(|:|!)|^[A-Z]'
    cat <<'BOILER'
## Install

macOS / Linux:

```sh
curl -fsSL https://orcapods.ai/orcacode.sh | sh
```

Windows (PowerShell):

```powershell
irm https://orcapods.ai/orcacode.ps1 | iex
```

## Assets

| Asset | Platform |
| :-- | :-- |
| orcacode-darwin-arm64.tar.gz | macOS, Apple Silicon |
| orcacode-darwin-x64.tar.gz | macOS, Intel |
| orcacode-linux-x64.tar.gz | Linux x64, static musl |
| orcacode-linux-arm64.tar.gz | Linux arm64, static musl |
| orcacode-windows-x64.zip | Windows x64 |

Verify downloads against `SHA256SUMS`.
BOILER
    if [[ -n "$prev" ]]; then
      printf '\n**Full changelog:** https://github.com/%s/compare/%s...%s\n' "$REPO_SLUG" "$prev" "$tag"
    fi
  } > "$notes"
  say "drafted $notes from $count commits (${range})"
  say "edit it, then: ci/release.sh prepare $version"
}

# ---------------------------------------------------------------------------
cmd_prepare() {
  require_clean
  [[ -f "$notes" ]] || die "$notes is missing; run: ci/release.sh notes $version"
  grep -q 'Replace this line before running prepare' "$notes" && die "$notes still has the draft placeholder line; edit it first"
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists"
  local branch current
  branch="$(git rev-parse --abbrev-ref HEAD)"
  current="$(workspace_version)"
  [[ "$branch" == "main" ]] || say "note: preparing on branch $branch, not main"

  if [[ "$current" != "$version" ]]; then
    perl -0pi -e 's/(\[workspace\.package\]\nversion = ")[^"]+(")/${1}'"$version"'${2}/' Cargo.toml
    [[ "$(workspace_version)" == "$version" ]] || die "could not set the version under [workspace.package] in Cargo.toml"
  fi
  # Refresh the workspace crates in Cargo.lock without touching dependencies.
  cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null
  cargo check --workspace --quiet
  ./ci/check-source-size.sh

  git add Cargo.toml Cargo.lock "$notes"
  git commit -q -m "chore(release): $PRODUCT v$version"
  say "committed: chore(release): $PRODUCT v$version"
  say "next: git push origin $branch && ci/release.sh tag $version"
}

# ---------------------------------------------------------------------------
cmd_tag() {
  require_clean
  local current
  current="$(workspace_version)"
  [[ "$current" == "$version" ]] || die "Cargo.toml says $current, not $version; run: ci/release.sh prepare $version"
  [[ -f "$notes" ]] || die "$notes is missing; the release body comes from it"
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists"
  git fetch -q origin
  git merge-base --is-ancestor HEAD origin/main || die "HEAD is not on origin/main; push it first (git push origin HEAD)"
  if [[ ! -f .github/workflows/release-orcacode.yml ]]; then
    say "warning: no release workflow at HEAD, so this tag builds nothing; use build and publish instead"
  fi
  git tag -a "$tag" -m "$PRODUCT v$version"
  git push origin "$tag"
  say "pushed $tag; the release workflow builds, attaches, and pushes to the host"
  say "watch:   gh run list -R $REPO_SLUG --workflow release-orcacode --limit 1"
  say "release: https://github.com/$REPO_SLUG/releases/tag/$tag"
}

# ---------------------------------------------------------------------------
cmd_build() {
  local current
  current="$(workspace_version)"
  [[ "$current" == "$version" ]] || die "Cargo.toml says $current, not $version; run: ci/release.sh prepare $version"
  [[ -z "$(git status --porcelain --untracked-files=no)" ]] || say "warning: building from a dirty tree; the tag will not reproduce these bytes"
  [[ "$(uname -s)" == "Darwin" ]] || die "build runs on macOS, where the Apple targets have a toolchain; elsewhere, push a tag and let CI build"
  command -v cargo-zigbuild >/dev/null 2>&1 && command -v zig >/dev/null 2>&1 || die "cargo-zigbuild and zig are needed for the Linux and Windows targets: brew install zig cargo-zigbuild"
  local t
  for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-pc-windows-gnu; do
    rustup target list --installed | grep -qx "$t" || die "rust target $t is not installed: rustup target add $t"
  done

  rm -rf "$dist"
  mkdir -p "$dist"
  export COPYFILE_DISABLE=1 MACOSX_DEPLOYMENT_TARGET=11.0

  # target, asset, zigbuild (1 or 0). Each archive holds one file at its root.
  build_one() {
    local target="$1" asset="$2" zig="$3" out="target/$1/release"
    say ">>> $target"
    if [[ "$zig" == 1 ]]; then
      cargo zigbuild --release -p "$PRODUCT" --target "$target"
    else
      cargo build --release -p "$PRODUCT" --target "$target"
    fi
    case "$asset" in
      *.tar.gz) tar -C "$out" -czf "$dist/$asset" "$PRODUCT" ;;
      *.zip) (cd "$out" && zip -q -j "$root/$dist/$asset" "$PRODUCT.exe") ;;
    esac
    say "packed $asset"
  }
  build_one aarch64-apple-darwin      orcacode-darwin-arm64.tar.gz 0
  build_one x86_64-apple-darwin       orcacode-darwin-x64.tar.gz   0
  build_one x86_64-unknown-linux-musl orcacode-linux-x64.tar.gz    1
  build_one aarch64-unknown-linux-musl orcacode-linux-arm64.tar.gz 1
  build_one x86_64-pc-windows-gnu     orcacode-windows-x64.zip     1

  ./ci/check-binary-size.sh target/aarch64-apple-darwin/release/orcacode
  ./ci/check-binary-size.sh target/x86_64-unknown-linux-musl/release/orcacode
  (cd "$dist" && LC_ALL=C sha256 orcacode-*.tar.gz orcacode-*.zip | LC_ALL=C sort -k 2 > SHA256SUMS && cat SHA256SUMS)
  say "built $dist"
  say "next: ci/release.sh publish $version"
}

# ---------------------------------------------------------------------------
cmd_publish() {
  [[ -f "$notes" ]] || die "$notes is missing; the release body comes from it"
  local f
  for f in "${ASSETS[@]}" SHA256SUMS; do
    [[ -f "$dist/$f" ]] || die "$dist/$f is missing; run: ci/release.sh build $version"
  done
  (cd "$dist" && sha256 -c SHA256SUMS >/dev/null) || die "the files in $dist do not match SHA256SUMS"
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "tag $tag does not exist; run: ci/release.sh tag $version"
  git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1 || die "tag $tag is not on origin; push it first"

  # 1. GitHub Release, the source of truth.
  local paths=()
  for f in "${ASSETS[@]}" SHA256SUMS; do paths+=("$dist/$f"); done
  if gh release view "$tag" -R "$REPO_SLUG" >/dev/null 2>&1; then
    gh release upload "$tag" -R "$REPO_SLUG" --clobber "${paths[@]}"
    say "replaced the assets on the existing release $tag"
  else
    local flags=(--title "$PRODUCT v$version" --notes-file "$notes")
    if [[ "$prerelease" == 1 ]]; then flags+=(--prerelease); else flags+=(--latest); fi
    gh release create "$tag" -R "$REPO_SLUG" "${flags[@]}" "${paths[@]}"
    say "created https://github.com/$REPO_SLUG/releases/tag/$tag"
  fi

  # 2. The release host, only once GitHub has it.
  local url="${RELEASES_URL:-$HOST_DEFAULT}" token="${RELEASES_ADMIN_TOKEN:-}"
  if [[ -z "$token" ]]; then
    say "RELEASES_ADMIN_TOKEN is not set, so the host was not pushed; it mirrors GitHub on its next poll when GITHUB_TOKEN is set there"
    return 0
  fi
  local published
  published="$(gh release view "$tag" -R "$REPO_SLUG" --json publishedAt -q .publishedAt)"
  for f in "${ASSETS[@]}" SHA256SUMS; do
    curl -fsS --http1.1 -o /dev/null -X PUT \
      -H "Authorization: Bearer $token" -H "Content-Type: application/octet-stream" \
      --data-binary "@$dist/$f" "$url/admin/$PRODUCT/$tag/$f"
    say "  pushed $f"
  done
  curl -fsS --http1.1 -o /dev/null -X POST -H "Authorization: Bearer $token" \
    "$url/admin/$PRODUCT/$tag/verify?publishedAt=$published" \
    || die "the host did not verify $tag; a file is missing or its hash differs"
  say "host serves $version: $url/$PRODUCT/latest.json"
}

# ---------------------------------------------------------------------------
case "$cmd" in
  notes)   cmd_notes ;;
  prepare) cmd_prepare ;;
  tag)     cmd_tag ;;
  build)   cmd_build ;;
  publish) cmd_publish ;;
  *) die "unknown command '$cmd' (notes, prepare, tag, build, publish)" ;;
esac
