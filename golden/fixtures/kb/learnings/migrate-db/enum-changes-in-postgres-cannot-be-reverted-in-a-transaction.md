---
id: learnings/migrate-db/enum-changes-in-postgres-cannot-be-reverted-in-a-transaction
kind: learning
title: Enum changes in Postgres cannot be reverted in a transaction
tags:
- postgres
- enum
- migrations
task_class: migrate-db
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

ALTER TYPE ADD VALUE cannot be used in the same transaction as a statement that uses the new value. Split into two migrations. Prefer a lookup table for values that change often.