---
id: learnings/fix-ci/flaky-settlement-test-was-a-clock-race
kind: learning
title: Flaky settlement test was a clock race
tags:
- testing
- flaky
- settlement
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

test_cutoff_rolls_to_next_day failed one run in twenty because it read the wall clock twice around 17:00 UTC. Fixed by injecting a Clock trait and using a fixed time in tests. Do not just retry the job.