---
id: learnings/debug-prod/memory-growth-in-ledgerd-traced-to-an-unbounded-channel
kind: learning
title: Memory growth in ledgerd traced to an unbounded channel
tags:
- memory
- tokio
- performance
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

The webhook dispatcher used an unbounded mpsc channel; a slow customer endpoint let it grow until the pod was OOM killed. Replaced with a bounded channel and drop-oldest backpressure plus a metric.