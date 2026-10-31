---
id: notes/migrate-db/row-level-security-needs-the-tenant-setting-on-every
kind: note
title: Row level security needs the tenant setting on every connection
tags:
- multitenancy
- postgres
- security
task_class: migrate-db
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

Policies read current_setting('app.tenant_id'). The pool hook runs SET LOCAL at the start of each transaction. A query on a fresh connection without it returns zero rows, which looks like missing data.