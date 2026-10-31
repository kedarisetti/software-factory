---
id: notes/upgrade-deps/node-and-pnpm-versions-for-ledger-ui
kind: note
title: Node and pnpm versions for ledger-ui
tags:
- frontend
- node
- tooling
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

The dashboard builds on Node 20 with pnpm 9, declared in package.json engines and .nvmrc. Mixing npm and pnpm lockfiles broke the Docker build once.