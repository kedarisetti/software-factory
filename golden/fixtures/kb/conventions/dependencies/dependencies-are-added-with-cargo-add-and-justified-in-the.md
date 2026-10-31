---
id: conventions/dependencies/dependencies-are-added-with-cargo-add-and-justified-in-the
kind: convention
title: Dependencies are added with cargo add and justified in the PR
tags:
- dependencies
- rust
- process
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

A new crate needs a sentence in the pull request: what it does, why std or an existing dependency is not enough, and its license. cargo deny must pass.