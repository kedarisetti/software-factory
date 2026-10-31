---
id: notes/review-pr/unsafe-code-is-banned-outside-the-ffi-crate
kind: note
title: Unsafe code is banned outside the ffi crate
tags:
- review
- rust
- unsafe
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

The workspace sets forbid(unsafe_code) except in crates/hsm-ffi, which wraps the signing library. Any new unsafe block there needs a SAFETY comment and a security review.