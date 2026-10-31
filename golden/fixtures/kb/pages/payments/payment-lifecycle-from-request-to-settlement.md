---
id: pages/payments/payment-lifecycle-from-request-to-settlement
kind: page
title: Payment lifecycle from request to settlement
tags:
- payments
- settlement
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

A transfer request is validated, a hold is placed, the transaction is posted to the journal, an event goes through the outbox, and the next settlement window batches it for the bank gateway. Failures at the gateway trigger the retry policy and, finally, a reversal.