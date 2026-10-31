---
id: notes/debug-prod/connection-pool-sizing-for-ledgerd
kind: note
title: Connection pool sizing for ledgerd
tags:
- database
- postgres
- performance
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

The sqlx pool is capped at 20 connections per pod and pgbouncer fronts the database in transaction mode. Prepared statements are disabled for that reason. Raising the pool beyond 20 starved the settlement worker.