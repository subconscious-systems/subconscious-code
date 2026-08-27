# SWE-bench Verified: best harness for each task

Generated from all local `benchctl` Verified history on 2026-08-26.

## Method

- Raw trial rows: **25200**; exact-deduplicated attempts: **25152**; canonical tasks: **500**.
- At least one harness solved **476** tasks; **24** have no recorded successful attempt.
- Accuracy is binary: every harness with a resolved attempt wins accuracy for that task. Token, cost, and time winners are the lowest successful observations only.
- Tokens are input + output. Time is full trial duration. Missing cost is unknown, never zero.
- Exact imported copies are collapsed; intentional reruns with different measurements remain.

## Winner counts

| Harness | Tasks solved | Lowest-token wins | Lowest-cost wins | Fastest wins |
|---|---:|---:|---:|---:|
| claude-code | 420 | 20 | 58 | 34 |
| codex | 410 | 38 | 0 | 37 |
| mini-swe-agent | 399 | 50 | 396 | 23 |
| opencode | 456 | 22 | 0 | 41 |
| pi | 299 | 12 | 0 | 3 |
| subconscious-code | 406 | 334 | 0 | 338 |
| unknown | 11 | 0 | 0 | 0 |

## Deduplicated harness history

| Harness | Tasks attempted | Tasks ever solved | Graded attempts | Successful attempts | Attempt accuracy | Mean success tokens | Mean success cost | Mean success time |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| opencode | 500 | 456 | 542 | 490 | 90.41% | 1991753 | unknown | 595.8s |
| claude-code | 500 | 420 | 1412 | 729 | 51.63% | 2716673 | 6.4727 | 465.1s |
| codex | 500 | 410 | 939 | 663 | 70.61% | 949172 | unknown | 434.3s |
| subconscious-code | 500 | 406 | 1760 | 1202 | 68.30% | 367369 | unknown | 305.0s |
| mini-swe-agent | 500 | 399 | 13709 | 6874 | 50.14% | 618548 | 0.1188 | 489.7s |
| pi | 500 | 299 | 509 | 301 | 59.14% | 1202996 | unknown | 839.0s |
| unknown | 39 | 11 | 23 | 11 | 47.83% | 659128 | 0.1204 | 283.3s |

Full per-task winners, including winning job IDs and variants, are in `swebench-verified-task-winners.csv`.
