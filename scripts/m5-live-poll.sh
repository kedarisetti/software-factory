#!/usr/bin/env bash
# Milestone 5 live check: poll the real repo with the gh-issues preset into a temp db and print
# the queue. Read-only: one GET via `gh api`; nothing is created or written on GitHub.
# Needs: gh (authenticated).
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cargo build -q -p sf-cli --manifest-path "$repo/Cargo.toml"
sf="$repo/target/debug/sf"
cfg="$work/intake.json"
cp "$repo/docs/intake.example.json" "$cfg"

export SF_GH_TOKEN="${SF_GH_TOKEN:-$(gh auth token)}"
run() { "$sf" --db "$work/sf.db" --sandbox "$work/sbx" "$@"; }

echo "== approve the gh-issues command source (it runs gh)"
run intake connectors approve gh-issues --as "$(whoami)" --config "$cfg"
echo "== poll kedarisetti/software-factory"
run intake poll --source gh-issues --now --config "$cfg"
echo "== queue"
run intake list
