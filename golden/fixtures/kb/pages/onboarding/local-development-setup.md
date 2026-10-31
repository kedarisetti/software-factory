---
id: pages/onboarding/local-development-setup
kind: page
title: Local development setup
tags:
- onboarding
- tooling
- docker
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

Install the pinned Rust toolchain and Node 20 with pnpm. Run docker compose up for Postgres, Redis and NATS, then cargo xtask test-db and cargo run -p ledgerd. The dashboard starts with pnpm dev in ledger-ui.