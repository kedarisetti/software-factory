---
id: conventions/money/money-is-integer-minor-units
kind: convention
title: Money is integer minor units
tags:
- money
- types
- ledger
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

All amounts in ledgerd are i64 minor units (cents) paired with an ISO 4217 currency code. Never use f64 for money, not even in tests or dashboards. The Money newtype in crates/money rejects arithmetic across currencies.