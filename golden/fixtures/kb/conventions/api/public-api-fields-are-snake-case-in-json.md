---
id: conventions/api/public-api-fields-are-snake-case-in-json
kind: convention
title: Public API fields are snake_case in JSON
tags:
- api
- json
- style
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

Rust structs keep snake_case and serde needs no rename. The TypeScript client converts to camelCase only at the edge in api/client.ts. Do not mix casing inside a single response.