---
id: notes/frontend-change/dashboard-charts-use-recharts-with-memoized-series
kind: note
title: Dashboard charts use Recharts with memoized series
tags:
- frontend
- react
- charts
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

Chart data is memoized with useMemo keyed on the query result, because passing a new array each render restarted the Recharts animation and caused visible flicker.