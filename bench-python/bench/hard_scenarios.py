r"""Tiered scenarios against the billing seed: medium to extremely hard.

The original suite saturated — a weak model with cheap routing disabled scores
100% on every fix and refactor, so it cannot rank two agents, let alone rank
Axium against OpenClaw or Hermes. These are built to discriminate.

Difficulty is not "a bigger file". It is how much of the work is *finding*:

  medium          the symptom names the module. Read it, spot it, fix it.
  hard            the symptom names a number that is wrong. Two modules could
                  produce it; the agent has to reproduce before it can fix.
  very hard       the symptom is a category ("some customers"), the defect is a
                  missing CONDITION rather than a wrong value, and the obvious
                  reading of the code is that it is already correct.
  extremely hard  the symptom appears in a module that is not at fault, the
                  cause is two modules upstream, and a plausible local fix makes
                  the symptom disappear WITHOUT fixing the cause. Graded so that
                  the local fix scores zero.

Every grader runs the agent's code in a fresh subprocess, and every one FAILS on
the pristine seed (enforced by `--sanity`). None of them consults the agent's
prose: what it says it did is not evidence.
"""
import json
import os
import subprocess
import sys

TIMEOUT = 120

# Read-only comprehension gets the same guard as the small suite.
AWARE_PREFIX = (
    "READ-ONLY ANALYSIS. Do NOT create, edit or delete any file — investigate "
    "the code and answer the question. Name the SPECIFIC files and functions "
    "involved in your final answer.\n\nQUESTION: ")


def _probe(build, name, body):
    """Run `body` against the agent's code. Passes only on a clean exit."""
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
    """The project's own suite. Green on the pristine seed, so red means damage.

    Note this does NOT catch any planted defect — that is deliberate, and it is
    what stops the scenarios being solvable by running the tests.
    """
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
        broken = [ln.strip(" -") for ln in r.stdout.splitlines()
                  if ln.startswith("  - ")]
        return [(f"smoke: {b}", False) for b in broken] or \
               [("smoke suite exits clean", False)]
    except subprocess.TimeoutExpired:
        return [("smoke suite terminates", False)]


# ── H1 · hard ────────────────────────────────────────────────────────────────
# Proration credits against the wrong month's length. The symptom is a number
# that is wrong by a few euro, and BOTH proration functions are candidates.
def g_h1(build):
    return [
        _probe(build, "a same-plan switch nets zero across a month boundary",
               "from datetime import date\n"
               "from billing import proration\n"
               "n = proration.change_plan(4900, 4900, date(2026, 2, 5), date(2026, 1, 20))\n"
               "assert n == 0, f'expected 0, got {n}'"),
        _probe(build, "credit uses the period length, not the calendar month",
               "from datetime import date\n"
               "from billing import proration, money\n"
               "ps, cd = date(2026, 1, 20), date(2026, 2, 5)\n"
               "pe = proration.period_end_for(ps)\n"
               "rem = proration.days_remaining(cd, pe)\n"
               "want = money.apply_rate(4900, rem / (pe - ps).days)\n"
               "got = proration.unused_credit(4900, cd, ps)\n"
               "assert got == want, f'expected {want}, got {got}'"),
        _probe(build, "same-month proration still correct",
               "from datetime import date\n"
               "from billing import proration\n"
               "n = proration.change_plan(4900, 4900, date(2026, 1, 16), date(2026, 1, 1))\n"
               "assert n == 0, f'regressed the simple case: {n}'"),
        _probe(build, "an upgrade still costs more than a downgrade",
               "from datetime import date\n"
               "from billing import proration\n"
               "up = proration.change_plan(4900, 14900, date(2026, 2, 5), date(2026, 1, 20))\n"
               "down = proration.change_plan(14900, 4900, date(2026, 2, 5), date(2026, 1, 20))\n"
               "assert up > 0 > down, f'up={up} down={down}'"),
    ]


# ── H2 · very hard ───────────────────────────────────────────────────────────
# Reverse charge applied without checking the countries differ. The code reads
# as correct: it checks for a VAT number and for EU membership. The missing
# condition is the one nobody wrote down.
def g_h2(build):
    return [
        _probe(build, "domestic B2B carries full domestic VAT",
               "from billing import tax\n"
               "t = tax.compute_tax(10000, 'GR', 'GR', 'GR123456789')\n"
               "assert t.amount_minor == 2400, f'expected 2400, got {t.amount_minor} ({t.note})'"),
        _probe(build, "cross-border B2B is still reverse charge",
               "from billing import tax\n"
               "t = tax.compute_tax(10000, 'GR', 'DE', 'DE123456789')\n"
               "assert t.amount_minor == 0, f'broke reverse charge: {t.amount_minor}'"),
        _probe(build, "domestic consumer VAT unchanged",
               "from billing import tax\n"
               "assert tax.compute_tax(10000, 'GR', 'GR').amount_minor == 2400"),
        _probe(build, "non-EU buyer still untaxed",
               "from billing import tax\n"
               "assert tax.compute_tax(10000, 'GR', 'US', 'US99').amount_minor == 0"),
        _probe(build, "the predicate itself rejects same-country",
               "from billing import tax\n"
               "assert tax.is_reverse_charge('GR', 'GR', 'GR123') is False\n"
               "assert tax.is_reverse_charge('GR', 'DE', 'DE123') is True"),
    ]


# ── H3 · extremely hard ──────────────────────────────────────────────────────
# The symptom is a reconciliation discrepancy. `ledger.reconcile` REPORTS it,
# `engine` triggers it, and the cause is `gateway.is_retryable` two modules
# upstream. A plausible local fix — loosen the ledger threshold, or stop
# recording attempt rows — makes the discrepancy vanish while the customer is
# still hit three times. The last two checks exist to score that zero.
def g_h3(build):
    setup = ("from billing import audit, engine, invoice\n"
             "class Decline:\n"
             "    def __init__(self, code): self.code, self.calls = code, 0\n"
             "    def charge(self, i, a, c):\n"
             "        self.calls += 1\n"
             "        return {'ok': False, 'code': self.code}\n")
    return [
        _probe(build, "a hard decline is attempted exactly once",
               setup +
               "c = Decline('card_stolen')\n"
               "log = audit.AuditLog()\n"
               "engine.BillingRun(c, log).process(invoice.for_subscription('X', 'a', 'starter'))\n"
               "assert c.calls == 1, f'gateway called {c.calls} times for a stolen card'"),
        _probe(build, "a soft decline is still retried",
               setup +
               "class Soft(Decline):\n"
               "    def charge(self, i, a, c):\n"
               "        self.calls += 1\n"
               "        return {'ok': False, 'code': 'issuer_unavailable'}\n"
               "c = Soft('issuer_unavailable')\n"
               "log = audit.AuditLog()\n"
               "engine.BillingRun(c, log).process(invoice.for_subscription('X', 'a', 'starter'))\n"
               "assert c.calls == 3, f'stopped retrying a soft decline: {c.calls}'"),
        _probe(build, "a soft decline that recovers still succeeds",
               "from billing import audit, engine, invoice\n"
               "class Flaky:\n"
               "    def __init__(self): self.n = 0\n"
               "    def charge(self, i, a, c):\n"
               "        self.n += 1\n"
               "        return {'ok': True, 'reference': 'R'} if self.n > 1 else "
               "{'ok': False, 'code': 'issuer_unavailable'}\n"
               "log = audit.AuditLog()\n"
               "run = engine.BillingRun(Flaky(), log)\n"
               "r = run.process(invoice.for_subscription('X', 'a', 'starter'))\n"
               "assert r.ok and not run.discrepancies()"),
        _probe(build, "no reconciliation discrepancy remains",
               setup +
               "log = audit.AuditLog()\n"
               "run = engine.BillingRun(Decline('card_stolen'), log)\n"
               "run.process(invoice.for_subscription('X', 'a', 'starter'))\n"
               "d = run.discrepancies()\n"
               "assert not d, f'still discrepant: {d}'"),
        # The two anti-cheat checks. A fix that silences the report without
        # stopping the retries fails BOTH of these while passing the one above.
        _probe(build, "the audit log still records every attempt",
               setup +
               "log = audit.AuditLog()\n"
               "engine.BillingRun(Decline('issuer_unavailable'), log)"
               ".process(invoice.for_subscription('Y', 'a', 'starter'))\n"
               "n = log.count('Y', 'charge_attempt')\n"
               "assert n == 3, f'attempt rows were suppressed rather than prevented: {n}'"),
        _probe(build, "the ledger still detects a genuine double charge",
               "from billing import audit, ledger\n"
               "log = audit.AuditLog()\n"
               "for _ in range(2):\n"
               "    log.record('Z', 'charge_attempt', {'ok': True})\n"
               "d = ledger.reconcile(log, 'Z', True)\n"
               "assert d, 'the ledger was loosened until it detects nothing'"),
    ]


# ── H4 · medium ──────────────────────────────────────────────────────────────
# A feature with a stated invariant. Medium because the symptom names the
# module and the invariant is written in the docstring.
def g_h4(build):
    return [
        _probe(build, "a zero-decimal currency formats without decimals",
               "from billing import money\n"
               "s = money.format_money(1200, 'JPY')\n"
               "assert '.' not in s, f'JPY rendered with decimals: {s}'"),
        _probe(build, "a three-decimal currency keeps three",
               "from billing import money\n"
               "assert money.to_minor('1.234', 'BHD') == 1234"),
        _probe(build, "splitting stays lossless for every part count",
               "from billing import money\n"
               "for total in (100, 101, 999, -101):\n"
               "    for parts in range(1, 8):\n"
               "        s = money.split_evenly(total, parts)\n"
               "        assert sum(s) == total, (total, parts, s)"),
    ]


# ── H5 · very hard, read-only ────────────────────────────────────────────────
def g_h5_answer(build, answer):
    a = (answer or "").lower()
    return [
        ("named the gateway as the cause", "gateway" in a or "is_retryable" in a),
        ("named the retry predicate", "retryable" in a or "retry" in a),
        ("connected it to reconciliation",
         any(w in a for w in ("reconcil", "ledger", "discrepan", "audit"))),
        ("did not blame the ledger itself",
         not any(p in a for p in ("bug is in the ledger", "ledger is wrong",
                                  "fix the ledger", "ledger.py is at fault"))),
    ]


SCENARIOS = [
    {"id": "H4", "name": "currency rounding invariants", "kind": "fix",
     "tier": "medium", "difficulty": 2, "seed": "hard",
     "request": "Our Japanese invoices show amounts like ¥1200.00 — JPY has no "
                "minor units and should render as ¥1200. Bahraini invoices are "
                "wrong too. Fix the money module so every currency uses its own "
                "number of decimal places.",
     "grade": g_h4},

    {"id": "H1", "name": "proration across a month boundary", "kind": "fix",
     "tier": "hard", "difficulty": 3, "seed": "hard",
     "request": "A customer on the Team plan changed plans on 5 February. Their "
                "billing period had started on 20 January. They were credited "
                "about €2.50 more than they should have been. Customers whose "
                "period starts and ends inside one month are billed correctly. "
                "Find the cause and fix it.",
     "grade": g_h1},

    {"id": "H2", "name": "reverse charge applied domestically", "kind": "fix",
     "tier": "very hard", "difficulty": 4, "seed": "hard",
     "request": "Our accountant says we have been under-collecting VAT on some "
                "Greek business customers. Greek consumers are charged correctly, "
                "and so is every customer outside Greece. Work out which "
                "customers are affected and fix it.",
     "grade": g_h2},

    {"id": "H3", "name": "retry storm on hard declines", "kind": "fix",
     "tier": "extremely hard", "difficulty": 5, "seed": "hard",
     "request": "The nightly billing run is producing reconciliation "
                "discrepancies, and one customer complained their stolen card was "
                "hit three times. Reconciliation is where we noticed it. Find the "
                "real cause and fix it there — do not change the reconciliation "
                "thresholds or stop recording audit rows, both of those hide the "
                "problem rather than solving it.",
     "grade": g_h3},

    {"id": "H5", "name": "trace a symptom to its cause", "kind": "aware",
     "tier": "very hard", "difficulty": 4, "seed": "hard",
     "request": "The billing run reports reconciliation discrepancies for "
                "invoices that failed to charge. Which module is actually at "
                "fault, which function, and why does the symptom appear "
                "somewhere else?",
     "grade_answer": g_h5_answer},
]

BY_ID = {s["id"]: s for s in SCENARIOS}
TIERS = ("medium", "hard", "very hard", "extremely hard")


def select(ids=None, tier=None):
    out = SCENARIOS
    if tier:
        out = [s for s in out if s["tier"] == tier]
    want = {i.strip().upper() for i in (ids or []) if i.strip()}
    if want:
        out = [s for s in out if s["id"] in want]
    return out
