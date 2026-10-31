---
id: notes/add-endpoint/authentication-is-a-jwt-checked-by-the-authlayer
kind: note
title: Authentication is a JWT checked by the AuthLayer
tags:
- auth
- jwt
- api
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

Requests carry a bearer JWT signed by the identity service. The AuthLayer validates signature, expiry and audience, then inserts Claims into request extensions. Handlers read tenant_id from Claims, never from the body.