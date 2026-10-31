---
id: notes/upgrade-deps/upgrading-axum-from-0-6-to-0-7-changes-the-server-api
kind: note
title: Upgrading axum from 0.6 to 0.7 changes the server API
tags:
- axum
- upgrade
- rust
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

axum::Server is gone; bind a tokio TcpListener and call axum::serve. Extractors that consume the body must be last. The hyper 1.0 types replaced those from hyper 0.14 in middleware signatures.