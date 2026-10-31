---
id: notes/review-pr/review-checklist-for-money-moving-changes
kind: note
title: Review checklist for money-moving changes
tags:
- review
- money
- process
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

Check that postings sum to zero, the idempotency key is honoured, amounts are minor units, and a test covers the reversal path. Money-moving changes need two approvals.