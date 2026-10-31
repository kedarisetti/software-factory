---
id: pages/multitenancy/multitenancy-design
kind: page
title: Multitenancy design
tags:
- multitenancy
- security
- architecture
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

Shared schema with tenant_id on every row, enforced by row level security and by an AuthLayer that sets the tenant on the connection. Noisy tenants are contained by per tenant rate limits.