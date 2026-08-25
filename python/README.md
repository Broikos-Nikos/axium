# Axium (Python) + benchmark harness

A Python implementation of the Rust agent in `../src`, plus a 20-scenario
benchmark that measures it. Same tool names, same
`classify → tool-loop → heartbeat → review` pipeline, same cost routing between a
primary and a cheap continuation model, so numbers measured here describe the
architecture, not just this port.

It exists because the Rust build needs a C toolchain (bundled SQLite) that this
machine does not have. Python runs the whole stack with one dependency.

```
pip install -r requirements.txt
python -m axium --check          # show providers + model routing
python -m axium                  # interactive REPL
```

## Layout

```
python/
  axium/                the agent
    config.py           config load/save, provider resolution
    providers.py        DeepSeek / OpenAI / Anthropic adapters (streaming + tools)
    pricing.py          $/M token table -> real USD per call
    metrics.py          Meter: tokens, cost, latency, tool behaviour
    toolspec.py         tool JSON schemas
    tools.py            tool implementations (sandboxed to the working directory)
    memory.py           persistent markdown memory
    db.py               SQLite history (FTS5) + task queue
    classifier.py       cheap-model routing, heartbeat, code review
    compactor.py        history compaction
    router.py           THE AGENT LOOP
    cli.py              REPL
  bench/                the benchmark
    fixtures.py         seed project generator with 6 planted defects
    scenarios.py        the 20 scenarios
    grade.py            objective graders (behavioural, not pattern-matching)
    runner.py           runs scenarios, writes JSONL
    report.py           aggregates logs into tables
    logs/               one .jsonl per (model, mode)
  config.json           local, git-ignored (holds the API key)
  config.example.json   template
```

## Providers

| provider | models | notes |
|---|---|---|
| `deepseek` | `deepseek-v4-pro`, `deepseek-v4-flash` | OpenAI-compatible; streams `reasoning_content`, reports `prompt_cache_hit_tokens` |
| `openai` | `gpt-4.1*`, `gpt-5.4-mini` | chat completions |
| `anthropic` | `claude-*` | extended thinking + prompt caching |

The provider is sniffed from the model id (`deepseek-*`, `claude-*`) or set
explicitly per role. Keys come from `config.json` or `DEEPSEEK_API_KEY` /
`OPENAI_API_KEY` / `ANTHROPIC_API_KEY`.

## Cost routing and the two effort tiers

The whole point of the design. Call 1 of a turn carries the reasoning and goes to
the primary model; every continuation call ("read the tool result, call the next
tool") is mechanical and goes to the cheap model. The classifier can also mark an
entire task cheap, or answer trivia itself without waking the primary model at all.

Reasoning effort is tiered the same way:

| role | model | effort | setting |
|---|---|---|---|
| primary | `deepseek-v4-pro` | `max` | `settings.thinking_effort` |
| continuation, classifier, compactor, review | `deepseek-v4-flash` | `low` | `settings.cheap_effort` |

The shared scale is `off | low | medium | high | max`, normalised per provider:
DeepSeek gets a nested `thinking` object, Anthropic gets adaptive thinking, and
OpenAI gets `reasoning_effort` only on the model families that accept it.

DeepSeek takes both a nested `thinking` object and a flat `reasoning_effort`
field. This uses the nested form because only that one produces a real gradient,
measured on `deepseek-v4-flash`, reasoning tokens ran low=33 / high=53 / max=58
where the flat field barely moved. It is also the shape the sibling **orange**
project sends, so both agents put identical bytes on the wire and their benchmark
numbers are comparable.

A real turn, from the per-role split the Meter records:

```
classifier     1 call        285 in    250 out   $0.00007
primary        1 call      2,456 in    136 out   $0.00119   <- the reasoning
continuation   7 calls    36,548 in  1,605 out   $0.00132   <- the mechanics
heartbeat      1 call        388 in     10 out   $0.00006
```

Seven mechanical calls on flash cost about what one reasoning call on pro costs.
That is the routing paying for itself, measured rather than assumed.

## Benchmark

20 scenarios across five families, each run against a **freshly generated** copy
of a seed project:

| family | n | what it measures |
|---|---|---|
| `fix` | 6 | diagnose and correct a planted defect |
| `refactor` | 3 | restructure without changing behaviour |
| `feature` | 3 | add something that does not exist yet |
| `aware` | 5 | read-only comprehension: answer, touch nothing |
| `behaviour` | 3 | memory persistence, cheap routing, care with destructive asks |

Two axes are scored separately:

- **change**: did it do the task? Graded by importing the agent's code in a fresh
  subprocess and asserting on real outputs. Code that looks right but does not run
  scores zero.
- **regress**: is everything that already worked still working? A 16-check
  acceptance suite that is green on the pristine seed, so any red is agent damage.

```
python -m bench.runner --sanity                     # validate the graders
python -m bench.runner                              # full suite
python -m bench.runner --only B1,B4,R1 --reps 3
python -m bench.runner --kind fix --mode simple
python -m bench.runner --compare deepseek-v4-pro,deepseek-v4-flash
python -m bench.report --all
```

### Why `--sanity` runs first

Every fix grader must **fail** on the untouched project and the regression suite
must **pass**. Otherwise a green score could just mean the grader is broken. The
runner refuses to start a paid run until sanity is clean.

### Knobs worth sweeping

Each of these changes cost or quality, and the harness measures the trade:

```
--model / --compare      primary model
--continuation ""        disable cheap routing entirely (measures what it saves)
--mode simple            skip the classifier, use the 12-tool minimal set
--max-iterations N       tool-loop budget
--effort off|low|max     reasoning effort for the PRIMARY model
--cheap-effort off|low   reasoning effort for the flash roles
--reps N                 repeat for variance
```

Each knob is folded into the log filename and recorded on every row, so two
different setups can never average into one misleading table row:

```
deepseek-v4-pro__supercharge__eff-max-low.jsonl
deepseek-v4-pro__supercharge__noroute__eff-max-low.jsonl
deepseek-v4-pro__simple__eff-max-low.jsonl
deepseek-v4-flash__supercharge__eff-max-low.jsonl
```

## Benchmarking the sibling `orange` project

`../../orange` runs the same models on the same two tiers (`CHAT_MODEL=flash` at
`CHAT_EFFORT=low`, `CODER_MODEL=deepseek-v4-pro` at `CODER_EFFORT=max`) and has
its own, more mature harness in `orange/evals/`:

| harness | scenarios | measures |
|---|---|---|
| `big_runner.py` | 24 (8 fix / 8 refactor / 8 aware) | real coder loop, 31 regression checks |
| `layered_runner.py` | router / planner / coder | per-layer scores + failure attribution |
| `toolcall_runner.py` | tool-call fidelity | does the model call the right tool |
| `project_runner.py` | medium project build | build from a SPEC |
| `brownfield_runner.py` | edit an existing project | the way the agent is actually used |
| `runner.py` | 5 conversation scripts | flat chat + coder combo |

Same two-axis design (change + regression) and the same sanity gate, so results
line up with this harness:

```bash
cd ../../orange
python evals/big_runner.py --sanity
python evals/big_runner.py --only B1 --model deepseek-v4-pro --effort max
python evals/report.py
```

## Recorded per run

`change`, `regress`, per-check detail, changed files, the agent's answer, wall
time, LLM calls, tool calls and histogram, input/output/cached/reasoning tokens,
cache-hit rate, USD cost, per-role cost split, retries, and errors.
