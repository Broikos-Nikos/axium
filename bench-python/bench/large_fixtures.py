r"""A seed large enough that context handling is the bottleneck.

The billing seed is 500 lines. Both DeepSeek models read essentially all of it
and solved every scenario, which is why the tiered suite saturated: at that size
there is no navigation problem, only a reading problem, and frontier models read
well.

This one is ~2,900 lines across 14 modules, with the signal buried in bulk that
looks exactly like the rest of the file. The work is no longer "understand this
function", it is "find the one that matters among two hundred that do not". That
is a harness property — how it searches, what it keeps in context, whether it
re-reads — rather than a model property, which is the whole point.

The bulk is generated (`_large_seed.py`) and deliberately repetitive: fourteen
near-identical helpers per module. A harness that greps converges quickly. One
that reads files whole burns its budget on boilerplate, and the budget is what
catches it.

Two things are hand-written and matter:

  `shop/ledger.py`      holds the defect the X-LOCATE scenario hunts.
  `shop/settings.py`    holds the constant X-SPREAD must change everywhere.

Same invariants as the other seeds: `tests/smoke.py` is green on a pristine copy
and stays green with every defect present.
"""
import os
import shutil

from bench._large_seed import BULK

FILES = dict(BULK)

# The needle. One function among ~200 whose behaviour is wrong, in a module that
# looks like all the others. Nothing in its name or docstring flags it.
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

    def total_debits(self):
        return sum(e.debit for e in self.entries)

    def total_credits(self):
        return sum(e.credit for e in self.entries)


def post_sale(journal, net_minor, tax_minor, account="revenue"):
    """Post a sale: cash in, revenue and tax out."""
    journal.add("cash", debit=net_minor + tax_minor, ref="sale")
    journal.add(account, credit=net_minor, ref="sale")
    journal.add("tax_payable", credit=tax_minor, ref="sale")
    return journal


def post_refund(journal, net_minor, tax_minor, account="revenue"):
    """Reverse a sale."""
    journal.add("cash", credit=net_minor + tax_minor, ref="refund")
    journal.add(account, debit=net_minor, ref="refund")
    journal.add("tax_payable", debit=tax_minor, ref="refund")
    return journal


def post_partial_refund(journal, net_minor, tax_minor, fraction, account="revenue"):
    """Refund part of a sale.

    `fraction` is the proportion being returned, 0.0 to 1.0.
    """
    part_net = int(net_minor * fraction)
    part_tax = int(tax_minor * fraction)
    journal.add("cash", credit=part_net + part_tax, ref="partial_refund")
    journal.add(account, debit=part_net, ref="partial_refund")
    # DEFECT (X-LOCATE): the tax leg reverses the FULL tax, not the fraction, so
    # a partial refund over-reverses tax_payable and the journal stops balancing.
    # Every full refund is correct, which is why the smoke suite passes.
    journal.add("tax_payable", debit=tax_minor, ref="partial_refund")
    return journal


def trial_balance(journals):
    """Aggregate net movement per account across journals."""
    out = {}
    for j in journals:
        for account, net in j.net_by_account().items():
            out[account] = out.get(account, 0) + net
    return out
'''

# The constant X-SPREAD must change everywhere it is used. Deliberately imported
# by six modules under three different aliases, so find-and-replace on one name
# misses most of them.
FILES["shop/settings.py"] = '''"""Runtime settings."""

# Maximum number of line items a single cart may hold.
MAX_CART_LINES = 50

DEFAULT_CURRENCY = "EUR"
SESSION_MINUTES = 30
PAGE_SIZE = 25
'''

FILES["shop/limits.py"] = '''"""Limit checks used across the checkout path."""
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
    return {
        "lines": line_count,
        "limit": CART_LIMIT,
        "remaining": remaining_slots(line_count),
        "full": line_count >= CART_LIMIT,
    }


def limit_message():
    return f"You can add up to {CART_LIMIT} items."
'''

FILES["shop/bulk.py"] = '''"""Bulk import."""
from shop import settings as cfg


def chunk_for_import(items):
    """Split an import into cart-sized chunks."""
    size = cfg.MAX_CART_LINES
    return [items[i:i + size] for i in range(0, len(items), size)]


def import_capacity(carts):
    return carts * cfg.MAX_CART_LINES
'''

FILES["tests/smoke.py"] = '''"""Happy-path smoke tests. Green on a correct build."""
import sys

sys.path.insert(0, ".")

from shop import ledger, limits, validation, api, bulk, settings

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
check("a full refund nets to zero",
      all(v == 0 for v in j2.net_by_account().values()))

check("cart limit", limits.cart_is_full(settings.MAX_CART_LINES))
check("remaining slots", limits.remaining_slots(10) == settings.MAX_CART_LINES - 10)
check("validation passes under the limit", not validation.validate_cart([1] * 10))
check("api reports the limit", api.cart_status(0)["limit"] == settings.MAX_CART_LINES)
check("bulk chunks to the limit",
      len(bulk.chunk_for_import(list(range(settings.MAX_CART_LINES + 1)))) == 2)

if FAILURES:
    print("SMOKE FAILURES:")
    for f in FAILURES:
        print("  -", f)
    raise SystemExit(1)
print("smoke suite clean")
'''

FILES["README.md"] = '''# shop

Large e-commerce backend. Fourteen domain modules plus a double-entry ledger.

    python tests/smoke.py
'''


def generate(dest):
    if os.path.exists(dest):
        shutil.rmtree(dest, ignore_errors=True)
    for rel, body in FILES.items():
        path = os.path.join(dest, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(body)
    return dest


def file_count():
    return len(FILES)


def line_count():
    return sum(b.count("\n") + 1 for b in FILES.values())
