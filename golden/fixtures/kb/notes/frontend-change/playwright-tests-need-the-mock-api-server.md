---
id: notes/frontend-change/playwright-tests-need-the-mock-api-server
kind: note
title: Playwright tests need the mock API server
tags:
- frontend
- testing
- playwright
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

End to end tests run against msw handlers started by pnpm test:e2e. Tests that call the real backend are flaky in CI and are not allowed.