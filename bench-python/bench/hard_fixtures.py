r"""A seed project big enough to hide a bug in.

The original seed is nine small, clean files. Every scenario against it is
solvable by reading one function, which is why a weak model with cheap routing
disabled still scores 100% on it: the suite cannot tell a good agent from a
lucky one.

This one is built to the opposite spec, taking the design from
`playground/bllm/evals/fixbench_real.py`, which hit the same saturation and
solved it:

    "Two things make it hard. The file is hundreds of lines, so the bug has to
     be found before it can be fixed, and there is no failing test to follow,
     only a description of the symptom."

So: a billing engine of ~900 lines across nine modules, with real domain
tangle — proration, tax jurisdictions, currency rounding, retries, an audit
log. The planted defects are not typos. Each one is a *plausible line* that is
wrong only in a case the obvious test does not cover, and finding it means
reproducing the symptom first.

Invariants, enforced by `--sanity` exactly as for the small seed:

  1. `tests/smoke.py` PASSES on the pristine seed. It covers the happy paths a
     developer would have written, and deliberately NOT the edge cases the
     defects live in — that is what makes them survive to be found.
  2. Every grader FAILS on the pristine seed.

The gap between 1 and 2 is the whole point: a suite whose own tests catch the
bug is measuring nothing but whether the agent ran the tests.
"""
import os
import shutil

FILES = {}

FILES["billing/__init__.py"] = '''"""Subscription billing engine."""
__version__ = "2.4.0"
'''

FILES["billing/money.py"] = '''"""Money as integer minor units.

Floats are not money. Every amount in this system is an int of minor units
(cents), and the ONLY places a float is allowed are the tax-rate multiplication
and the proration ratio, both of which round back immediately.
"""
from decimal import Decimal, ROUND_HALF_UP

CURRENCIES = {
    "EUR": {"minor": 2, "symbol": "\\u20ac"},
    "USD": {"minor": 2, "symbol": "$"},
    "JPY": {"minor": 0, "symbol": "\\u00a5"},
    "BHD": {"minor": 3, "symbol": "BD"},
}


def minor_units(currency):
    return CURRENCIES.get(currency, {"minor": 2})["minor"]


def to_minor(amount, currency="EUR"):
    """A decimal string or number to integer minor units."""
    exp = Decimal(1).scaleb(-minor_units(currency))
    return int(Decimal(str(amount)).quantize(exp, rounding=ROUND_HALF_UP)
               .scaleb(minor_units(currency)))


def from_minor(units, currency="EUR"):
    return Decimal(units).scaleb(-minor_units(currency))


def format_money(units, currency="EUR"):
    d = from_minor(units, currency)
    sym = CURRENCIES.get(currency, {"symbol": ""})["symbol"]
    return f"{sym}{d:,.2f}"


def apply_rate(units, rate):
    """Multiply minor units by a rate, rounding half up to whole units."""
    return int(Decimal(units * Decimal(str(rate))).quantize(Decimal(1),
                                                            rounding=ROUND_HALF_UP))


def split_evenly(units, parts):
    """Split `units` into `parts`, distributing the remainder.

    The sum of the result must always equal `units` exactly. A naive
    `units // parts` loses money to rounding on every split.
    """
    if parts <= 0:
        raise ValueError("parts must be positive")
    base, rem = divmod(abs(units), parts)
    sign = -1 if units < 0 else 1
    out = [sign * (base + (1 if i < rem else 0)) for i in range(parts)]
    return out
'''

FILES["billing/plans.py"] = '''"""Plan catalogue and price lookup."""
from dataclasses import dataclass, field


@dataclass(frozen=True)
class Plan:
    code: str
    name: str
    monthly_minor: int
    currency: str = "EUR"
    included_seats: int = 1
    extra_seat_minor: int = 0
    annual_discount_pct: int = 0
    metered: bool = False


CATALOGUE = {
    "starter": Plan("starter", "Starter", 900, "EUR", 1, 0),
    "team": Plan("team", "Team", 4900, "EUR", 5, 800, 10),
    "business": Plan("business", "Business", 14900, "EUR", 20, 600, 15),
    "enterprise": Plan("enterprise", "Enterprise", 49900, "EUR", 100, 400, 20),
    "metered": Plan("metered", "Pay as you go", 0, "EUR", 1, 0, 0, True),
}


def get_plan(code):
    plan = CATALOGUE.get(code)
    if plan is None:
        raise KeyError(f"unknown plan: {code}")
    return plan


def seat_charge(plan, seats):
    """Charge for seats beyond the included allowance."""
    extra = max(0, seats - plan.included_seats)
    return extra * plan.extra_seat_minor


def annual_price(plan, seats=1):
    """Twelve months, less the annual discount."""
    monthly = plan.monthly_minor + seat_charge(plan, seats)
    gross = monthly * 12
    if plan.annual_discount_pct:
        return gross - (gross * plan.annual_discount_pct) // 100
    return gross
'''

# DEFECT H1 (hard): proration uses the CURRENT month's length for every period,
# so an upgrade in February credits 28 days against a 31-day month. Correct for
# same-length months, wrong by up to 10% otherwise, and invisible unless the
# test crosses a month boundary with different lengths.
FILES["billing/proration.py"] = '''"""Mid-cycle plan changes."""
import calendar
from datetime import date

from billing import money


def days_in_month(year, month):
    return calendar.monthrange(year, month)[1]


def days_remaining(on, period_end):
    """Whole days left in the period, not counting the day of change."""
    return max(0, (period_end - on).days)


def period_end_for(start):
    """The day before the same day next month."""
    year, month = start.year, start.month
    if month == 12:
        year, month = year + 1, 1
    else:
        month += 1
    day = min(start.day, days_in_month(year, month))
    return date(year, month, day)


def unused_credit(old_monthly_minor, change_date, period_start):
    """Credit for the unused remainder of the current period."""
    period_end = period_end_for(period_start)
    remaining = days_remaining(change_date, period_end)
    span = days_in_month(change_date.year, change_date.month)
    if span <= 0:
        return 0
    return money.apply_rate(old_monthly_minor, remaining / span)


def prorated_charge(new_monthly_minor, change_date, period_start):
    """Charge for the remainder of the period on the new plan."""
    period_end = period_end_for(period_start)
    remaining = days_remaining(change_date, period_end)
    span = (period_end - period_start).days
    if span <= 0:
        return 0
    return money.apply_rate(new_monthly_minor, remaining / span)


def change_plan(old_monthly_minor, new_monthly_minor, change_date, period_start):
    """Net amount due when switching plans mid-period.

    Positive means the customer owes; negative means they are in credit.
    """
    credit = unused_credit(old_monthly_minor, change_date, period_start)
    charge = prorated_charge(new_monthly_minor, change_date, period_start)
    return charge - credit
'''

# DEFECT H2 (very hard): the reverse-charge rule is applied when the customer
# has a VAT number, without checking the countries DIFFER. A domestic B2B sale
# with a VAT number is zero-rated when it should carry full domestic VAT. Every
# cross-border test passes; only same-country B2B reveals it.
FILES["billing/tax.py"] = '''"""VAT and sales tax."""
from billing import money

EU = {"AT", "BE", "BG", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FR", "GR",
      "HR", "HU", "IE", "IT", "LT", "LU", "LV", "MT", "NL", "PL", "PT", "RO",
      "SE", "SI", "SK"}

VAT_RATES = {
    "GR": 0.24, "DE": 0.19, "FR": 0.20, "IE": 0.23, "NL": 0.21,
    "ES": 0.21, "IT": 0.22, "PT": 0.23, "BE": 0.21, "AT": 0.20,
}

DEFAULT_EU_RATE = 0.21


class TaxLine:
    def __init__(self, rate, amount_minor, note=""):
        self.rate = rate
        self.amount_minor = amount_minor
        self.note = note

    def __repr__(self):
        return f"TaxLine(rate={self.rate}, amount={self.amount_minor}, note={self.note!r})"


def rate_for(country):
    if country in VAT_RATES:
        return VAT_RATES[country]
    if country in EU:
        return DEFAULT_EU_RATE
    return 0.0


def is_reverse_charge(seller_country, buyer_country, buyer_vat_number):
    """B2B cross-border inside the EU shifts the VAT liability to the buyer."""
    if not buyer_vat_number:
        return False
    if seller_country not in EU or buyer_country not in EU:
        return False
    return True


def compute_tax(net_minor, seller_country, buyer_country, buyer_vat_number=None):
    if is_reverse_charge(seller_country, buyer_country, buyer_vat_number):
        return TaxLine(0.0, 0, "reverse charge")
    rate = rate_for(buyer_country)
    if rate == 0.0:
        return TaxLine(0.0, 0, "no tax")
    return TaxLine(rate, money.apply_rate(net_minor, rate), f"VAT {int(rate * 100)}%")
'''

# DEFECT H3 (extremely hard): the retry loop treats any non-2xx as retryable,
# so a hard decline (card stolen) is retried three times. Each retry appends an
# audit row, and the ledger reconciles on ROW COUNT, so the invoice is recorded
# as paid three times over. The symptom surfaces two modules away, in
# reconciliation, and only for declines whose code is in HARD_DECLINES.
FILES["billing/gateway.py"] = '''"""Payment gateway client with retries."""
import time

HARD_DECLINES = {"card_stolen", "card_lost", "fraudulent", "do_not_honour"}
SOFT_DECLINES = {"insufficient_funds", "issuer_unavailable", "try_again_later"}
MAX_ATTEMPTS = 3
BACKOFF_SECONDS = 0.0


class GatewayError(Exception):
    def __init__(self, code, message=""):
        super().__init__(message or code)
        self.code = code


class Charge:
    def __init__(self, ok, reference="", code="", attempts=1):
        self.ok = ok
        self.reference = reference
        self.code = code
        self.attempts = attempts


def is_retryable(code):
    """Whether a failed charge is worth another attempt."""
    # Anything the gateway gave us a code for is a response we can retry.
    return bool(code)


def charge(client, invoice_id, amount_minor, currency="EUR", audit=None):
    """Attempt a charge, retrying transient failures."""
    last_code = ""
    for attempt in range(1, MAX_ATTEMPTS + 1):
        result = client.charge(invoice_id, amount_minor, currency)
        if audit is not None:
            audit.record(invoice_id, "charge_attempt", {
                "attempt": attempt, "amount": amount_minor, "ok": bool(result.get("ok")),
                "code": result.get("code", ""),
            })
        if result.get("ok"):
            return Charge(True, result.get("reference", ""), attempts=attempt)
        last_code = result.get("code", "")
        if not is_retryable(last_code):
            break
        if attempt < MAX_ATTEMPTS and BACKOFF_SECONDS:
            time.sleep(BACKOFF_SECONDS)
    return Charge(False, code=last_code, attempts=attempt)
'''

FILES["billing/audit.py"] = '''"""Append-only audit log."""
import json
import time


class AuditLog:
    def __init__(self):
        self.rows = []

    def record(self, invoice_id, event, payload=None):
        self.rows.append({
            "ts": time.time(),
            "invoice_id": invoice_id,
            "event": event,
            "payload": payload or {},
        })
        return len(self.rows)

    def for_invoice(self, invoice_id):
        return [r for r in self.rows if r["invoice_id"] == invoice_id]

    def count(self, invoice_id, event):
        return sum(1 for r in self.rows
                   if r["invoice_id"] == invoice_id and r["event"] == event)

    def dump(self):
        return json.dumps(self.rows, indent=2, default=str)
'''

FILES["billing/ledger.py"] = '''"""Reconciliation between charges and the audit log."""


class Discrepancy:
    def __init__(self, invoice_id, kind, detail):
        self.invoice_id = invoice_id
        self.kind = kind
        self.detail = detail

    def __repr__(self):
        return f"Discrepancy({self.invoice_id}, {self.kind}, {self.detail!r})"


def successful_attempts(audit, invoice_id):
    return sum(1 for r in audit.for_invoice(invoice_id)
               if r["event"] == "charge_attempt" and r["payload"].get("ok"))


def failed_attempts(audit, invoice_id):
    return sum(1 for r in audit.for_invoice(invoice_id)
               if r["event"] == "charge_attempt" and not r["payload"].get("ok"))


def reconcile(audit, invoice_id, expected_paid):
    """A paid invoice must show exactly one successful attempt.

    A failed invoice must show at least one attempt and no successes. More than
    one success means the customer was charged twice.
    """
    out = []
    ok_n = successful_attempts(audit, invoice_id)
    fail_n = failed_attempts(audit, invoice_id)
    if expected_paid and ok_n != 1:
        out.append(Discrepancy(invoice_id, "payment_count",
                               f"expected 1 successful charge, found {ok_n}"))
    if not expected_paid and ok_n:
        out.append(Discrepancy(invoice_id, "unexpected_payment",
                               f"invoice not paid but {ok_n} success rows"))
    if not expected_paid and fail_n > 1:
        out.append(Discrepancy(invoice_id, "retry_storm",
                               f"{fail_n} failed attempts on an unpayable invoice"))
    return out
'''

FILES["billing/invoice.py"] = '''"""Invoice assembly."""
from dataclasses import dataclass, field
from datetime import date

from billing import money, plans, tax


@dataclass
class Line:
    description: str
    amount_minor: int
    quantity: int = 1

    @property
    def total_minor(self):
        return self.amount_minor * self.quantity


@dataclass
class Invoice:
    invoice_id: str
    customer: str
    currency: str = "EUR"
    seller_country: str = "GR"
    buyer_country: str = "GR"
    buyer_vat_number: str = ""
    issued: date = field(default_factory=date.today)
    lines: list = field(default_factory=list)

    def add(self, description, amount_minor, quantity=1):
        self.lines.append(Line(description, amount_minor, quantity))
        return self

    def net_minor(self):
        return sum(line.total_minor for line in self.lines)

    def tax_line(self):
        return tax.compute_tax(self.net_minor(), self.seller_country,
                               self.buyer_country, self.buyer_vat_number)

    def gross_minor(self):
        return self.net_minor() + self.tax_line().amount_minor

    def render(self):
        out = [f"Invoice {self.invoice_id} for {self.customer}"]
        for line in self.lines:
            out.append(f"  {line.description:<34} {line.quantity:>3} x "
                       f"{money.format_money(line.amount_minor, self.currency):>12}")
        t = self.tax_line()
        out.append(f"  {'net':<34} {money.format_money(self.net_minor(), self.currency):>18}")
        out.append(f"  {t.note or 'tax':<34} {money.format_money(t.amount_minor, self.currency):>18}")
        out.append(f"  {'gross':<34} {money.format_money(self.gross_minor(), self.currency):>18}")
        return "\\n".join(out)


def for_subscription(invoice_id, customer, plan_code, seats=1, **kw):
    plan = plans.get_plan(plan_code)
    inv = Invoice(invoice_id, customer, currency=plan.currency, **kw)
    inv.add(f"{plan.name} plan", plan.monthly_minor)
    extra = plans.seat_charge(plan, seats)
    if extra:
        inv.add("Additional seats", plan.extra_seat_minor,
                max(0, seats - plan.included_seats))
    return inv
'''

FILES["billing/engine.py"] = '''"""The billing run: charge every due invoice and reconcile."""
from billing import gateway, ledger


class BillingRun:
    def __init__(self, client, audit):
        self.client = client
        self.audit = audit
        self.paid = []
        self.failed = []

    def process(self, invoice):
        amount = invoice.gross_minor()
        result = gateway.charge(self.client, invoice.invoice_id, amount,
                               invoice.currency, self.audit)
        if result.ok:
            self.paid.append(invoice.invoice_id)
            self.audit.record(invoice.invoice_id, "invoice_paid",
                              {"reference": result.reference, "amount": amount})
        else:
            self.failed.append(invoice.invoice_id)
            self.audit.record(invoice.invoice_id, "invoice_failed",
                              {"code": result.code, "attempts": result.attempts})
        return result

    def run(self, invoices):
        for inv in invoices:
            self.process(inv)
        return {"paid": list(self.paid), "failed": list(self.failed)}

    def discrepancies(self):
        out = []
        for invoice_id in self.paid:
            out.extend(ledger.reconcile(self.audit, invoice_id, True))
        for invoice_id in self.failed:
            out.extend(ledger.reconcile(self.audit, invoice_id, False))
        return out
'''

# The smoke suite a developer would plausibly have written: happy paths only.
# It passes on the pristine seed AND with every planted defect present, which is
# exactly why the defects survived to be found.
FILES["tests/smoke.py"] = '''"""Happy-path smoke tests. Green on a correct build."""
import sys
from datetime import date

sys.path.insert(0, ".")

from billing import money, plans, proration, tax, invoice, gateway, audit, engine, ledger

FAILURES = []


def check(name, cond):
    if not cond:
        FAILURES.append(name)


class FakeClient:
    def __init__(self, script):
        self.script = list(script)
        self.calls = 0

    def charge(self, invoice_id, amount, currency):
        self.calls += 1
        return self.script.pop(0) if self.script else {"ok": True, "reference": "R"}


check("minor units", money.to_minor("9.99") == 999)
check("zero-decimal currency", money.to_minor("1200", "JPY") == 1200)
check("format", money.format_money(4900) == "\\u20ac49.00")
check("split is lossless", sum(money.split_evenly(100, 3)) == 100)

check("plan lookup", plans.get_plan("team").monthly_minor == 4900)
check("seat charge", plans.seat_charge(plans.get_plan("team"), 7) == 1600)
check("annual discount", plans.annual_price(plans.get_plan("team")) == 52920)

# Proration inside a single 31-day month: correct even with the defect.
start = date(2026, 1, 1)
check("same-month proration",
      proration.change_plan(4900, 14900, date(2026, 1, 16), start) > 0)

# Cross-border B2B: correct even with the defect.
t = tax.compute_tax(10000, "GR", "DE", "DE123456789")
check("reverse charge cross-border", t.amount_minor == 0)
check("domestic consumer VAT",
      tax.compute_tax(10000, "GR", "GR").amount_minor == 2400)

inv = invoice.for_subscription("INV-1", "acme", "team", seats=7)
check("invoice net", inv.net_minor() == 4900 + 1600)
check("invoice renders", "Invoice INV-1" in inv.render())

# Soft decline retries then succeeds.
log = audit.AuditLog()
client = FakeClient([{"ok": False, "code": "issuer_unavailable"}, {"ok": True, "reference": "R1"}])
run = engine.BillingRun(client, log)
res = run.process(invoice.for_subscription("INV-2", "acme", "starter"))
check("soft decline retried", res.ok and res.attempts == 2)
check("soft decline reconciles", not run.discrepancies())

if FAILURES:
    print("SMOKE FAILURES:")
    for f in FAILURES:
        print("  -", f)
    raise SystemExit(1)
print(f"smoke suite clean")
'''

FILES["README.md"] = '''# billing

Subscription billing: plans, proration, VAT, gateway retries, reconciliation.

Money is integer minor units everywhere. `tests/smoke.py` covers the happy paths.

    python tests/smoke.py
'''


def generate(dest):
    """Write a pristine copy of the hard seed to `dest`."""
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
    return sum(body.count("\n") + 1 for body in FILES.values())
