# Benchmarks, handover guide

Each implementation of Axium is measured by its own project. Both run the **same
scenarios** through the **same graders** and write the **same JSONL rows**, so a
Rust number and a Python number are directly comparable: that is the whole
design, and everything below exists to protect it.

| project | measures | how |
|---|---|---|
| **`bench-python/`** | the Python agent | imports it and calls `Agent.run()` |
| **`bench-rust/`** | the Rust binary | runs `axium --once` per turn |

Inside `bench-python` there are two suites answering different questions:

| suite | answers |
|---|---|
| `bench` | which model, and which routing knobs, are best for Axium? |
| `versus` | Axium or Orange, which agent **design** wins? |

All paid suites call a real API. Read [Cost](#cost-and-safety) before a full run.

---

## What is shared, and why

```
scenarios.json          generated from the Python definitions; ids, prompts, turn text
bench-python/bench/     fixtures (the seed project) and graders (the definition of correct)
bench-python/bridge.py  the process boundary bench-rust talks to
src/agent/metrics.rs    pricing, mirroring python/axium/pricing.py, parity-tested
```

Nothing is implemented twice. The graders are Python that imports and executes
the agent's output code, so `bench-rust` shells out to them rather than owning a
second copy: two graders that could disagree about what "correct" means would
make the comparison meaningless.

`scenarios.json` is **generated**. Edit the Python definitions, then:

```powershell
cd bench-python
python export_scenarios.py           # regenerate
python export_scenarios.py --check   # fails if stale
```

`--check` belongs before any paid run. A stale file means the two suites are
already measuring different things while still looking comparable.

---

## 0. Setup

### bench-python

```powershell
cd bench-python
pip install -r requirements.txt          # one dependency: requests
python -m bench.runner --sanity          # free smoke test
```

It imports the agent rather than vendoring it, looking in this order:
`AXIUM_PYTHON` → `../python` → whatever is importable.

```powershell
$env:AXIUM_PYTHON = "C:\path\to\axium\python"   # only if not at ../python
```

Keys come from the agent's own `config.json` (git-ignored) or from
`DEEPSEEK_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`.

### bench-rust

```powershell
cargo build --release                    # the binary under test
cd bench-rust && cargo build --release
.\target\release\bench-rust --sanity      # free
```

It needs an axium `config.json` with a key (`--config PATH`, default
`../python/config.json`). Nothing in it is used as-is: every scenario gets a copy
with `working_directory`, `memory_file` and `facts_file` pointed **inside the
build directory**, so no scenario can reach your real memory, facts or history.

---

## 1. `bench`, 23 scenarios

Each runs against a freshly generated copy of a seed project with planted
defects.

| family | n | measures |
|---|---|---|
| `fix` | 6 | diagnose and correct a planted defect |
| `refactor` | 3 | restructure without changing behaviour |
| `feature` | 3 | add something that does not exist |
| `aware` | 5 | read-only comprehension: answer, touch nothing |
| `behaviour` | 3 | memory persistence, cheap routing, care with destructive asks |
| **mechanism** | **3** | **do the durable-context mechanisms actually do anything** |

Two axes, never merged. **change**: did it do the task, graded by importing the
agent's code in a fresh subprocess. **regress**: does everything that already
worked still work. A run that ships the feature and breaks the build is worse
than one that does nothing.

```powershell
# python
cd bench-python
python -m bench.runner --sanity                       # ALWAYS first
python -m bench.runner --list
python -m bench.runner --only B1,B4,R1 --reps 3
python -m bench.runner --kind fix --mode simple
python -m bench.runner --compare deepseek-v4-pro,deepseek-v4-flash
python -m bench.runner --continuation ""              # disable cheap routing

# rust, same flags
cd bench-rust
.\target\release\bench-rust --sanity
.\target\release\bench-rust --only B1,B4,R1 --reps 3
.\target\release\bench-rust --compare deepseek-v4-pro,deepseek-v4-flash
```

### The mechanism scenarios (M1-M3)

These grade the durable-context layer rather than a coding task, and each is
paired with the ablation flag that should reopen its failure.

| id | measures | ablation |
|---|---|---|
| **M1** | a rule stated in turn 1 still governs turn 5, after filler and compaction | `--no-facts` |
| **M2** | "put it back exactly" restores **byte-identical** files, not merely plausible ones | `--no-checkpoints` |
| **M3** | a second session on a known project does not re-explore it | `--no-brain` |

M1 is the V3 head-to-head failure turned into a bench scenario. M2 compares
against a snapshot taken before the agent ran, because scoring "the file exists
again" passes a reconstruction that silently dropped a line.

### Ablations

Both runners take the same four flags. Each removes **exactly one** mechanism
and lands in its own log file, so a score difference can be attributed to that
mechanism rather than to "the new version".

```powershell
--no-facts --no-brain --no-planner --no-checkpoints
```

If an ablation changes nothing, the mechanism is not doing what it claims and
the claim gets deleted, not the flag.

### Reports

Logs land in `<project>/…/logs/<model>__<mode>[__knobs].jsonl`, one file per
configuration. The Python report reads either project's rows:

```powershell
cd bench-python
python -m bench.report --all                              # the Python runs
python -m bench.report --dir ..\bench-rust\logs --all      # the Rust runs
python -m bench.report --scenarios --tools
```

Every row carries `impl` (`"python"` / `"rust"`) and, for Rust rows,
`config.binary` and `config.binary_mtime`: a benchmark that cannot answer
"which binary?" cannot answer "did the change help?".

### Why `--sanity` is not optional

It asserts the acceptance suite passes on an untouched seed and that every fix
grader *fails* before an agent touches anything. Without both, a green score
could just mean the grader is broken. Both runners refuse to start a paid run
until it is clean. **Do not reach for a flag to get past a failure**, sanity
failing means the graders are measuring nothing, so any score from that run is
meaningless. Fix the grader or the seed.

---

## 2. `versus`, Axium against Orange

Both agents driven through the same five multi-turn sessions, on byte-identical
copies of the same seed, graded by code neither can see.

| id | axis | what it separates |
|---|---|---|
| **V1** | repair | coding ability, and whether fix 2 reverts fix 1 |
| **V2** | restraint | answering is easy; touching nothing is not |
| **V3** | continuity | a rule from turn 1 must still govern turn 6, after compaction |
| **V4** | blast radius | damage avoided **and** damage undone |
| **V5** | economy | cost per correct answer, not just correctness |

```powershell
cd bench-python
python -m versus.runner --sanity                  # first, always
python -m versus.runner
python -m versus.runner --only V1,V4 --agents axium --verbose
python -m versus.report --all --detail
```

By default each agent runs its own configured models, mixing agent design with
model choice. To isolate the architecture, pin both sides:

```powershell
python -m versus.runner --model deepseek-v4-pro --continuation deepseek-v4-flash `
                        --orange-coder deepseek-v4-pro --orange-chat deepseek-v4-flash
```

`--detail` prints only the checks the two agents disagreed on. The headline
`net` is `change x regress`; `$/pt` prices each point of real progress.

`--max-turns N` truncates every session. It exists to smoke-test the plumbing
for a few cents; **scores from a truncated run are not comparable and are never
written to the logs.**

### What it does to Orange, and does not

Orange is redirected **in memory only** for the duration of a run. No file in
the Orange repo is modified: the project search root, the conversation store and
the tool dispatcher are patched and restored in `close_session`. The project root
is patched rather than written through Orange's settings, which would rewrite its
real settings file and survive a crash mid-run.

---

## Cost and safety

| run | order of magnitude |
|---|---|
| any `--sanity`, `--list`, `--check` | free (no API calls) |
| `bench --only X2` | a fraction of a cent |
| `bench --only B1` | a few cents |
| `versus --max-turns 1` | a few cents |
| `bench` full suite | low single-digit dollars |
| `versus` (5 scenarios x 2 agents) | low single-digit dollars |
| `bench --compare a,b,c --reps 3` | tens of dollars |

Everything runs against **generated throwaway copies** under the system temp
directory (`axium-bench-builds`, `axium-versus-builds`). No suite touches a real
project. `--keep` leaves build directories behind for inspection.

V4 and M2 deliberately ask an agent to delete things. Safe because the target is
a generated copy, but do not "helpfully" repoint a runner at a real project.

---

## Adding to the suites

- **A bench scenario:** add it to `bench/scenarios.py` with a grader in
  `bench/grade.py`, then `python export_scenarios.py`. The grader must FAIL on
  the pristine seed or `--sanity` will reject it.
- **A versus scenario:** add it to `versus/scenarios.py`. Keep the turn text
  agent-neutral (no tool names, no framework vocabulary) and use `{project}`.
  Grade from the project state via `versus/graders.py`, never from what the
  agent says it did.
- **A model's pricing:** `python/axium/pricing.py` **and** `src/agent/metrics.rs`
: they are parity-tested against each other, so a table edited on one side
  fails the other's test until both agree. An unpriced model still runs but
  reports `$0.0000`, which silently corrupts every cost comparison; both meters
  list such models under `unpriced_models`, so check that field.

---

## Troubleshooting

**`bench-python cannot find the axium package`**: set `AXIUM_PYTHON` to the
directory containing it, as the error prints.

**`cannot read scenarios.json`**, `cd bench-python && python export_scenarios.py`.

**`axium binary not found`**, `cargo build --release` from the repo root.

**`ModuleNotFoundError: No module named 'versus'`**: run from `bench-python/`.

**`ModuleNotFoundError: No module named 'PySide6'`** during an Orange run,
harmless. Orange's evals import it transitively; headless it logs a traceback for
its settings watcher and carries on.

**Zero LLM calls and zero cost on a trivial turn.** Not a bug. `quick_classify`
answers some trivia with a local regex fastpath, no API call at all. That is a
real architectural difference, and V5 exists to price it.

**A clean run prints `regress 1/1`, not `regress 16/16`.** Expected. The grader
emits a row per *failure* plus one "suite exits clean" row.

**Numbers look worse than last week.** Check the log tag before concluding
anything, logs split by configuration, and comparing a `noroute`, `simple` or
`nofacts` file against a default one is comparing two different agents.

**A Rust row scored badly right after a code change.** Check
`config.binary_mtime` in the row. `cargo test` builds a test executable, not
`axium.exe`; a suite will happily run against a binary from before the change.
