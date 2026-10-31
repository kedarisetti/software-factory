---
id: learnings/migrate-db/migration-locked-the-accounts-table-during-deploy
kind: learning
title: Migration locked the accounts table during deploy
tags:
- migrations
- postgres
- incident
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

Adding a NOT NULL column with a default rewrote the table on Postgres 11 and blocked writes for 90 seconds. Add the column nullable, backfill in chunks, then add the constraint as NOT VALID and validate separately.