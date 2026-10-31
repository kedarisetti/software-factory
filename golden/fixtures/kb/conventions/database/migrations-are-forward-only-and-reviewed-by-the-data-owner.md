---
id: conventions/database/migrations-are-forward-only-and-reviewed-by-the-data-owner
kind: convention
title: Migrations are forward only and reviewed by the data owner
tags:
- database
- migrations
- postgres
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

We never write down migrations. A bad migration is fixed with a new migration. Every migration file needs an approval from the data owner on the pull request, and must be safe to run while the old binary is still serving traffic.