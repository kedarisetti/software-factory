---
id: conventions/errors/errors-use-thiserror-in-libraries-and-anyhow-at-the-edge
kind: convention
title: Errors use thiserror in libraries and anyhow at the edge
tags:
- errors
- rust
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

Library crates define typed errors with thiserror so callers can match on them. Only the binary crates and request handlers use anyhow, and they attach context with with_context before returning.