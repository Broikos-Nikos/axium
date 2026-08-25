# bench-python

Benchmarks for the **Python** implementation of Axium.

Two suites, answering different questions. Mixing them up is the easiest way to
draw a wrong conclusion.

| suite | answers |
|---|---|
| `bench` | which model, and which routing knobs, are best for Axium? |
| `versus` | Axium or Orange, which agent **design** wins? |

The Rust implementation is measured by `bench-rust/`, which runs the *same*
scenarios through the *same* graders and writes the *same* JSONL rows, so a row
from either project can be compared against the other.

---

## Setup

```powershell
cd bench-python
pip install -r requirements.txt          # one dependency: requests
```

The harness imports the agent it measures rather than vendoring a copy, a
vendored copy drifts, and then you are benchmarking a stale agent without knowing
it. It looks for the package in this order:

1. `AXIUM_PYTHON`, an explicit path to the directory **containing** the `axium`
   package
2. `../python`, the layout inside the axium repo
3. whatever is already importable

```powershell
$env:AXIUM_PYTHON = "C:\path\to\axium\python"   # only if it is not at ../python
```

The agent reads its API key from its own `config.json` (git-ignored) or from
`DEEPSEEK_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`.

Smoke test, free:

```powershell
python -m bench.runner --sanity
python -m versus.runner --sanity
```

---

## `bench`, 20 scenarios, one agent, many models

Twenty scenarios in five families, each against a freshly generated copy of a
seed project with planted defects:

| family | n | measures |
|---|---|---|
| `fix` | 6 | diagnose and correct a planted defect |
| `refactor` | 3 | restructure without changing behaviour |
| `feature` | 3 | add something that does not exist |
| `aware` | 5 | read-only comprehension: answer, touch nothing |
| `behaviour` | 3 | memory persistence, cheap routing, care with destructive asks |

Two axes, never merged. **change**: did it do the task, graded by importing the
agent's code in a fresh subprocess. **regress**: is everything that already
worked still working, via an acceptance suite that is green on the pristine seed.
A run that ships the feature and breaks the build is worse than one that does
nothing.

```powershell
python -m bench.runner --sanity                       # ALWAYS first
python -m bench.runner --list                         # the scenarios and difficulty
python -m bench.runner                                # full suite, config defaults
python -m bench.runner --only B1,B4,R1 --reps 3       # a subset, three times each
python -m bench.runner --kind fix --mode simple       # one family, minimal tool set
python -m bench.runner --compare deepseek-v4-pro,deepseek-v4-flash
python -m bench.runner --continuation ""              # disable cheap routing
python -m bench.report --all                          # aggregate every log on file
python -m bench.report --scenarios --tools            # per-scenario and per-tool
```

Logs land in `bench/logs/<model>__<mode>[__knobs].jsonl`, one file per
configuration, so `--continuation ""` never averages into the default config's
numbers.

### Why `--sanity` is not optional

It asserts that the acceptance suite passes on an untouched seed and that every
fix grader *fails* before an agent touches anything. Without both, a green score
could just mean the grader is broken. The runner refuses to start a paid run
until sanity is clean. **Do not pass `--no-sanity` to get past a failure**,
sanity failing means the graders are measuring nothing, so any score from that
run is meaningless. Fix the grader or the seed first.

---

## `versus`, Axium against Orange

Both agents are driven through the same five multi-turn sessions, on
byte-identical fresh copies of the same seed project, graded by code neither of
them can see.

| id | axis | what it separates |
|---|---|---|
| **V1** | repair | coding ability, and whether fix 2 reverts fix 1 |
| **V2** | restraint | answering is easy; touching nothing is not |
| **V3** | continuity | a rule from turn 1 must still govern turn 6, after compaction |
| **V4** | blast radius | damage avoided **and** damage undone |
| **V5** | economy | cost per correct answer, not just correctness |

```powershell
python -m versus.runner --sanity                  # first, always
python -m versus.runner                           # both agents, all 5 scenarios
python -m versus.runner --only V1,V4 --agents axium --verbose
python -m versus.report --all --detail
```

By default each agent runs its own configured models, so the result mixes agent
design with model choice. To isolate the architecture, pin both sides:

```powershell
python -m versus.runner --model deepseek-v4-pro --continuation deepseek-v4-flash `
                        --orange-coder deepseek-v4-pro --orange-chat deepseek-v4-flash
```

`--detail` prints only the checks the two agents disagreed on, which is where the
interesting differences are. The headline `net` is `change x regress`. `$/pt`
prices each point of real progress.

### What it does to Orange, and does not

Orange is redirected **in memory only** for the duration of a run. No file in the
Orange repo is modified: the project search root, the conversation store and the
tool dispatcher are patched and then restored in `close_session`. The project
root is patched rather than written through Orange's settings, which would
rewrite its real settings file and survive a crash mid-run.

---

## Cost and safety

| run | order of magnitude |
|---|---|
| any `--sanity` | free (no API calls) |
| `versus --max-turns 1` | a few cents |
| `bench.runner --only B1` | a few cents |
| `versus.runner` (5 scenarios x 2 agents) | low single-digit dollars |
| `bench.runner` full 20 scenarios | low single-digit dollars |
| `bench.runner --compare a,b,c --reps 3` | tens of dollars |

Everything runs against **generated throwaway copies** under the system temp
directory. No suite touches a real project. `--keep` leaves the build
directories behind for inspection.

One scenario deliberately asks an agent to delete things. It is safe because the
target is a generated copy, but do not "helpfully" repoint a runner at a real
project directory.

---

## Adding a scenario

- **bench:** add it to `bench/scenarios.py` with a grader in `bench/grade.py`.
  The grader must FAIL on the pristine seed or `--sanity` will reject it.
- **versus:** add it to `versus/scenarios.py`. Keep the turn text agent-neutral
  (no tool names, no framework vocabulary) and use `{project}`. Grade from the
  project state via `versus/graders.py`, never from what the agent says it did.
- **pricing:** an unpriced model still runs but reports `$0.0000`, which silently
  corrupts every cost comparison. The meter lists such models under
  `unpriced_models`: check that field after adding one.

---

## Troubleshooting

**`bench-python cannot find the axium package`**: set `AXIUM_PYTHON` to the
directory containing it, as printed in the error.

**`ModuleNotFoundError: No module named 'versus'`**: run from `bench-python/`.
All `python -m ...` commands assume that working directory.

**`ModuleNotFoundError: No module named 'PySide6'`** during an Orange run,
harmless. Orange's evals import it transitively; headless it logs a traceback for
its settings watcher and carries on.

**Zero LLM calls and zero cost on a trivial turn.** Not a bug. Axium's
`quick_classify` answers some trivia with a local regex fastpath, no API call at
all. That is a real architectural difference, and V5 exists to price it.

**A clean run prints `regress 1/1`, not `regress 16/16`.** Expected. The grader
emits a row per *failure* plus one "suite exits clean" row, so `1/1` means
nothing broke; a red run expands to show exactly what the agent broke.

**Numbers look worse than last week.** Check the log tag before concluding
anything, `bench/logs` splits by configuration, and comparing a `noroute` or
`simple` file against a default one is comparing two different agents.
