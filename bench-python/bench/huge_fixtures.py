r"""~8,000-line seed for the very-hard and crazy-hard tiers.

The 2,834-line seed still let a frontier model read most of the tree. At ~8,000
lines across 30 modules that stops being affordable, so the harness's search
strategy becomes the cost driver rather than the model's reading speed - which
is the property the benchmark is trying to price.

The bulk (`_huge_seed.py`) is 24 near-identical domain modules, 18 lookalike
helpers each. Repetitive on purpose: it is chaff, and a harness that greps
converges while one that reads whole files pays for every line of it.

Hand-written, and where all the signal lives:

    shop/ledger.py     N-tier navigation defect (partial refunds)
    shop/settings.py   C-tier constant, imported 12 ways
    shop/pipeline.py   I-tier interaction pair, deliberately far apart
    tests/smoke.py     green on pristine AND with every defect present

Same invariants as every other seed: smoke green on a pristine copy, every
grader red there.
"""
import os
import shutil

from bench._huge_seed import BULK

FILES = dict(BULK)

# ── N tier: the needle ───────────────────────────────────────────────────────
# Same defect class as the 2,834-line seed but buried four times deeper: the
# partial-refund tax leg reverses the FULL tax rather than the fraction.
FILES["shop/ledger.py"] = '''"""Double-entry ledger."""
from dataclasses import dataclass, field


@dataclass
class Entry:
    account: str
    debit: int = 0
    credit: int = 0
    ref: str = ""

    def net(self):
        return self.debit - self.credit


@dataclass
class Journal:
    ident: str
    entries: list = field(default_factory=list)

    def add(self, account, debit=0, credit=0, ref=""):
        self.entries.append(Entry(account, debit, credit, ref))
        return self

    def balance(self):
        """A journal balances when total debits equal total credits."""
        return sum(e.debit for e in self.entries) == sum(e.credit for e in self.entries)

    def net_by_account(self):
        out = {}
        for e in self.entries:
            out[e.account] = out.get(e.account, 0) + e.net()
        return out


def post_sale(journal, net_minor, tax_minor, account="revenue"):
    """Post a sale: cash in, revenue and tax out."""
    journal.add("cash", debit=net_minor + tax_minor, ref="sale")
    journal.add(account, credit=net_minor, ref="sale")
    journal.add("tax_payable", credit=tax_minor, ref="sale")
    return journal


def post_refund(journal, net_minor, tax_minor, account="revenue"):
    """Reverse a sale in full."""
    journal.add("cash", credit=net_minor + tax_minor, ref="refund")
    journal.add(account, debit=net_minor, ref="refund")
    journal.add("tax_payable", debit=tax_minor, ref="refund")
    return journal


def post_partial_refund(journal, net_minor, tax_minor, fraction, account="revenue"):
    """Refund part of a sale. `fraction` is the proportion returned, 0.0-1.0."""
    part_net = int(net_minor * fraction)
    part_tax = int(tax_minor * fraction)
    journal.add("cash", credit=part_net + part_tax, ref="partial_refund")
    journal.add(account, debit=part_net, ref="partial_refund")
    # DEFECT (N): full tax reversed on a PARTIAL refund. Full refunds are
    # correct, which is why the smoke suite never catches it.
    journal.add("tax_payable", debit=tax_minor, ref="partial_refund")
    return journal


def trial_balance(journals):
    out = {}
    for j in journals:
        for account, net in j.net_by_account().items():
            out[account] = out.get(account, 0) + net
    return out
'''

# The stale docstring the crazy-hard N tier uses as contradictory evidence: it
# names the WRONG function as the recent change, sending a careless search to an
# innocent file that is genuinely correct.
FILES["shop/reconcile.py"] = '''"""Reconciliation checks.

NOTE (2026-07): the rounding in `settle_batch` below was changed to fix a
balancing complaint from finance. If refund totals look wrong, start here.
"""
from shop import ledger


def settle_batch(journals):
    """Settle a batch of journals. This function is correct."""
    unbalanced = [j.ident for j in journals if not j.balance()]
    return {"settled": [j.ident for j in journals if j.balance()],
            "unbalanced": unbalanced}


def check_account(journals, account):
    return ledger.trial_balance(journals).get(account, 0)
'''

# ── C tier: one constant, twelve ways ────────────────────────────────────────
FILES["shop/settings.py"] = '''"""Runtime settings."""

# Maximum number of line items a single cart may hold.
MAX_CART_LINES = 50

DEFAULT_CURRENCY = "EUR"
SESSION_MINUTES = 30
PAGE_SIZE = 25
'''

FILES["shop/limits.py"] = '''"""Limit checks."""
from shop.settings import MAX_CART_LINES


def cart_is_full(line_count):
    return line_count >= MAX_CART_LINES


def remaining_slots(line_count):
    return max(0, MAX_CART_LINES - line_count)
'''

FILES["shop/validation.py"] = '''"""Input validation."""
from shop import settings


def validate_cart(lines):
    problems = []
    if len(lines) > settings.MAX_CART_LINES:
        problems.append(f"cart has {len(lines)} lines, limit is {settings.MAX_CART_LINES}")
    return problems


def describe_limit():
    return f"up to {settings.MAX_CART_LINES} items per cart"
'''

FILES["shop/api.py"] = '''"""HTTP-facing helpers."""
from shop.settings import MAX_CART_LINES as CART_LIMIT
from shop.limits import remaining_slots


def cart_status(line_count):
    return {"lines": line_count, "limit": CART_LIMIT,
            "remaining": remaining_slots(line_count),
            "full": line_count >= CART_LIMIT}


def limit_message():
    return f"You can add up to {CART_LIMIT} items."
'''

FILES["shop/bulkimport.py"] = '''"""Bulk import."""
from shop import settings as cfg


def chunk_for_import(items):
    size = cfg.MAX_CART_LINES
    return [items[i:i + size] for i in range(0, len(items), size)]


def import_capacity(carts):
    return carts * cfg.MAX_CART_LINES
'''

# The two sites a name-based search misses: a default ARGUMENT and a string
# LITERAL in user-facing copy.
FILES["shop/quotes.py"] = '''"""Quotation builder."""
from shop import settings


def build_quote(lines, max_lines=50):
    """Build a quote. `max_lines` defaults to the cart limit."""
    return {"lines": lines[:max_lines], "truncated": len(lines) > max_lines,
            "limit": max_lines}


def quote_help_text():
    return "Quotes are capped at 50 line items, matching the cart limit."


def limit_from_settings():
    return settings.MAX_CART_LINES
'''

# ── I tier: the interaction pair, deliberately far apart ─────────────────────
FILES["shop/pipeline.py"] = '''"""Order pipeline: normalise, then apply adjustments."""


def normalise_amounts(lines):
    """Round every line to whole minor units.

    Correct in isolation: rounding half up is the house convention.
    """
    out = []
    for line in lines:
        out.append({**line, "amount": int(line["amount"] + 0.5)})
    return out


def apply_pipeline(lines, adjuster):
    """Normalise, then adjust. Correct in isolation."""
    return adjuster(normalise_amounts(lines))
'''

FILES["shop/adjustments.py"] = '''"""Adjustments applied after normalisation."""


def proportional_discount(lines, pct=10):
    """Take `pct` off every line.

    Correct in isolation: it rounds its own result. Composed AFTER
    normalise_amounts, the two roundings compound and the total drifts.
    """
    return [{**l, "amount": int(l["amount"] * (100 - pct) / 100 + 0.5)} for l in lines]


def exact_proportional_discount(lines, pct=10):
    """Discount computed from the pre-rounded total, then distributed."""
    total = sum(l["amount"] for l in lines)
    target = int(total * (100 - pct) / 100 + 0.5)
    out, running = [], 0
    for i, l in enumerate(lines):
        if i == len(lines) - 1:
            amt = target - running
        else:
            amt = int(l["amount"] * (100 - pct) / 100 + 0.5)
            running += amt
        out.append({**l, "amount": amt})
    return out
'''

FILES["tests/smoke.py"] = '''"""Happy-path smoke tests. Green on a correct build."""
import sys

sys.path.insert(0, ".")

from shop import ledger, limits, validation, api, bulkimport, settings, quotes
from shop import pipeline, adjustments

FAILURES = []


def check(name, cond):
    if not cond:
        FAILURES.append(name)


j = ledger.Journal("J1")
ledger.post_sale(j, 10000, 2400)
check("a sale balances", j.balance())

j2 = ledger.Journal("J2")
ledger.post_sale(j2, 10000, 2400)
ledger.post_refund(j2, 10000, 2400)
check("a full refund balances", j2.balance())
check("a full refund nets to zero", all(v == 0 for v in j2.net_by_account().values()))

check("cart limit", limits.cart_is_full(settings.MAX_CART_LINES))
check("remaining slots", limits.remaining_slots(10) == settings.MAX_CART_LINES - 10)
check("validation under the limit", not validation.validate_cart([1] * 10))
check("api limit", api.cart_status(0)["limit"] == settings.MAX_CART_LINES)
check("bulk chunks", len(bulkimport.chunk_for_import(list(range(settings.MAX_CART_LINES + 1)))) == 2)
check("quote limit", quotes.limit_from_settings() == settings.MAX_CART_LINES)

# A single line composes cleanly, so the interaction defect stays hidden.
one = pipeline.apply_pipeline([{"amount": 100.0}], adjustments.proportional_discount)
check("single-line pipeline", one[0]["amount"] == 90)

if FAILURES:
    print("SMOKE FAILURES:")
    for f in FAILURES:
        print("  -", f)
    raise SystemExit(1)
print("smoke suite clean")
'''

FILES["shop/__init__.py"] = '"""Large e-commerce backend."""\n__version__ = "6.0.0"\n'
FILES["README.md"] = '''# shop

Large e-commerce backend. Thirty modules.

    python tests/smoke.py
'''


# The misleading note is a VARIANT, not part of the base seed. The very-hard
# tier should be hard because the surface is large; only the crazy-hard tier
# adds evidence that actively points the wrong way.
CLEAN_RECONCILE = FILES["shop/reconcile.py"].replace(
    """Reconciliation checks.

NOTE (2026-07): the rounding in `settle_batch` below was changed to fix a
balancing complaint from finance. If refund totals look wrong, start here.
""",
    """Reconciliation checks.""")


def generate(dest, misleading=False, stale_doc=None):
    """Write the seed.

    misleading: keep the stale note in reconcile.py that names an innocent,
        correct function as the recent change. Crazy-hard N tier only.
    stale_doc: text for docs/limits.md. Used by the crazy-hard M tier to plant
        a SUPERSEDED value where a harness with a decayed memory will find it
        and believe it - the wrong answer has to be discoverable, or forgetting
        just looks like uncertainty rather than confident error.
    """
    if os.path.exists(dest):
        shutil.rmtree(dest, ignore_errors=True)
    files = dict(FILES)
    if not misleading:
        files["shop/reconcile.py"] = CLEAN_RECONCILE
    if stale_doc:
        files["docs/limits.md"] = stale_doc
    for rel, body in files.items():
        path = os.path.join(dest, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(body)
    return dest


def file_count():
    return len(FILES)


def line_count():
    return sum(b.count("\n") + 1 for b in FILES.values())
