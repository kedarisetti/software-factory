---
id: learnings/upgrade-deps/a-minor-bump-of-the-jwt-crate-rejected-our-tokens
kind: learning
title: A minor bump of the jwt crate rejected our tokens
tags:
- jwt
- auth
- upgrade
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

The new version validates the nbf claim by default and enforces a required audience. Tokens from the identity service without nbf started failing; set validate_nbf false explicitly or fix the issuer.