---
id: notes/upgrade-deps/sqlx-0-7-to-0-8-requires-regenerating-offline-data
kind: note
title: sqlx 0.7 to 0.8 requires regenerating offline data
tags:
- sqlx
- upgrade
- database
task_class: upgrade-deps
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

The offline query format changed, so delete .sqlx and run cargo sqlx prepare again. The Executor trait bounds also changed, which breaks generic repository helpers.