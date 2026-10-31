---
id: notes/add-endpoint/pagination-uses-opaque-cursors-not-offsets
kind: note
title: Pagination uses opaque cursors not offsets
tags:
- api
- pagination
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

List endpoints take a limit up to 200 and an opaque cursor, a base64url encoded keyset of created_at and id. Offset pagination was removed because it was slow and unstable on the journal table.