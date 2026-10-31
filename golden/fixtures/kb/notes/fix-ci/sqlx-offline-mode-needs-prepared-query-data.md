---
id: notes/fix-ci/sqlx-offline-mode-needs-prepared-query-data
kind: note
title: sqlx offline mode needs prepared query data
tags:
- sqlx
- ci
- database
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

CI builds with SQLX_OFFLINE=true, so the .sqlx directory must be regenerated with cargo sqlx prepare whenever a query! macro changes. The error is 'set DATABASE_URL or run cargo sqlx prepare'.