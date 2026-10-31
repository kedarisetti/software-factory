---
id: notes/add-endpoint/reversing-a-posted-transaction
kind: note
title: Reversing a posted transaction
tags:
- ledger
- reversal
- money
task_class: add-endpoint
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

Reversals are new transactions that mirror the original postings with opposite sign and a reverses_id reference. The journal itself is never edited. The reverse endpoint is idempotent on the original id.