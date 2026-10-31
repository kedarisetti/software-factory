---
id: glossary/api/idempotency-key
kind: glossary
title: Idempotency key
tags:
- api
- idempotency
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

A client-supplied opaque string in the Idempotency-Key header. Replaying a request with the same key within 24 hours returns the stored response instead of posting again.