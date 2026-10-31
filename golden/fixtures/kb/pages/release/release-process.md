---
id: pages/release/release-process
kind: page
title: Release process
tags:
- release
- process
- ci
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

Merging to main builds an image tagged with the git sha and deploys to staging. A release is cut by tagging; the pipeline then runs smoke tests and promotes to production with a canary at ten percent.