# orca-harness
#
#   make help                     every target
#   make check test               what CI runs before a merge
#   make notes VERSION=0.3.0      start a release (then edit docs/releases/0.3.0.md)
#   make release VERSION=0.3.0    prepare, push, tag: CI builds and publishes
#
# Release targets wrap ci/release.sh and take VERSION=X.Y.Z.

SHELL := /bin/bash
.DEFAULT_GOAL := help

VERSION ?=
require_version = $(if $(VERSION),,$(error VERSION is required, for example: make $@ VERSION=0.3.0))

.PHONY: help build test fmt clippy audit check size bench bench-all notes prepare tag release dist publish

help:  ## list targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-9s %s\n", $$1, $$2}'

## Development

build:  ## release build of orcacode
	cargo build --release -p orcacode

test:  ## the workspace test suite plus the benchmark reporting tests, as CI runs them
	cargo test --workspace
	./benchmarks/run_reporting_tests.sh

fmt:  ## rustfmt check, as CI runs it
	cargo fmt --all --check

clippy:  ## clippy with warnings denied, as CI runs it
	cargo clippy --workspace --all-targets -- -D warnings

audit:  ## source-size limit: the gate, then the largest files
	./ci/check-source-size.sh
	python3 scripts/audit_crates.py --top 25

check: fmt audit clippy  ## what CI checks before the tests: fmt, source size, clippy

size: build  ## the release binary against ci/check-binary-size.sh
	./ci/check-binary-size.sh

bench:  ## benchmark smoke, as CI runs it
	cargo bench --workspace -- --test

bench-all: build  ## every suite bench.yml runs, budgets informational off Linux
	cargo build --release -p orca-harness-core --example fanout_probe
	cargo build --release -p orca-harness-tools --example tool_fanout_perf
	./benchmarks/kernel/run.sh --ci --criterion
	./benchmarks/startup/run.sh --ci
	./benchmarks/core-tools/run.sh
	python3 -m unittest discover -s benchmarks/mcp -p '*_test.py'
	python3 benchmarks/mcp/accuracy.py
	python3 benchmarks/mcp/distribution.py
	python3 benchmarks/mcp/pareto.py
	./benchmarks/subagent/run.sh --quick
	./benchmarks/subagent/sim.sh --quick

## Release: notes, edit the file, prepare, push, tag (VERSION=X.Y.Z)

notes:  ## draft docs/releases/VERSION.md from the commits since the last tag
	$(require_version)
	./ci/release.sh notes $(VERSION)

prepare:  ## bump the workspace version, refresh Cargo.lock, commit with the notes
	$(require_version)
	./ci/release.sh prepare $(VERSION)

tag:  ## tag orcacode-vVERSION and push it; CI builds and publishes
	$(require_version)
	./ci/release.sh tag $(VERSION)

release: prepare  ## prepare, push, tag in one go, once the notes are written
	git push origin HEAD
	./ci/release.sh tag $(VERSION)

dist:  ## no-CI fallback: build all five targets into dist/release/VERSION
	$(require_version)
	./ci/release.sh build $(VERSION)

publish:  ## no-CI fallback: GitHub Release from dist, then the release host
	$(require_version)
	./ci/release.sh publish $(VERSION)
