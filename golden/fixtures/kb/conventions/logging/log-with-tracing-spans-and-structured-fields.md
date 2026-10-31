---
id: conventions/logging/log-with-tracing-spans-and-structured-fields
kind: convention
title: Log with tracing spans and structured fields
tags:
- logging
- tracing
- observability
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

Use tracing, not println or log. Put request_id and account_id on the root span. Do not log full card numbers, bearer tokens or customer email addresses; log the last four digits only.