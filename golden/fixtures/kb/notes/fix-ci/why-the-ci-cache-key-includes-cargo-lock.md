---
id: notes/fix-ci/why-the-ci-cache-key-includes-cargo-lock
kind: note
title: Why the CI cache key includes Cargo.lock
tags:
- ci
- cache
- rust
task_class: fix-ci
source: agent:golden/seed
author:
  agent: golden
  session: seed
created_at: 1767225600000
updated_at: 1767225600000
status: active
count: 1
helped: 0
misled: 0
---

The GitHub Actions cache for target and the cargo registry is keyed on the hash of Cargo.lock plus the toolchain version. Keying on Cargo.toml alone served stale artifacts after a transitive bump and produced linker errors.