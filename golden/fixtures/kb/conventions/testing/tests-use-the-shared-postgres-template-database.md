---
id: conventions/testing/tests-use-the-shared-postgres-template-database
kind: convention
title: Tests use the shared postgres template database
tags:
- testing
- postgres
- database
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

Integration tests clone the template database created by xtask test-db instead of running migrations per test. A clone takes about 40 ms. Never point tests at the shared dev database.