---
id: glossary/events/outbox
kind: glossary
title: Outbox
tags:
- events
- messaging
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

The outbox table stores events written in the same database transaction as the state change. A relay process publishes them to NATS and marks them sent.