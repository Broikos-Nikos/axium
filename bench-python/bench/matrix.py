r"""The 3 x 3 matrix: three categories, three difficulties each.

Five categories were specified; three were kept. The two that were dropped were
dropped for cause, not for time:

    I (interaction)  every scenario of that shape scored 100% for both DeepSeek
                     models. It prices the model, not the harness.
    C (coverage)     already measured three ways, and all three harnesses solved
                     it. It discriminates on cost only, and that ground is
                     already covered by N.

What is left is the three that can separate harnesses:

    N  navigation    where does it look, and what does it read on the way?
                     Produces a cost spread rather than a pass/fail.
    M  memory        does a constraint from turn 1 still govern turn 12?
                     A harness property by definition: the model cannot
                     remember what it was not shown.
    R  restore       can it put back exactly what it destroyed, and only that?
                     A harness with snapshots does it in one call; one without
                     reconstructs from memory, which is where it breaks.

The escalation across tiers is deliberate and the same in each column:

    hard        the symptom localises to a module. ~2,800 lines.
    very hard   the symptom names only a behaviour, and the surface is ~3x
                larger. Search strategy starts to dominate.
    crazy hard  as very hard, PLUS evidence that actively points the wrong way,
                PLUS an invariant that a plausible fix violates. The wrong
                answer has to be discoverable and attractive, or a harness that
                has lost the thread merely looks uncertain instead of being
                confidently wrong - and uncertainty is not what this is trying
                to measure.

Turn text stays agent-neutral: no tool names, no framework vocabulary, so the
same words go to Axium, Hermes and OpenClaw without favouring any of them.
"""
import filecmp
import os

from bench import huge_fixtures, large_fixtures
from bench.xharness import _probe, smoke  # noqa: F401  (smoke re-exported)

SKIP_DIRS = {".git", ".axium", "__pycache__", ".openclaw", ".hermes", ".claude",
             ".venv", "node_modules", ".pytest_cache"}

# A harness keeps state, and it keeps it in the working directory because that
# is what it was pointed at. Grading those files as "left behind" measures which
# harness stores state where, not which harness can restore a tree - and it
# penalises exactly the harnesses that HAVE the machinery being tested.
#
# R1 failed Axium on this before it was fixed: 2/3, for its own memory file.
SKIP_FILES = {
    "memory.md", "facts.db", "facts.db-wal", "facts.db-shm",
    "plugins.json",          # Axium's plugin registry (src/plugins/mod.rs)
    "AGENTS.md", "CLAUDE.md", ".gitignore",
}
SKIP_STATE_DIRS = ("axium-plugins/", "skills/", ".skills/")


def _is_harness_state(rel):
    base = rel.rsplit("/", 1)[-1]
    return (base in SKIP_FILES
            or rel.startswith(SKIP_STATE_DIRS)
            or base.startswith("hermes_usage_")
            or base.endswith((".db-wal", ".db-shm")))


# ── seeds ────────────────────────────────────────────────────────────────────
def seed_large(dest):
    return large_fixtures.generate(dest)


def seed_huge(dest):
    return huge_fixtures.generate(dest)


def seed_huge_misleading(dest):
    return huge_fixtures.generate(dest, misleading=True)


STALE_DOC = """# Limits

Agreed limits for the current cycle.

| limit | value |
|---|---|
| cart lines | 200 |
| session minutes | 30 |
| page size | 25 |

The cart line limit was raised to 200 at the start of this cycle.
"""


def seed_huge_stale_doc(dest):
    return huge_fixtures.generate(dest, stale_doc=STALE_DOC)


# ── N: navigation ────────────────────────────────────────────────────────────
def _g_partial_refund(build):
    """The same four probes at every N tier: the fix is identical, only the
    difficulty of FINDING it changes. Holding the target constant is what makes
    the three tiers comparable to each other."""
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


def g_n3(build):
    """Crazy hard adds the invariant. `settle_batch` is correct and the stale
    note in reconcile.py says it is the recent change; a harness that believes
    the note will 'fix' it. Rewriting it is the plausible wrong move, so it is
    checked mechanically rather than left to judgement."""
    rows = _g_partial_refund(build)
    rows.append(_probe(build, "the innocent function was left alone",
                       "from shop import reconcile, ledger\n"
                       "a = ledger.Journal('A')\n"
                       "ledger.post_sale(a, 10000, 2400)\n"
                       "b = ledger.Journal('B')\n"
                       "b.add('cash', debit=5)\n"
                       "out = reconcile.settle_batch([a, b])\n"
                       "assert out['settled'] == ['A'], out\n"
                       "assert out['unbalanced'] == ['B'], out"))
    return rows


# ── M: memory ────────────────────────────────────────────────────────────────
FILLER_SHORT = [
    "List the modules under shop/ and say in one line what each is for.",
    "Which module holds the double-entry ledger, and what are its main functions?",
    "How many helper functions does the promotions module define?",
    "Summarise what the reporting module does.",
]

# Long filler for the 12-turn tiers. Every one of these is real work against the
# tree, so the context genuinely fills rather than being padded with nothing.
FILLER_LONG = FILLER_SHORT + [
    "What does the tax module do, and how does it relate to the ledger?",
    "Which modules define a dataclass called anything ending in Record?",
    "Roughly how many lines is the whole shop package?",
    "Which module would I change to alter how search results are ordered?",
    "Name three modules that do not import anything from shop/.",
    "What is the difference between the inventory and the catalog modules?",
]


def _said(text, *words):
    t = (text or "").lower()
    return all(w.lower() in t for w in words)


# Phrases that are an actual admission of having lost the constraint. Kept
# narrow on purpose: the first version matched "earlier in" and "not sure",
# which scored a CORRECT answer ("the limit we agreed earlier in this session")
# as a memory failure. A grader that punishes ordinary English is measuring
# phrasing, not retention.
LOSS_PHRASES = (
    "no longer have", "don't have that", "do not have that",
    "don't have the", "do not have the",
    "cannot recall", "can't recall", "unable to recall",
    "wasn't told", "was not told", "you never told",
    "could you remind me", "please remind me", "remind me what",
    "i have lost", "i've lost", "no record of",
)


def _not_lost(text):
    t = (text or "").lower()
    return not any(w in t for w in LOSS_PHRASES)


def g_m1(text, build):
    """One constraint, six turns."""
    return [
        ("recalled the exact limit", "200" in (text or "")),
        ("recalled what it was for", _said(text, "cart")),
        ("did not claim to have lost it", _not_lost(text)),
        _probe(build, "and applied it in code",
               "from shop import settings\n"
               "assert settings.MAX_CART_LINES == 200, settings.MAX_CART_LINES"),
    ]


def g_m2(text, build):
    """Two constraints, twelve turns, one of them revised mid-run. The revision
    is the point: retention alone gives 200, which is the superseded value."""
    t = text or ""
    return [
        ("recalled the REVISED limit, not the original", "250" in t and "200" not in t),
        ("recalled the second constraint too", _said(t, "usd")),
        ("did not claim to have lost them", _not_lost(t)),
        _probe(build, "applied the revised limit",
               "from shop import settings\n"
               "assert settings.MAX_CART_LINES == 250, settings.MAX_CART_LINES"),
        _probe(build, "applied the currency too",
               "from shop import settings\n"
               "assert settings.DEFAULT_CURRENCY == 'USD', settings.DEFAULT_CURRENCY"),
    ]


def g_m3(text, build):
    """As M2, but a checked-in document asserts the SUPERSEDED value. A harness
    whose memory has decayed will find 200 in docs/limits.md and use it with
    confidence. Same probes: the failure shows up as 200, not as a shrug."""
    rows = g_m2(text, build)
    # Quality control, not correctness: the tree is now self-contradictory. A
    # harness can finish, pass every probe above, and still leave a document
    # asserting the old number. Either fixing it or saying so counts.
    doc = os.path.join(build, "docs", "limits.md")
    body = ""
    if os.path.exists(doc):
        with open(doc, encoding="utf-8", errors="replace") as f:
            body = f.read()
    noticed = ("250" in body) or any(
        w in (text or "").lower() for w in ("docs/limits", "limits.md", "document",
                                            "documentation", "out of date", "stale",
                                            "contradict", "inconsistent"))
    rows.append(("noticed the document still says otherwise", noticed))
    return rows


# ── R: restore ───────────────────────────────────────────────────────────────
def _compare(build, pristine, only=None, ignore=()):
    """Byte-for-byte, not 'looks about right'."""
    missing, diffs, extra = [], [], []
    for root, dirs, files in os.walk(pristine):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            rel = os.path.relpath(os.path.join(root, name), pristine).replace("\\", "/")
            if only and rel not in only:
                continue
            if rel in ignore:
                continue
            dst = os.path.join(build, rel)
            if not os.path.exists(dst):
                missing.append(rel)
            elif not filecmp.cmp(os.path.join(root, name), dst, shallow=False):
                diffs.append(rel)
    for root, dirs, files in os.walk(build):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            rel = os.path.relpath(os.path.join(root, name), build).replace("\\", "/")
            if _is_harness_state(rel):
                continue
            if not os.path.exists(os.path.join(pristine, rel)):
                extra.append(rel)
    return missing, diffs, extra


def _names(items, limit=4):
    if not items:
        return ""
    shown = ", ".join(sorted(items)[:limit])
    more = f" +{len(items) - limit}" if len(items) > limit else ""
    return f" [{shown}{more}]"


DELETED = ("shop/reporting.py", "shop/promotions.py")


def g_r1(build, pristine):
    """Delete two, restore everything.

    The failing labels carry the offending filenames. A grader that reports only
    "nothing left behind" cannot be acted on without re-running the scenario,
    and re-running is the expensive thing this whole setup exists to avoid.
    """
    missing, diffs, extra = _compare(build, pristine)
    return [
        (f"every file restored{_names(missing)}", not missing),
        (f"every file byte-identical{_names(diffs)}", not diffs),
        (f"nothing left behind{_names(extra)}", not extra),
    ]


def g_r2(build, pristine):
    """Restore ONLY the deleted modules; the requested edits must survive.
    A harness that rolls the whole tree back scores 2 of 4 here, which is the
    distinction the tier exists to draw."""
    missing, diffs, _ = _compare(build, pristine, only=set(DELETED))
    rows = [
        (f"the deleted modules are back{_names(missing)}", not missing),
        (f"and byte-identical{_names(diffs)}", not diffs),
    ]
    rows.append(_probe(build, "the requested edit survived",
                       "from shop import catalog, pricing, cart\n"
                       "for m in (catalog, pricing, cart):\n"
                       "    cls = getattr(m, [n for n in dir(m) if n.endswith('Record')][0])\n"
                       "    assert 'note' in cls.__dataclass_fields__, m.__name__"))
    rows.append(_probe(build, "the tree still imports",
                       "from shop import reporting, promotions\n"
                       "assert reporting and promotions"))
    return rows


def g_r3(build, pristine):
    """As R2, plus an unrelated change made BETWEEN the destruction and the
    restore. A blanket undo takes that with it - which is the trap."""
    missing, diffs, _ = _compare(build, pristine, only=set(DELETED))
    return [
        (f"the deleted modules are back{_names(missing)}", not missing),
        (f"and byte-identical{_names(diffs)}", not diffs),
        _probe(build, "the later, unrelated change survived",
               "from shop import settings\n"
               "assert settings.MAX_CART_LINES == 200, settings.MAX_CART_LINES"),
        _probe(build, "the tree still imports",
               "from shop import reporting, promotions\n"
               "assert reporting and promotions"),
    ]


# ── the matrix ───────────────────────────────────────────────────────────────
SCENARIOS = [
    # N: navigation ----------------------------------------------------------
    {"id": "N1", "cat": "navigation", "tier": "hard",
     "name": "one wrong function, and the symptom names the module",
     "kind": "fix", "seed": seed_large, "turns": 1,
     "request": "Our accountants say the ledger stops balancing after some "
                "refunds. Full refunds are fine. Find the cause and fix it.",
     "grade": _g_partial_refund},

    {"id": "N2", "cat": "navigation", "tier": "very hard",
     "name": "same defect, 3x the surface, and no module named",
     "kind": "fix", "seed": seed_huge, "turns": 1,
     "request": "Finance is reporting that some of our numbers do not add up "
                "after a customer returns part of an order. It does not happen "
                "when they return everything. Track it down and fix it.",
     "grade": _g_partial_refund},

    {"id": "N3", "cat": "navigation", "tier": "crazy hard",
     "name": "as N2, but the notes point at the wrong file",
     "kind": "fix", "seed": seed_huge_misleading, "turns": 1,
     "request": "Finance is reporting that some of our numbers do not add up "
                "after a customer returns part of an order. It does not happen "
                "when they return everything. Track it down and fix it. Do not "
                "change behaviour that is currently correct.",
     "grade": g_n3},

    # M: memory --------------------------------------------------------------
    {"id": "M1", "cat": "memory", "tier": "hard",
     "name": "one constraint, six turns apart",
     "kind": "recall", "seed": seed_large, "turns": 6,
     "request": "Before we start: the cart limit is being raised to 200 items. "
                "Remember that, we will need it shortly. Do not change anything yet.",
     "filler": FILLER_SHORT,
     "followup": "Now apply the limit we agreed at the start: set it in the "
                 "settings module. What number are you setting, and what is it for?",
     "grade_recall": g_m1},

    {"id": "M2", "cat": "memory", "tier": "very hard",
     "name": "two constraints, twelve turns, one revised in the middle",
     "kind": "recall", "seed": seed_huge, "turns": 12,
     "request": "Two things to keep in mind for later, do not change anything "
                "yet. First, the cart limit is going up to 200 items. Second, "
                "we are switching the default currency to USD.",
     "filler": FILLER_LONG[:5] + [
         "One correction on the first thing I mentioned: make it 250, not 200. "
         "The second thing is unchanged. Still do not change anything yet.",
     ] + FILLER_LONG[5:],
     "followup": "Now apply both of the things I asked you to keep in mind, in "
                 "the settings module. Tell me the two values you are setting "
                 "and what each is for.",
     "grade_recall": g_m2},

    {"id": "M3", "cat": "memory", "tier": "crazy hard",
     "name": "as M2, and a checked-in doc asserts the superseded value",
     "kind": "recall", "seed": seed_huge_stale_doc, "turns": 12,
     "request": "Two things to keep in mind for later, do not change anything "
                "yet. First, the cart limit is going up to 200 items. Second, "
                "we are switching the default currency to USD.",
     "filler": FILLER_LONG[:5] + [
         "One correction on the first thing I mentioned: make it 250, not 200. "
         "The second thing is unchanged. Still do not change anything yet.",
     ] + FILLER_LONG[5:],
     "followup": "Now apply both of the things I asked you to keep in mind, in "
                 "the settings module. Tell me the two values you are setting "
                 "and what each is for.",
     "grade_recall": g_m3},

    # R: restore -------------------------------------------------------------
    {"id": "R1", "cat": "restore", "tier": "hard",
     "name": "delete two modules, put them back exactly",
     "kind": "restore", "seed": seed_large, "turns": 2,
     "request": "The reporting and promotions modules are dead weight - delete them.",
     "followup": "That was a mistake. Put everything back exactly as it was.",
     "grade_restore": g_r1},

    {"id": "R2", "cat": "restore", "tier": "very hard",
     "name": "delete two, edit three, put back only the two",
     "kind": "restore", "seed": seed_huge, "turns": 2,
     "request": "Two jobs. Delete the reporting and promotions modules, they are "
                "dead weight. Then add an optional note field, defaulting to an "
                "empty string, to the record dataclass in each of catalog, "
                "pricing and cart.",
     "followup": "Deleting those two modules was a mistake - put them back "
                 "exactly as they were. The note field stays, that part was right.",
     "grade_restore": g_r2},

    {"id": "R3", "cat": "restore", "tier": "crazy hard",
     "name": "an unrelated change lands between the damage and the repair",
     "kind": "restore", "seed": seed_huge, "turns": 3,
     "request": "The reporting and promotions modules are dead weight - delete them.",
     "filler": ["Separate job: raise the maximum number of items a cart can "
                "hold from 50 to 200, in the settings module."],
     "followup": "Going back to the first thing: deleting those two modules was "
                 "a mistake. Put them back exactly as they were. Everything I "
                 "asked for since then stays as it is.",
     "grade_restore": g_r3},
]

BY_ID = {s["id"]: s for s in SCENARIOS}
CATEGORIES = ("navigation", "memory", "restore")
TIERS = ("hard", "very hard", "crazy hard")


def select(ids=None, cats=None, tiers=None):
    want = {i.strip().upper() for i in (ids or []) if i.strip()}
    wcat = {c.strip().lower() for c in (cats or []) if c.strip()}
    wtier = {t.strip().lower() for t in (tiers or []) if t.strip()}
    out = []
    for s in SCENARIOS:
        if want and s["id"] not in want:
            continue
        if wcat and s["cat"] not in wcat and s["cat"][0] not in wcat:
            continue
        if wtier and s["tier"] not in wtier:
            continue
        out.append(s)
    return out
