---
id: pages/ledger/ledger-data-model-overview
kind: page
title: Ledger data model overview
tags:
- ledger
- architecture
- domain
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

Accounts hold postings; postings belong to transactions; transactions are written to the journal and never mutated. Balances are derived by summing postings and cached in account_balances, which is rebuilt from the journal if it drifts. See Money is integer minor units and Posting.