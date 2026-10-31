---
id: learnings/upgrade-deps/tower-layer-ordering-changed-after-the-axum-upgrade
kind: learning
title: tower layer ordering changed after the axum upgrade
tags:
- axum
- middleware
- upgrade
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

Layers added with ServiceBuilder apply top to bottom, but Router::layer applies bottom to top. After upgrading, the AuthLayer ran after the RateLimit layer and unauthenticated traffic consumed tenant tokens.