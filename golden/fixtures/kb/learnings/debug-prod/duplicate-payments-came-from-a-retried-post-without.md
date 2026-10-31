---
id: learnings/debug-prod/duplicate-payments-came-from-a-retried-post-without
kind: learning
title: Duplicate payments came from a retried POST without idempotency key
tags:
- idempotency
- incident
- gateway
task_class: debug-prod
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

A client library retried a timed out POST /transfers and posted twice. Root cause: the key was generated per attempt rather than per logical request. Server now rejects transfers without an Idempotency-Key.