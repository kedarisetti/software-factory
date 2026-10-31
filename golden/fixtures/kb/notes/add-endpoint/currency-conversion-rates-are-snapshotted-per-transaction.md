---
id: notes/add-endpoint/currency-conversion-rates-are-snapshotted-per-transaction
kind: note
title: Currency conversion rates are snapshotted per transaction
tags:
- money
- fx
- ledger
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

Cross currency transfers store the FX rate and its source timestamp on the transaction so later audits can reproduce amounts. Rates come from a cached provider feed refreshed every minute.