---
id: learnings/migrate-db/serde-default-needed-after-adding-a-field-to-a-stored-json
kind: learning
title: serde default needed after adding a field to a stored JSON blob
tags:
- serde
- json
- migrations
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

Old rows in the metadata column lacked the new field and deserialization failed in production. Add serde(default) when extending types that are persisted, and cover an old-shape fixture in a test.