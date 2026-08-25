"""Deterministic seed project for the benchmark.

Generates a small but realistic Python order-processing app with SIX planted
defects, one per fix scenario. Every scenario runs against a fresh copy, so runs
never contaminate each other.

Two invariants make the results trustworthy, both enforced by `runner --sanity`:

  1. The regression suite (`tests/acceptance.py`) PASSES on the pristine seed.
     It only asserts behaviour that is already correct, so any later failure is
     damage the agent did, not a pre-existing bug.
  2. Every fix grader FAILS on the pristine seed. A grader that passes before the
     agent touches anything measures nothing.
"""
import os
import shutil

FILES = {}

FILES["shop/__init__.py"] = '''"""Tiny order-processing app used to exercise the agent."""
__version__ = "0.3.0"
'''

FILES["shop/models.py"] = '''"""Core data types."""
from dataclasses import dataclass, field


@dataclass
class Product:
    sku: str
    name: str
    unit_price: float
    stock: int
    category: str = "general"


@dataclass
class OrderLine:
    sku: str
    quantity: int
    unit_price: float

    @property
    def subtotal(self):
        return self.unit_price * self.quantity


@dataclass
class Order:
    order_id: str
    customer: str
    lines: list = field(default_factory=list)
    country: str = "GR"

    def item_count(self):
        return sum(line.quantity for line in self.lines)
'''

# BUG 1 (B1): bulk discount uses > instead of >=, so an order of exactly
#             BULK_THRESHOLD units gets no discount.
# BUG 3 (B3): compute_tax truncates with int() instead of rounding.
FILES["shop/pricing.py"] = '''"""Discounts and tax."""

BULK_THRESHOLD = 10
BULK_DISCOUNT = 0.10
VAT_RATES = {"GR": 0.24, "DE": 0.19, "US": 0.0}


def apply_discount(subtotal, quantity):
    """Bulk discount for orders of BULK_THRESHOLD units or more."""
    if quantity > BULK_THRESHOLD:
        return subtotal * (1 - BULK_DISCOUNT)
    return subtotal


def compute_tax(amount, country="GR"):
    """VAT for the destination country, in cents-accurate currency units."""
    rate = VAT_RATES.get(country, 0.24)
    return int(amount * rate * 100) / 100


def line_total_for_report(unit_price, quantity):
    """Line total used by the reporting module."""
    return round(unit_price * quantity, 2)


def line_total(unit_price, quantity):
    return unit_price * quantity
'''

# BUG 2 (B2): reserve() does not check available stock and happily goes negative.
FILES["shop/inventory.py"] = '''"""Stock tracking."""


class OutOfStock(Exception):
    pass


class Inventory:
    def __init__(self, products=None):
        self.products = {p.sku: p for p in (products or [])}

    def get(self, sku):
        return self.products.get(sku)

    def available(self, sku):
        p = self.products.get(sku)
        return p.stock if p else 0

    def reserve(self, sku, quantity):
        """Take `quantity` units out of stock for an order."""
        p = self.products.get(sku)
        if p is None:
            raise OutOfStock(f"unknown sku {sku}")
        p.stock -= quantity
        return p.stock

    def restock(self, sku, quantity):
        p = self.products.get(sku)
        if p is None:
            raise OutOfStock(f"unknown sku {sku}")
        p.stock += quantity
        return p.stock
'''

# BUG 4 (B4): free shipping condition is inverted — shipping is charged on large
#             orders and waived on small ones.
FILES["shop/orders.py"] = '''"""Order totals."""
from shop import pricing

def subtotal(order):
    return sum(line.subtotal for line in order.lines)


def shipping_cost(order):
    """Orders at or above the free-shipping threshold ship free."""
    if subtotal(order) < 50.0:
        return 0.0
    return 4.90


def order_total(order):
    """Final amount the customer pays: goods, discount, tax, shipping."""
    goods = subtotal(order)
    discounted = pricing.apply_discount(goods, order.item_count())
    tax = pricing.compute_tax(discounted, order.country)
    return round(discounted + tax + shipping_cost(order), 2)
'''

# BUG 5 (B5): save() writes without an encoding (mangles non-ASCII on Windows)
#             and is not atomic, so a crash mid-write truncates the file.
FILES["shop/storage.py"] = '''"""JSON persistence."""
import json

from shop.models import Product


def save(path, products):
    """Write the product catalogue to disk."""
    data = [{"sku": p.sku, "name": p.name, "unit_price": p.unit_price,
             "stock": p.stock, "category": p.category} for p in products]
    with open(path, "w") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
    return len(data)


def load(path):
    with open(path, encoding="utf-8") as f:
        raw = json.load(f)
    return [Product(**row) for row in raw]
'''

# BUG 6 (B6): top_products sorts ascending, so "top" returns the WORST sellers.
# Also carries the duplicated formatting block that R2 must collapse.
FILES["shop/report.py"] = '''"""Text reports."""
from shop.pricing import line_total_for_report


def top_products(sales, limit=3):
    """Best-selling products first."""
    ranked = sorted(sales.items(), key=lambda kv: kv[1])
    return ranked[:limit]


def revenue_report(inventory, sales):
    """Revenue per product, using pricing's reporting helper."""
    out = []
    for sku, qty in sorted(sales.items()):
        p = inventory.products.get(sku)
        if p:
            out.append((p.name, line_total_for_report(p.unit_price, qty)))
    return out


def stock_report(inventory):
    lines = []
    for sku, p in sorted(inventory.products.items()):
        name = p.name[:24].ljust(24)
        qty = str(p.stock).rjust(6)
        lines.append(f"| {name} | {qty} |")
    return "\\n".join(lines)


def category_report(inventory, category):
    lines = []
    for sku, p in sorted(inventory.products.items()):
        if p.category != category:
            continue
        name = p.name[:24].ljust(24)
        qty = str(p.stock).rjust(6)
        lines.append(f"| {name} | {qty} |")
    return "\\n".join(lines)


def low_stock_report(inventory, threshold=5):
    lines = []
    for sku, p in sorted(inventory.products.items()):
        if p.stock > threshold:
            continue
        name = p.name[:24].ljust(24)
        qty = str(p.stock).rjust(6)
        lines.append(f"| {name} | {qty} |")
    return "\\n".join(lines)
'''

FILES["shop/cli.py"] = '''"""Command-line entry point."""
import sys

from shop import orders, report, storage
from shop.inventory import Inventory
from shop.models import Order, OrderLine


def build_demo_order(inv):
    return Order(order_id="D-1", customer="demo", country="GR", lines=[
        OrderLine("SKU-1", 2, inv.get("SKU-1").unit_price),
        OrderLine("SKU-2", 1, inv.get("SKU-2").unit_price),
    ])


def main(argv=None):
    argv = argv or sys.argv[1:]
    inv = Inventory(storage.load("data/catalogue.json"))
    if argv and argv[0] == "stock":
        print(report.stock_report(inv))
        return 0
    order = build_demo_order(inv)
    print(f"total: {orders.order_total(order):.2f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
'''

FILES["data/catalogue.json"] = '''[
  {"sku": "SKU-1", "name": "Widget", "unit_price": 9.99, "stock": 40, "category": "tools"},
  {"sku": "SKU-2", "name": "Gadget", "unit_price": 24.5, "stock": 12, "category": "tools"},
  {"sku": "SKU-3", "name": "Doohickey", "unit_price": 3.25, "stock": 3, "category": "parts"},
  {"sku": "SKU-4", "name": "Thingamajig", "unit_price": 15.0, "stock": 0, "category": "parts"}
]
'''

# The regression gate. Every assertion here holds on the PRISTINE seed, bugs and
# all — so a failure after an agent run means the agent broke something.
FILES["tests/acceptance.py"] = '''"""Baseline behaviours that must survive every change.

Run: python tests/acceptance.py   (exit 0 = all green)
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from shop import orders, pricing, report, storage          # noqa: E402
from shop.inventory import Inventory, OutOfStock           # noqa: E402
from shop.models import Order, OrderLine, Product          # noqa: E402

CHECKS = []


def check(name):
    def deco(fn):
        CHECKS.append((name, fn))
        return fn
    return deco


def catalogue():
    return [Product("SKU-1", "Widget", 9.99, 40, "tools"),
            Product("SKU-2", "Gadget", 24.5, 12, "tools"),
            Product("SKU-3", "Doohickey", 3.25, 3, "parts")]


@check("models: order counts items")
def _():
    o = Order("O-1", "ann", [OrderLine("SKU-1", 2, 9.99), OrderLine("SKU-2", 3, 24.5)])
    assert o.item_count() == 5
    assert abs(o.lines[0].subtotal - 19.98) < 1e-6


@check("pricing: no discount below the threshold")
def _():
    assert pricing.apply_discount(100.0, 2) == 100.0


@check("pricing: discount applies well above the threshold")
def _():
    assert abs(pricing.apply_discount(100.0, 50) - 90.0) < 1e-6


@check("pricing: zero-rated country pays no tax")
def _():
    assert pricing.compute_tax(100.0, "US") == 0.0


@check("pricing: line_total multiplies")
def _():
    assert abs(pricing.line_total(2.5, 4) - 10.0) < 1e-9


@check("inventory: available reflects stock")
def _():
    inv = Inventory(catalogue())
    assert inv.available("SKU-1") == 40
    assert inv.available("NOPE") == 0


@check("inventory: reserve decrements, restock increments")
def _():
    inv = Inventory(catalogue())
    inv.reserve("SKU-1", 5)
    assert inv.available("SKU-1") == 35
    inv.restock("SKU-1", 5)
    assert inv.available("SKU-1") == 40


@check("inventory: unknown sku raises OutOfStock")
def _():
    inv = Inventory(catalogue())
    try:
        inv.reserve("NOPE", 1)
    except OutOfStock:
        return
    raise AssertionError("expected OutOfStock")


@check("orders: subtotal sums the lines")
def _():
    o = Order("O-2", "bob", [OrderLine("SKU-1", 2, 10.0), OrderLine("SKU-2", 1, 5.0)])
    assert abs(orders.subtotal(o) - 25.0) < 1e-9


@check("orders: total is a positive rounded number")
def _():
    o = Order("O-3", "cy", [OrderLine("SKU-1", 2, 9.99)])
    t = orders.order_total(o)
    assert t > 0 and abs(t - round(t, 2)) < 1e-9


@check("report: stock_report renders one row per product")
def _():
    inv = Inventory(catalogue())
    rows = report.stock_report(inv).splitlines()
    assert len(rows) == 3 and rows[0].startswith("|")


@check("report: category_report filters by category")
def _():
    inv = Inventory(catalogue())
    rows = report.category_report(inv, "parts").splitlines()
    assert len(rows) == 1 and "Doohickey" in rows[0]


@check("report: low_stock_report respects the threshold")
def _():
    inv = Inventory(catalogue())
    rows = report.low_stock_report(inv, 5).splitlines()
    assert len(rows) == 1 and "Doohickey" in rows[0]


@check("report: top_products returns at most `limit` rows")
def _():
    out = report.top_products({"a": 5, "b": 9, "c": 1, "d": 7}, limit=2)
    assert len(out) == 2


@check("storage: save then load round-trips")
def _():
    import tempfile
    path = os.path.join(tempfile.mkdtemp(), "cat.json")
    storage.save(path, catalogue())
    back = storage.load(path)
    assert len(back) == 3 and back[0].sku == "SKU-1"


@check("cli: stock command runs")
def _():
    from shop import cli
    inv = Inventory(catalogue())
    assert cli.build_demo_order(inv).item_count() == 3


def main():
    failed = []
    for name, fn in CHECKS:
        try:
            fn()
        except Exception as e:                              # noqa: BLE001
            failed.append(f"{name}: {type(e).__name__}: {e}")
    for f in failed:
        print("FAIL", f)
    print(f"{len(CHECKS) - len(failed)}/{len(CHECKS)} acceptance checks passed")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
'''

FILES["README.md"] = '''# shop

Tiny order-processing app.

    python -m shop.cli            # print the demo order total
    python -m shop.cli stock      # stock report
    python tests/acceptance.py    # regression suite

Layout: `shop/models.py` types, `shop/pricing.py` discount and VAT,
`shop/inventory.py` stock, `shop/orders.py` totals, `shop/storage.py` JSON
persistence, `shop/report.py` text reports, `shop/cli.py` entry point.
'''


def generate(dest):
    """Write a pristine copy of the seed project to `dest`."""
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
