# bench-rust

Benchmarks for the **Rust** implementation of Axium.

`bench-python` measures the Python implementation by importing it. This measures
the Rust implementation by *running it*: one `axium --once` process per
scenario, against a freshly generated copy of the same seed project, graded by
the same graders, written as the same JSONL row. A row from either project can
be compared with a row from the other, which is the entire point.

## What is shared, and how

| thing | lives in | reached by |
|---|---|---|
| scenario ids, prompts, turn text | `../scenarios.json` | read directly |
| seed project fixtures | `bench-python/bench/fixtures.py` | `bridge.py generate` |
| graders and the regression suite | `bench-python/bench/grade.py`, `scenarios.py` | `bridge.py grade` |
| pricing table | `src/agent/metrics.rs` (mirrors `python/axium/pricing.py`, parity-tested) | inside the binary |
| row schema | both runners | identical keys; `impl` says which build |

Nothing is reimplemented here. The graders are Python that imports and executes
the agent's output code, and two graders that could disagree would make the
comparison meaningless. So there is one, and this project talks to it over a
process boundary (`bench-python/bridge.py`, one JSON object per call).

`scenarios.json` is generated. Edit the Python definitions, then
`python export_scenarios.py`; `python export_scenarios.py --check` fails if the
file is stale, which means the two suites are already measuring different things.

## Setup

```powershell
cd ..
cargo build --release                    # the binary this measures
cd bench-rust
cargo build --release
```

It needs an axium `config.json` with an API key. By default it reads
`../python/config.json`; pass `--config PATH` for another. Nothing in that
config is used as-is: every scenario gets its own copy with `working_directory`,
`memory_file` and `facts_file` pointed **inside the build directory**, so nothing
a scenario does can reach your real memory, facts or history.

```powershell
.\target\release\bench-rust --sanity     # ALWAYS first. Free.
.\target\release\bench-rust --list
```

## Running

```powershell
bench-rust --only X2                                   # the cheapest real run
bench-rust --only B1,B4,R1 --reps 3
bench-rust --kind fix
bench-rust --compare deepseek-v4-pro,deepseek-v4-flash
bench-rust --continuation ""                           # disable cheap routing
bench-rust --no-facts                                  # one ablation
bench-rust --no-facts --no-brain --no-planner --no-checkpoints
```

Each ablation flag removes **exactly one** mechanism and nothing else, and lands
in its own log file, so a score difference can be attributed to that mechanism
rather than to "the new version".

Logs land in `logs/<model>__<mode>[__knobs].jsonl`, one file per configuration.
The Python report reads them unchanged:

```powershell
cd ..\bench-python
python -m bench.report --dir ..\bench-rust\logs --all
python -m bench.report --dir ..\bench-rust\logs --scenarios --tools
```

## How a scenario runs

1. `bridge generate <build>`, a fresh seed project with its planted defects.
2. A per-build `config.json` is written under `<build>/.axium/` with every knob
   applied and every data path inside the build.
3. `axium --once "<request>" --workdir <build> --config <build>/.axium/config.json`
, one JSON object on stdout: text, changed files, prompt class, questions
   asked, and the full turn metrics.
4. `bridge grade <id> --build <build> --turn <turn.json>`, the same three-way
   split as the Python runner (file-tree graders for fix/refactor/feature,
   answer graders for aware, turn graders for behaviour), plus the regression
   suite.
5. The row is written. The build directory is deleted unless `--keep`.

A turn that has not finished in ten minutes is killed and recorded as such. A
grader that crashes is recorded as `GRADER FAILED`, never as a zero score.

## Differences from bench-python that are real

- **`wall_s` includes process startup.** The Python runner times `agent.run()`;
  this times the whole `axium --once` process, roughly half a second more.
- **No `--cheap-effort`.** The Rust build has no separate reasoning-effort knob
  for its cheap roles. The row records `cheap_effort: null` rather than copying
  a flag the binary does not honour.
- **`--once` auto-approves `ask_user`.** Same policy as the Python bench: the
  question is recorded in `asked`, the run continues. That the agent asked at
  all is what the destructive-request scenario measures.

## Cost

| run | order of magnitude |
|---|---|
| `--sanity`, `--list` | free |
| `--only X2` | a fraction of a cent |
| `--only B1` | a few cents |
| full 20 scenarios | low single-digit dollars |
| `--compare a,b --reps 3` | tens of dollars |

Everything runs against generated throwaway copies under the system temp
directory. No scenario touches a real project.
