---
id: learnings/fix-ci/out-of-memory-in-ci-linking-the-test-binaries
kind: learning
title: Out of memory in CI linking the test binaries
tags:
- ci
- memory
- linker
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

Linking many integration test binaries in parallel exhausted the 7 GB runner. Setting CARGO_BUILD_JOBS=2 and using lld fixed the killed-by-signal-9 failures.