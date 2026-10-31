---
id: learnings/debug-prod/high-p99-was-caused-by-an-unbounded-query-on-the-journal
kind: learning
title: High p99 was caused by an unbounded query on the journal
tags:
- latency
- database
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

An account statement endpoint omitted the created_at bound and scanned all partitions. Adding the date bound and a default thirty day window brought p99 from 4 s to 120 ms.