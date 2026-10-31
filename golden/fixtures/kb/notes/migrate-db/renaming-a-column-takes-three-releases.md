---
id: notes/migrate-db/renaming-a-column-takes-three-releases
kind: note
title: Renaming a column takes three releases
tags:
- database
- migrations
- postgres
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

Add the new column and write both, backfill, switch reads, then drop the old column in a later release. A direct ALTER TABLE RENAME breaks the previous binary still running during the rollout.