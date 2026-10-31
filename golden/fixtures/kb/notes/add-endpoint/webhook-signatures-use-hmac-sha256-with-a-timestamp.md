---
id: notes/add-endpoint/webhook-signatures-use-hmac-sha256-with-a-timestamp
kind: note
title: Webhook signatures use HMAC-SHA256 with a timestamp
tags:
- webhooks
- security
- hmac
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

Outgoing webhooks are signed over timestamp, a dot, and the raw body. Receivers must reject timestamps older than five minutes. Secrets rotate every 90 days with a ten day overlap.