---
id: learnings/review-pr/reviewers-missed-a-missing-tenant-filter-in-a-new-query
kind: learning
title: Reviewers missed a missing tenant filter in a new query
tags:
- review
- security
- multitenancy
task_class: review-pr
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

A report query joined postings without tenant_id and leaked cross-tenant totals in staging. The review checklist now requires a tenant predicate on every query; a lint script greps for it.