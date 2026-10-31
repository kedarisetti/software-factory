---
id: pages/testing/testing-strategy
kind: page
title: Testing strategy
tags:
- testing
- process
- ci
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

Unit tests sit beside the code, integration tests use the template database, and end to end tests run against mocked APIs. Property tests cover the ledger invariants such as postings summing to zero.