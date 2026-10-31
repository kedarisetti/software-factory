---
id: learnings/fix-ci/docker-layer-cache-invalidated-by-copying-the-whole-repo
kind: learning
title: Docker layer cache invalidated by copying the whole repo first
tags:
- docker
- ci
- cache
task_class: fix-ci
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

The ledgerd Dockerfile copied the entire source before building dependencies, so every change rebuilt all crates. Using cargo chef to build dependencies in an earlier layer cut image builds from 14 minutes to 3.