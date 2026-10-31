---
id: notes/review-pr/what-reviewers-look-for-in-sql-changes
kind: note
title: What reviewers look for in SQL changes
tags:
- review
- database
- sql
task_class: review-pr
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

Ask for EXPLAIN output on new queries over the journal, confirm tenant_id appears in the predicate, and verify that new indexes are built concurrently.