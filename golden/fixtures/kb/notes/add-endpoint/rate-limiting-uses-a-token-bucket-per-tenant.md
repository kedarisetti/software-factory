---
id: notes/add-endpoint/rate-limiting-uses-a-token-bucket-per-tenant
kind: note
title: Rate limiting uses a token bucket per tenant
tags:
- rate-limit
- api
- tenant
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

The RateLimit middleware keeps a token bucket per tenant in Redis with 100 requests per second burst 200. New endpoints inherit it automatically; heavy endpoints can declare a cost greater than one.