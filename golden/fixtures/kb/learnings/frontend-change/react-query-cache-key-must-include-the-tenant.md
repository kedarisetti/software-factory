---
id: learnings/frontend-change/react-query-cache-key-must-include-the-tenant
kind: learning
title: React Query cache key must include the tenant
tags:
- frontend
- react
- caching
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

Switching tenant in the header showed the previous tenant's balances until refetch because query keys omitted tenant_id. Include it in every key, and clear the cache on tenant switch.