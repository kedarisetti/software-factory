---
id: learnings/migrate-db/foreign-key-validation-on-a-big-table-needs-not-valid
kind: learning
title: Foreign key validation on a big table needs NOT VALID
tags:
- postgres
- migrations
- constraints
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

Adding a foreign key to postings scanned the whole table under a lock. Create it NOT VALID and run VALIDATE CONSTRAINT afterwards, which takes a weaker lock.