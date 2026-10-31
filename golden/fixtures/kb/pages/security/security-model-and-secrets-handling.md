---
id: pages/security/security-model-and-secrets-handling
kind: page
title: Security model and secrets handling
tags:
- security
- secrets
- auth
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

Secrets come from the cloud secret manager and are mounted at runtime, never committed. Service to service calls use mTLS; client calls use JWT. Row level security is the last line of defence for tenant isolation.