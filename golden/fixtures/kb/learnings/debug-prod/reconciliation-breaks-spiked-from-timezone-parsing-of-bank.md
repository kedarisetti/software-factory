---
id: learnings/debug-prod/reconciliation-breaks-spiked-from-timezone-parsing-of-bank
kind: learning
title: Reconciliation breaks spiked from timezone parsing of bank files
tags:
- reconciliation
- timezone
- incident
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

One bank switched statement timestamps from UTC to local time at the daylight saving change, shifting transactions across the settlement window. The parser now requires an explicit offset and fails loudly without it.