# Baseline

The pre-upgrade numbers this repo's benchmark claims are measured against.
Captured 2026-08-06, before the durable-context layer existed.

| file | what |
|---|---|
| `versus-axium-20260806.jsonl` | Axium vs Orange, 5 scenarios x 3 reps |
| `versus-orange-20260806.jsonl` | the same, Orange's side |
| `bench-deepseek-v4-pro-20260806.jsonl` | the 20-scenario bench, default config |

These are kept out of `logs/` on purpose. Everything in `logs/` is a per-run
result and is git-ignored; these are the fixed point a later run is compared
against, so they are source. Do not append to them — capture a new file instead.

The headline: V3 continuity 0.71 / 1.00 / 0.71 for Axium against 1.00 / 1.00 /
1.00 for Orange, and V4 at 47 tool calls against Orange's 14. Those two lines are
what the upgrade set out to fix.
