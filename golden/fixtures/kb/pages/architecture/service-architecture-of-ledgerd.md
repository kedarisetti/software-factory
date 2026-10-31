---
id: pages/architecture/service-architecture-of-ledgerd
kind: page
title: Service architecture of ledgerd
tags:
- architecture
- rust
- overview
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

ledgerd is an axum HTTP service backed by Postgres through sqlx, with a Redis rate limiter, a NATS event bus, and a settlement worker run as a separate deployment. ledger-ui is a React dashboard that talks to the public API only.