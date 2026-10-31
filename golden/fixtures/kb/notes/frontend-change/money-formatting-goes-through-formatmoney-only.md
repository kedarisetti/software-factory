---
id: notes/frontend-change/money-formatting-goes-through-formatmoney-only
kind: note
title: Money formatting goes through formatMoney only
tags:
- frontend
- money
- i18n
task_class: frontend-change
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

formatMoney in src/lib/money.ts takes minor units and a currency code and uses Intl.NumberFormat. Zero decimal currencies like JPY are handled there. Do not divide by 100 in components.