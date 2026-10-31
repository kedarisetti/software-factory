---
id: learnings/debug-prod/dead-letter-queue-filled-after-a-schema-change
kind: learning
title: Dead letter queue filled after a schema change
tags:
- events
- nats
- incident
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

A consumer deserialized events with deny_unknown_fields, so adding a field to the payload sent every message to the dead letter queue. Consumers must ignore unknown fields.