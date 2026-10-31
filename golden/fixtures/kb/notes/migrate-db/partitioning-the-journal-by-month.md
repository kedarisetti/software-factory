---
id: notes/migrate-db/partitioning-the-journal-by-month
kind: note
title: Partitioning the journal by month
tags:
- database
- postgres
- partitioning
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

The journal is range partitioned by created_at month. A cron creates next month's partition seven days ahead. Queries without a created_at bound scan every partition and should be avoided.