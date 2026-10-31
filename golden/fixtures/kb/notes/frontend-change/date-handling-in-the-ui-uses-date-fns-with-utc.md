---
id: notes/frontend-change/date-handling-in-the-ui-uses-date-fns-with-utc
kind: note
title: Date handling in the UI uses date-fns with UTC
tags:
- frontend
- dates
- timezone
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

The API returns RFC 3339 UTC timestamps. The UI formats them with date-fns-tz in the viewer's zone, but settlement dates are shown in UTC because the settlement window is defined in UTC.