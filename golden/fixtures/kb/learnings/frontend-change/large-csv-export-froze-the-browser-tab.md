---
id: learnings/frontend-change/large-csv-export-froze-the-browser-tab
kind: learning
title: Large CSV export froze the browser tab
tags:
- frontend
- performance
- export
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

Building a 200k row CSV in memory on the main thread blocked the UI. The export now streams from a server endpoint with a signed download URL.