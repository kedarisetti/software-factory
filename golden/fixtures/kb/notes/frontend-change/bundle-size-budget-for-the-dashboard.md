---
id: notes/frontend-change/bundle-size-budget-for-the-dashboard
kind: note
title: Bundle size budget for the dashboard
tags:
- frontend
- performance
- bundle
task_class: frontend-change
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

The main chunk must stay under 250 kB gzipped. The size-limit check fails the build. Lazy load the reports route and avoid importing all of lodash.