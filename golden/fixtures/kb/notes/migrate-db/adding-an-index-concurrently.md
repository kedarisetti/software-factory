---
id: notes/migrate-db/adding-an-index-concurrently
kind: note
title: Adding an index concurrently
tags:
- database
- postgres
- index
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

Use CREATE INDEX CONCURRENTLY and mark the migration with the no-transaction directive because concurrent builds cannot run inside a transaction. A failed build leaves an invalid index that must be dropped by hand.