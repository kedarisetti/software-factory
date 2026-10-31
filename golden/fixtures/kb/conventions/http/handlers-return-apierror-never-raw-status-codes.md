---
id: conventions/http/handlers-return-apierror-never-raw-status-codes
kind: convention
title: Handlers return ApiError never raw status codes
tags:
- http
- axum
- errors
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

Every axum handler returns Result<Json<T>, ApiError>. ApiError maps domain errors to a stable JSON body with a machine readable code field, so clients never parse messages.