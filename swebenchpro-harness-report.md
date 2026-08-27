# SWE-bench Pro harness comparison

Generated from local `benchctl` history on 2026-08-26.

## Scope and method

- Jobs in history: **72**; raw task records: **6087**.
- Stable canonical task checksums represented: **761** (the 731-task main suite plus 30 legacy/revision-specific checksums); mapped, exact-deduplicated attempts: **4757**.
- Unmapped rows: **299**. Of these, 297 are from `2f58c5373fa6`: its artifact is unavailable and nine repeated base-commit groups prevent exact issue assignment. The other two are pending rows with no trial result or metrics.
- Exact duplicate imported attempts are collapsed. The two duplicated mini-swe-agent-pypi job IDs therefore do not count twice.
- Accuracy is binary per task: every harness with at least one resolved attempt is an accuracy winner; the CSV also shows its resolved/graded sample count. Token, cost, and time winners are the lowest **successful** observed attempt; failed zero-token/build-error trials cannot win efficiency. Tokens are input + output. Time is end-to-end trial duration.
- Null cost is treated as unknown, not zero.

## Bottom line

- At least one harness solved **672** of the 761 stable task checksums; **89** had no recorded successful attempt.
- `claude-code` solved the broadest set (**647**) and had the lowest successful token count on **496** tasks and fastest successful run on **541** tasks. It uniquely solved **268** tasks.
- `mini-swe-agent-pypi` solved **374** tasks, uniquely solved **16**, and won known successful cost on **245** tasks.
- `mini-swe-agent` had the lowest mean successful cost in the mapped attempt data and won token/cost/time on **78/122/52** tasks. Its official 540-task job is stronger than the exact-task aggregate below because 297 rows cannot be safely mapped.
- `subconscious-code` is only a 28-task partial run: it resolved 5 graded tasks, and those five were the lowest-token and fastest successful observations where it participated. Cost was not recorded.

## Largest jobs by recorded accuracy

| Harness | Model | Job | Trials | Resolved | Accuracy | Total tokens | Cost | Tokens/resolved | Cost/resolved | Seconds/resolved |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| claude-code | subconscious/glm-5.2 | `eed41370c828` | 731 | 571 | 78.11% | 1883976167 | 1791.10 | 3299433 | 3.14 | 1054.5 |
| claude-code | subconscious/glm-5.2 | `a79a271e31b6` | 662 | 469 | 70.85% | 1569937865 | 1478.33 | 3347415 | 3.15 | 1101.1 |
| mini-swe-agent-pypi | nex-agi/Nex-N2-Pro | `f01be53ac348` | 731 | 374 | 51.16% | 3350894038 | 572.39 | 8959610 | 1.53 | 2388.2 |
| mini-swe-agent-pypi | nex-agi/Nex-N2-Pro | `91d939412447` | 731 | 374 | 51.16% | 3350894038 | 572.39 | 8959610 | 1.53 | 2388.2 |
| claude-code | /mnt/glm-5.2-nvfp4 | `2f892e769696` | 161 | 79 | 49.07% | 455939341 | 398.79 | 5771384 | 5.05 | 1180.0 |
| mini-swe-agent | subconscious/tim-qwen3.6-27b | `2f58c5373fa6` | 540 | 261 | 48.33% | 851378335 | 155.35 | 3261986 | 0.60 | 1137.3 |
| claude-code | glm-5.2 | `399624f9cd96` | 122 | 58 | 47.54% | 418325526 | 291.78 | 7212509 | 5.03 | 1409.3 |
| claude-code | subconscious/glm-5.2 | `d17f2121f74e` | 269 | 88 | 32.71% | 521292319 | 460.10 | 5923776 | 5.23 | 1538.4 |
| claude-code | subconscious/glm-5.2 | `55b03f849574` | 731 | 57 | 7.80% | 188032379 | 149.78 | 3298814 | 2.63 | 6917.7 |
| mini-swe-agent | subconscious/glm-5.2 | `a0967f91009f` | 731 | 31 | 4.24% | 472127942 | 68.09 | 15229934 | 2.20 | 13462.2 |

## Deduplicated historical harness results

| Harness | Tasks | Graded attempts | Successful | Attempt accuracy | Tasks ever solved | Mean success tokens | Mean success cost | Mean success time |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| claude-code | 731 | 2689 | 1342 | 49.91% | 647 | 2492407 | 2.3857 | 752.5s |
| mini-swe-agent-pypi | 731 | 731 | 374 | 51.16% | 374 | 3600526 | 0.6192 | 976.3s |
| subconscious-code | 28 | 10 | 5 | 50.00% | 5 | 989508 | n/a | 315.0s |
| opencode | 3 | 5 | 1 | 20.00% | 1 | 2256620 | n/a | 588.6s |
| mini-swe-agent | 731 | 921 | 139 | 15.09% | 133 | 1570114 | 0.2764 | 604.1s |
| pi | 69 | 64 | 1 | 1.56% | 1 | 2125975 | n/a | 650.5s |
| codex | 26 | 26 | 0 | 0.00% | 0 | n/a | n/a | n/a |

## Per-task winner counts

Accuracy counts are tasks each harness solved at least once; all-failed tasks have no winner. Efficiency columns require a successful attempt and known metric.

| Harness | Accuracy wins/ties | Token wins | Cost wins | Time wins |
|---|---:|---:|---:|---:|
| claude-code | 647 | 496 | 298 | 541 |
| mini-swe-agent | 133 | 78 | 122 | 52 |
| mini-swe-agent-pypi | 374 | 93 | 245 | 74 |
| opencode | 1 | 0 | 0 | 0 |
| pi | 1 | 0 | 0 | 0 |
| subconscious-code | 5 | 5 | 0 | 5 |

## Files

- `swebenchpro-task-winners.csv`: exact per-task winners and winning job/variant for all four metrics.
- `swebenchpro-harness-summary.csv`: deduplicated harness aggregates.
- `swebenchpro-job-inventory.csv`: all 72 jobs, including empty, failed, cancelled, and imported runs.
- `swebenchpro-unmapped-trials.csv`: 297 ambiguous legacy rows plus two pending rows with no archived result or metrics.
