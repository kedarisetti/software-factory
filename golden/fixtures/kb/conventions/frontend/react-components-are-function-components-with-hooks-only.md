---
id: conventions/frontend/react-components-are-function-components-with-hooks-only
kind: convention
title: React components are function components with hooks only
tags:
- frontend
- react
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

The dashboard in ledger-ui has no class components. Data fetching goes through the useQuery wrappers in src/data, never fetch directly inside a component.