---
id: conventions/rust/rust-formatting-and-lints-are-enforced-in-ci
kind: convention
title: Rust formatting and lints are enforced in CI
tags:
- rust
- lint
- ci
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

cargo fmt --check and cargo clippy with warnings denied run on every pull request. Allow attributes need a comment explaining why. The nightly rustfmt options in rustfmt.toml are not used.