---
id: notes/migrate-db/backfills-run-as-chunked-jobs-not-one-big-statement
kind: note
title: Backfills run as chunked jobs not one big statement
tags:
- database
- migrations
- backfill
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

Large backfills update at most 5000 rows per batch, sleep between batches and record progress in backfill_state. A single UPDATE across the journal held locks for minutes during the March incident.