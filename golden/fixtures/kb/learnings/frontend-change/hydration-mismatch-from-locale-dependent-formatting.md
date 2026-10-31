---
id: learnings/frontend-change/hydration-mismatch-from-locale-dependent-formatting
kind: learning
title: Hydration mismatch from locale dependent formatting
tags:
- frontend
- ssr
- i18n
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

Server rendered dates used the server locale and the client used the browser locale, causing a hydration warning. Pass the locale explicitly to formatters instead of relying on defaults.