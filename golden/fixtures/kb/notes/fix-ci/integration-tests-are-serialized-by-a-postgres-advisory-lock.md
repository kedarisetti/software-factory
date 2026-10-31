---
id: notes/fix-ci/integration-tests-are-serialized-by-a-postgres-advisory-lock
kind: note
title: Integration tests are serialized by a postgres advisory lock
tags:
- testing
- postgres
- ci
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

Tests that touch the settlement tables take advisory lock 7001 so they do not interleave. If a test panics while holding the lock the connection drops and the lock releases, so a hung CI job usually means a different cause.