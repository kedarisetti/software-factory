---
id: pages/database/database-operations-overview
kind: page
title: Database operations overview
tags:
- database
- postgres
- operations
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

Postgres 15 with streaming replicas and pgbouncer. The journal is partitioned monthly. Read-only reports use the replica. Backups are daily with point-in-time recovery for seven days.