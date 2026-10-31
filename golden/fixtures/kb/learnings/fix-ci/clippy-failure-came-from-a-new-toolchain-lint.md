---
id: learnings/fix-ci/clippy-failure-came-from-a-new-toolchain-lint
kind: learning
title: Clippy failure came from a new toolchain lint
tags:
- clippy
- ci
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

After the toolchain bumped, needless_borrows_for_generic_args fired in the money crate. The right fix was removing the borrow, not adding an allow attribute.