---
id: notes/debug-prod/retry-policy-for-the-bank-gateway-client
kind: note
title: Retry policy for the bank gateway client
tags:
- gateway
- retries
- http
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

Calls to the bank gateway retry three times with exponential backoff starting at 200 ms and full jitter. Only 429, 502, 503 and connection resets retry. A 400 is never retried because the request itself is wrong.