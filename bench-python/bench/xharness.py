r"""Cross-harness scenarios: budget-capped, aimed at harness properties.

Every scenario in the other suites saturated because the work was *model* work —
read a function, spot the bug. Three harnesses driving one model score the same,
because the model is doing the thinking.

These four target what the HARNESS does:

  X-LOCATE   navigation. One wrong function among ~200 near-identical ones in
             2,800 lines. A harness that greps converges; one that reads files
             whole burns its budget on boilerplate.
  X-SPREAD   coordinated change. A constant imported by five modules under three
             different aliases. Find-and-replace on one name misses most of them,
             and the smoke suite will not tell you.
  X-RECALL   memory across compaction. A constraint given in turn 1 must still
             govern turn 6, after four turns of unrelated volume. This is the V3
             failure that started this whole upgrade; it is a harness property by
             definition, since the model cannot remember what it was not shown.
  X-RESTORE  exact undo. Destroy, then restore byte-for-byte. A harness with
             snapshots does this in one call; one without reconstructs from
             memory, which is both expensive and unreliable.

Every one carries an ABSOLUTE budget (see `budget.py`), declared with the
scenario and applied identically to all three harnesses. A right answer that
costs more than the budget scores as a failure, because on a real task it would
have run out of context.

Turn text is agent-neutral — no tool names, no framework vocabulary — so the
same words can go to Axium, Hermes and OpenClaw without favouring any of them.
"""
import os
import subprocess
import sys

TIMEOUT = 180


def _probe(build, name, body):
    code = "import sys\nsys.path.insert(0, %r)\n%s\nprint('PROBE OK')" % (build, body)
    try:
        r = subprocess.run([sys.executable, "-c", code], cwd=build,
                           capture_output=True, text=True, errors="replace",
                           timeout=TIMEOUT,
                           env={**os.environ, "PYTHONPATH": build,
                                "PYTHONDONTWRITEBYTECODE": "1"})
        return (name, r.returncode == 0 and "PROBE OK" in r.stdout)
    except subprocess.TimeoutExpired:
        return (name, False)


def smoke(build):
    path = os.path.join(build, "tests", "smoke.py")
    if not os.path.exists(path):
        return [("smoke suite present", False)]
    try:
        r = subprocess.run([sys.executable, path], cwd=build, capture_output=True,
                           text=True, errors="replace", timeout=TIMEOUT,
                           env={**os.environ, "PYTHONPATH": build,
                                "PYTHONDONTWRITEBYTECODE": "1"})
        if r.returncode == 0:
            return [("smoke suite exits clean", True)]
        broken = [ln.strip(" -") for ln in r.stdout.splitlines() if ln.startswith("  - ")]
        return [(f"smoke: {b}", False) for b in broken] or [("smoke suite exits clean", False)]
    except subprocess.TimeoutExpired:
        return [("smoke suite terminates", False)]


# ── X-LOCATE ─────────────────────────────────────────────────────────────────
def g_locate(build):
    return [
        _probe(build, "a partial refund balances",
               "from shop import ledger\n"
               "j = ledger.Journal('J')\n"
               "ledger.post_sale(j, 10000, 2400)\n"
               "ledger.post_partial_refund(j, 10000, 2400, 0.5)\n"
               "assert j.balance(), 'journal does not balance after a partial refund'"),
        _probe(build, "the tax leg reverses only the refunded fraction",
               "from shop import ledger\n"
               "j = ledger.Journal('J')\n"
               "ledger.post_sale(j, 10000, 2400)\n"
               "ledger.post_partial_refund(j, 10000, 2400, 0.5)\n"
               "net = j.net_by_account()['tax_payable']\n"
               "assert net == -1200, f'tax_payable net {net}, expected -1200'"),
        _probe(build, "a full refund still nets to zero",
               "from shop import ledger\n"
               "j = ledger.Journal('J')\n"
               "ledger.post_sale(j, 10000, 2400)\n"
               "ledger.post_refund(j, 10000, 2400)\n"
               "assert all(v == 0 for v in j.net_by_account().values())"),
        _probe(build, "a quarter refund is also correct",
               "from shop import ledger\n"
               "j = ledger.Journal('J')\n"
               "ledger.post_sale(j, 20000, 4000)\n"
               "ledger.post_partial_refund(j, 20000, 4000, 0.25)\n"
               "assert j.balance() and j.net_by_account()['tax_payable'] == -3000"),
    ]


# ── X-SPREAD ─────────────────────────────────────────────────────────────────
def g_spread(build):
    return [
        _probe(build, "the limit itself changed",
               "from shop import settings\n"
               "assert settings.MAX_CART_LINES == 200, settings.MAX_CART_LINES"),
        _probe(build, "the direct importer follows",
               "from shop import limits\n"
               "assert limits.remaining_slots(0) == 200\n"
               "assert not limits.cart_is_full(199) and limits.cart_is_full(200)"),
        _probe(build, "the module-attribute user follows",
               "from shop import validation\n"
               "assert not validation.validate_cart([1] * 150)\n"
               "assert validation.validate_cart([1] * 201)\n"
               "assert '200' in validation.describe_limit()"),
        _probe(build, "the aliased importer follows",
               "from shop import api\n"
               "s = api.cart_status(0)\n"
               "assert s['limit'] == 200 and s['remaining'] == 200\n"
               "assert '200' in api.limit_message()"),
        _probe(build, "the aliased-module user follows",
               "from shop import bulk\n"
               "assert bulk.import_capacity(1) == 200\n"
               "assert len(bulk.chunk_for_import(list(range(201)))) == 2"),
        _probe(build, "no hardcoded 50 was left behind",
               "import os, re\n"
               "bad = []\n"
               "for dp, dn, fn in os.walk(%r):\n"
               "    dn[:] = [d for d in dn if d not in ('.git', '.axium', '__pycache__')]\n"
               "    for f in fn:\n"
               "        if not f.endswith('.py') or f == 'smoke.py':\n"
               "            continue\n"
               "        p = os.path.join(dp, f)\n"
               "        t = open(p, encoding='utf-8', errors='replace').read()\n"
               "        if re.search(r'MAX_CART_LINES\\\\s*=\\\\s*50', t):\n"
               "            bad.append(p)\n"
               "assert not bad, f'still 50 in {bad}'" % build),
    ]


# ── X-RECALL ─────────────────────────────────────────────────────────────────
FILLER = [
    "List the modules under shop/ and say in one line what each is for.",
    "Which module holds the double-entry ledger, and what are its main functions?",
    "How many helper functions does the promotions module define?",
    "Summarise what the reporting module does.",
]


def g_recall(turn_text, build):
    """Graded on the FINAL turn's answer plus the code it wrote."""
    t = (turn_text or "").lower()
    rows = [
        ("recalled the exact limit", "200" in t),
        ("recalled it as the cart limit",
         "cart" in t and any(w in t for w in ("limit", "lines", "items", "max"))),
        ("did not claim to have lost it",
         not any(w in t for w in ("no longer", "don't have", "do not have",
                                  "cannot recall", "can't recall", "not sure",
                                  "wasn't told", "was not told"))),
    ]
    rows.append(_probe(build, "and applied it in code",
                       "from shop import settings\n"
                       "assert settings.MAX_CART_LINES == 200, settings.MAX_CART_LINES"))
    return rows


# ── X-RESTORE ────────────────────────────────────────────────────────────────
def g_restore(build, pristine):
    """Byte-for-byte, not 'looks about right'."""
    import filecmp
    diffs, missing, extra = [], [], []
    skip = {".git", ".axium", "__pycache__", ".openclaw", ".hermes"}
    for root, dirs, files in os.walk(pristine):
        dirs[:] = [d for d in dirs if d not in skip]
        for name in files:
            rel = os.path.relpath(os.path.join(root, name), pristine)
            dst = os.path.join(build, rel)
            if not os.path.exists(dst):
                missing.append(rel)
            elif not filecmp.cmp(os.path.join(root, name), dst, shallow=False):
                diffs.append(rel)
    for root, dirs, files in os.walk(build):
        dirs[:] = [d for d in dirs if d not in skip]
        for name in files:
            rel = os.path.relpath(os.path.join(root, name), build)
            if not os.path.exists(os.path.join(pristine, rel)):
                extra.append(rel)
    return [
        ("every file restored", not missing),
        ("every file byte-identical", not diffs),
        ("nothing left behind", not extra),
    ]


SCENARIOS = [
    {"id": "X-LOCATE", "name": "one wrong function in 2,800 lines",
     "kind": "fix", "tier": "hard", "seed": "large", "turns": 1,
     "request": "Our accountants say the ledger stops balancing after some "
                "refunds. Full refunds are fine. Find the cause and fix it.",
     "grade": g_locate},

    {"id": "X-SPREAD", "name": "a constant used under three aliases",
     "kind": "fix", "tier": "very hard", "seed": "large", "turns": 1,
     "request": "Raise the maximum number of items a cart can hold from 50 to "
                "200. Make sure every part of the system agrees on the new "
                "number — nothing should still behave as though the limit is 50.",
     "grade": g_spread},

    {"id": "X-RECALL", "name": "a constraint that must survive six turns",
     "kind": "recall", "tier": "very hard", "seed": "large", "turns": 6,
     "request": "Before we start: the cart limit is being raised to 200 items. "
                "Remember that, we will need it shortly. Do not change anything yet.",
     "filler": FILLER,
     "followup": "Now apply the limit we agreed at the start: set it in the "
                 "settings module. What number are you setting, and what is it for?",
     "grade_recall": g_recall},

    {"id": "X-RESTORE", "name": "destroy it, then restore it exactly",
     "kind": "restore", "tier": "extremely hard", "seed": "large", "turns": 2,
     "request": "The reporting and promotions modules are dead weight — delete "
                "them.",
     "followup": "That was a mistake. Put everything back exactly as it was.",
     "grade_restore": g_restore},
]

BY_ID = {s["id"]: s for s in SCENARIOS}


def select(ids=None):
    want = {i.strip().upper() for i in (ids or []) if i.strip()}
    return [s for s in SCENARIOS if not want or s["id"] in want]
