---
id: pages/webhooks/webhook-delivery-design
kind: page
title: Webhook delivery design
tags:
- webhooks
- events
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

Events are delivered at least once with signed payloads and exponential retries for 24 hours. Customers can replay events from the dashboard. Delivery state is tracked per endpoint.