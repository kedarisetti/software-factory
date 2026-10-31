---
id: conventions/flags/feature-flags-live-in-flags-toml-and-default-off
kind: convention
title: Feature flags live in flags.toml and default off
tags:
- flags
- config
- rollout
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

New behaviour ships behind a flag declared in config/flags.toml. Flags default to off, are read through the Flags extractor, and are deleted within two releases of reaching 100 percent.