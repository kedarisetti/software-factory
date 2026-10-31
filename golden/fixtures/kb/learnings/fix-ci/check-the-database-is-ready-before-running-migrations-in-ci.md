---
id: learnings/fix-ci/check-the-database-is-ready-before-running-migrations-in-ci
kind: learning
title: Check the database is ready before running migrations in CI
tags:
- ci
- postgres
- migrations
task_class: fix-ci
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

The postgres service container reports started before it accepts connections. Add pg_isready to a wait loop; otherwise the first migration fails with connection refused roughly one run in ten.