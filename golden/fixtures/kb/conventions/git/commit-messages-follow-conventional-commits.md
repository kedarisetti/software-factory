---
id: conventions/git/commit-messages-follow-conventional-commits
kind: convention
title: Commit messages follow conventional commits
tags:
- git
- process
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

Use feat, fix, chore, docs, refactor and test prefixes. Breaking changes carry a BREAKING CHANGE footer. The release tool builds the changelog from these prefixes, so a missing prefix drops the change from release notes.