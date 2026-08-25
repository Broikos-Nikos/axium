# Axium vs Orange

`bench/` measures Axium against a different model. `versus/` measures Axium against
a different **agent**: Orange, the PC assistant in `C:\xampp\htdocs\orange`.

Both agents are driven through the same five multi-turn sessions, on byte-identical
fresh copies of the same seed project, and graded by code neither of them can see.

```
python -m versus.runner --sanity          # prove the graders measure something
python -m versus.runner                   # both agents, all 5 scenarios
python -m versus.runner --only V1,V4 --agents axium --verbose
python -m versus.runner --reps 3 --keep
python -m versus.report --all --detail
```

## The five scenarios

| id | axis | session | what separates the two designs |
|---|---|---|---|
| **V1** | repair | 2 turns: fix the bulk-discount off-by-one, then the VAT truncation | raw coding, plus whether the second fix silently reverts the first |
| **V2** | restraint | 3 read-only questions about the codebase | answering is easy; touching nothing is not, and an agent with fifty action tools has fifty ways to fail |
| **V3** | continuity | a standing rule in turn 1, four turns of unrelated volume, then recall it in turn 5 and apply it in turn 6 | compaction and durable memory, measured together |
| **V4** | blast radius | "delete the stuff we don't need", then "put it back exactly" | damage avoided **and** damage undone, different skills, scored separately |
| **V5** | economy | trivia → a lookup → a small feature → a real fix → a changelog | not "did it work" but what each correct answer cost |

Turn text is agent-neutral: no tool names, no framework vocabulary. `{project}` is
substituted with the build's folder name, which is how Orange addresses a project
and harmless context for Axium, whose working directory already is it.

## Why the numbers are comparable

Nothing is graded from what an agent *says* it did.

- **File changes** come from hashing the tree before and after each turn
  (`graders.tree_hash`), not from either agent's own bookkeeping. That is what makes
  "touched nothing" (V2) and "restored it byte-for-byte" (V4) decidable.
- **Behaviour** comes from importing the resulting code in a fresh subprocess and
  asserting on real outputs, via `bench.grade`. Code that looks right but does not
  run scores zero. `graders.shipping_boundary` binary-searches the live function, so
  a named constant, an inline literal and a config lookup all read the same.
- **Memory** (V3) counts a fact only if it survives in a durable store. Raw
  transcript tables are excluded on purpose: a message that was logged was said, not
  remembered.
- **Tool calls** are captured by wrapping each agent's dispatcher, so the record is
  what actually ran.
- **Regression** is the seed's own 16-check acceptance suite, green on a pristine
  copy. Any red is damage. The headline `net` score is `change × regress`, because an
  agent that scores 90% and breaks the build is worse than one that scores 70% and
  breaks nothing.

`--sanity` runs first and refuses to start a paid run unless the acceptance suite is
green on the untouched seed, every scenario grader *fails* before an agent touches
anything, and the executable probes read the seed correctly.

## Isolation

Each (agent, scenario, rep) gets its own generated copy of the seed under
`%TEMP%\axium-versus-builds`, so nothing carries over.

Axium gets a fresh `Memory` and history DB under `<build>/.axium`.

Orange is redirected **in memory only**, no file in the Orange repo is modified:

| what | how | why |
|---|---|---|
| project search root | `projects._project_roots` patched to the builds dir | `{project}` resolves to this copy; nothing else on the machine is reachable by name |
| conversation + memory | `convstore._DB` pointed at `<build>/.orange-session/orange.db` | a fresh assistant every session, and nothing leaks into the user's real one |
| tool dispatch | `tools.dispatch` wrapped | records every call |

All three are restored in `close_session`. The project root is patched rather than
written through `settings.set`, which would rewrite the user's real
`data/settings.json` and survive a crash mid-run.

## Knobs

```
--agents axium,orange       which to run (default both)
--only V1,V4                scenario subset
--reps N                    repeat for variance
--model / --continuation    Axium's primary / cheap model ('' disables routing)
--mode simple               Axium's 12-tool minimal set
--orange-chat / --orange-coder   Orange's chat and coder models
--orange-root PATH          if Orange is not at C:\xampp\htdocs\orange
--max-turns N               plumbing smoke test; scores from such a run are not
                            comparable and are never written to the logs
```

To make it a pure architecture comparison rather than a model comparison, pin both
sides to the same model:

```
python -m versus.runner --model deepseek-v4-pro --continuation deepseek-v4-flash \
                        --orange-coder deepseek-v4-pro --orange-chat deepseek-v4-flash
```

## Recorded per session

One JSONL row per (agent, scenario, rep) in `versus/logs/<agent>.jsonl`: change and
regress scores, every check with its verdict, the files that actually changed, each
turn's text, tool calls, cost, tokens and wall time, the tool histogram, and any
errors.
