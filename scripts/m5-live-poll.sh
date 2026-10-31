#!/usr/bin/env bash
# Milestone 5 live check: poll the real repo for issues labelled "factory" into a temp db and print
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

echo "== poll kedarisetti/software-factory (label factory)"
"$sf" --db "$work/sf.db" --sandbox "$work/sbx" intake poll github --repo kedarisetti/software-factory --label factory --config "$cfg"
echo "== queue"
"$sf" --db "$work/sf.db" --sandbox "$work/sbx" intake list
