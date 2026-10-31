---
id: notes/upgrade-deps/pinning-the-rust-toolchain-for-reproducible-builds
kind: note
title: Pinning the Rust toolchain for reproducible builds
tags:
- rust
- toolchain
- ci
task_class: upgrade-deps
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

rust-toolchain.toml pins the stable version. Bump it deliberately in its own pull request after running the full test suite, since new clippy lints can fail CI on untouched code.