---
id: notes/debug-prod/on-call-runbook-for-a-stuck-outbox-relay
kind: note
title: On-call runbook for a stuck outbox relay
tags:
- runbook
- events
- outbox
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

Symptom: outbox rows older than five minutes with sent_at null. Check the relay pod logs for NATS auth errors, then restart the relay deployment. Rows are safe to re-publish because consumers dedupe on event id.