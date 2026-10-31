---
id: notes/add-endpoint/openapi-spec-is-generated-from-handler-annotations
kind: note
title: OpenAPI spec is generated from handler annotations
tags:
- api
- openapi
- docs
task_class: add-endpoint
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

Run cargo xtask openapi after adding a route. The utoipa annotations on handlers produce openapi.json, which the TypeScript client generator consumes. CI fails if the committed file is out of date.