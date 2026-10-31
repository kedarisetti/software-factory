---
id: notes/debug-prod/grafana-dashboards-for-ledgerd-latency
kind: note
title: Grafana dashboards for ledgerd latency
tags:
- observability
- metrics
- latency
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

The ledgerd overview dashboard shows p50, p95 and p99 request latency by route. Alert thresholds: p99 above 800 ms for ten minutes pages the on-call. Histogram buckets are defined in telemetry/metrics.rs.