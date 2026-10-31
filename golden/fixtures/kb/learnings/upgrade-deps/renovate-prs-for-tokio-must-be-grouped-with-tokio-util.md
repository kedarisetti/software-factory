---
id: learnings/upgrade-deps/renovate-prs-for-tokio-must-be-grouped-with-tokio-util
kind: learning
title: Renovate PRs for tokio must be grouped with tokio-util
tags:
- dependencies
- tokio
- renovate
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

Bumping tokio alone produced duplicate versions in the dependency tree and a trait mismatch compile error. Grouping tokio, tokio-util and tokio-stream in renovate.json avoids it.