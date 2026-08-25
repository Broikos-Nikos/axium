"""Objective graders.

Every grader returns a list of (check_name, passed) pairs. Two axes are scored
separately:

    change, did the agent do what was asked?
    regress, is everything that already worked still working?

Behaviour is graded by IMPORTING the agent's code in a fresh subprocess and
asserting on real outputs, never by pattern-matching the diff. An agent that
writes convincing-looking code that does not run scores zero, which is the point.

Awareness scenarios are read-only: they grade the agent's ANSWER against ground
truth (required substrings, forbidden substrings) plus "it changed no files".
"""
import os
import re
import subprocess
import sys

TIMEOUT = 90


def _run_python(build, code):
    """Run `code` with `build` on sys.path. Returns (exit_code, stdout, stderr)."""
    try:
        r = subprocess.run([sys.executable, "-c", code], cwd=build, capture_output=True,
                           text=True, errors="replace", timeout=TIMEOUT,
                           env={**os.environ, "PYTHONPATH": build, "PYTHONDONTWRITEBYTECODE": "1"})
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return -1, "", f"timeout after {TIMEOUT}s"
    except Exception as e:                                  # noqa: BLE001
        return -1, "", f"{type(e).__name__}: {e}"


def _probe(build, name, body):
    """One assertion-style check. `body` must raise to fail."""
    code = "import sys\nsys.path.insert(0, %r)\n%s\nprint('OK')" % (build, body)
    rc, out, err = _run_python(build, code)
    return (name, rc == 0 and "OK" in out)


def regression(build):
    """The acceptance suite. Green on the pristine seed, so any red is agent damage.

    Run as a real script, not via -c: the suite resolves its own package root from
    __file__, which does not exist under -c.
    """
    path = os.path.join(build, "tests", "acceptance.py")
    if not os.path.exists(path):
        return [("acceptance suite present", False)]
    try:
        r = subprocess.run([sys.executable, os.path.join("tests", "acceptance.py")],
                           cwd=build, capture_output=True, text=True, errors="replace",
                           timeout=TIMEOUT,
                           env={**os.environ, "PYTHONPATH": build,
                                "PYTHONDONTWRITEBYTECODE": "1"})
        rc, out, err = r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return [("acceptance suite completes", False)]
    rows = [(f"acceptance: {line[5:]}", False)
            for line in (out + err).splitlines() if line.startswith("FAIL ")]
    rows.append(("acceptance suite exits clean", rc == 0))
    return rows


# ── fix scenarios ────────────────────────────────────────────────────────────
def g_b1(build):
    """Bulk discount must apply AT the threshold, not only above it."""
    return [
        _probe(build, "discount applies at exactly 10 units",
               "from shop import pricing\n"
               "assert abs(pricing.apply_discount(100.0, 10) - 90.0) < 1e-6, "
               "pricing.apply_discount(100.0, 10)"),
        _probe(build, "discount still applies above the threshold",
               "from shop import pricing\n"
               "assert abs(pricing.apply_discount(100.0, 11) - 90.0) < 1e-6"),
        _probe(build, "no discount below the threshold",
               "from shop import pricing\n"
               "assert pricing.apply_discount(100.0, 9) == 100.0"),
    ]


def g_b2(build):
    """Reserving more than available must raise, and must not mutate stock."""
    setup = ("from shop.inventory import Inventory, OutOfStock\n"
             "from shop.models import Product\n"
             "inv = Inventory([Product('S', 'n', 1.0, 5)])\n")
    return [
        _probe(build, "over-reserve raises OutOfStock",
               setup + "raised = False\n"
               "try:\n    inv.reserve('S', 6)\nexcept OutOfStock:\n    raised = True\n"
               "assert raised, 'reserve(6) of 5 units did not raise'"),
        _probe(build, "stock never goes negative",
               setup + "try:\n    inv.reserve('S', 99)\nexcept Exception:\n    pass\n"
               "assert inv.available('S') >= 0, inv.available('S')"),
        _probe(build, "a valid reserve still works",
               setup + "inv.reserve('S', 5)\nassert inv.available('S') == 0"),
    ]


def g_b3(build):
    """VAT must round, not truncate: 0.24 of 10.10 is 2.42, not 2.42->2.42 by luck."""
    return [
        _probe(build, "tax rounds instead of truncating",
               "from shop import pricing\n"
               "v = pricing.compute_tax(19.99, 'GR')\n"
               "assert abs(v - 4.80) < 1e-9, v"),
        _probe(build, "second rounding case",
               "from shop import pricing\n"
               "v = pricing.compute_tax(10.10, 'DE')\n"
               "assert abs(v - 1.92) < 1e-9, v"),
        _probe(build, "zero-rate country unchanged",
               "from shop import pricing\nassert pricing.compute_tax(50.0, 'US') == 0.0"),
    ]


def g_b4(build):
    """Free shipping above the threshold, flat rate below: the seed has it backwards."""
    mk = ("from shop.models import Order, OrderLine\nfrom shop import orders\n"
          "def o(total):\n    return Order('X', 'c', [OrderLine('S', 1, total)])\n")
    return [
        _probe(build, "large order ships free",
               mk + "assert orders.shipping_cost(o(100.0)) == 0.0, orders.shipping_cost(o(100.0))"),
        _probe(build, "small order pays flat shipping",
               mk + "c = orders.shipping_cost(o(10.0))\n"
                    "assert abs(c - 4.90) < 1e-9, c"),
        _probe(build, "exactly at the threshold ships free",
               mk + "assert orders.shipping_cost(o(50.0)) == 0.0"),
    ]


def g_b5(build):
    """Saving must be UTF-8 safe and atomic.

    Atomicity is proven by fault injection, not by reading the source: json.dump is
    monkeypatched to blow up mid-write, and the pre-existing file must survive
    untouched. A non-atomic save truncates it.
    """
    body = ("import json, os, tempfile\n"
            "from shop import storage\nfrom shop.models import Product\n"
            "d = tempfile.mkdtemp(); p = os.path.join(d, 'c.json')\n"
            "storage.save(p, [Product('S1', 'Καφές ☕', 1.5, 2, 'x')])\n")
    return [
        _probe(build, "non-ASCII names round-trip",
               body + "back = storage.load(p)\n"
               "assert back[0].name == 'Καφές ☕', back[0].name"),
        _probe(build, "file is valid UTF-8 JSON on disk",
               body + "raw = open(p, encoding='utf-8').read()\n"
               "assert json.loads(raw)[0]['sku'] == 'S1'"),
        _probe(build, "a crash mid-save leaves the old file intact",
               body + "good = open(p, encoding='utf-8').read()\n"
               "real = json.dump\n"
               "def boom(*a, **k):\n"
               "    real(*a, **k)\n"
               "    raise IOError('disk full')\n"
               "storage.json.dump = boom\n"
               "try:\n    storage.save(p, [Product('S2', 'x', 1.0, 1, 'y')])\n"
               "except Exception:\n    pass\n"
               "finally:\n    storage.json.dump = real\n"
               "now = open(p, encoding='utf-8').read()\n"
               "assert now == good, 'file was clobbered by a failed save'\n"
               "assert not [f for f in os.listdir(d) if f.endswith('.tmp')], 'tmp left behind'"),
    ]


def g_b6(build):
    """top_products must return the BEST sellers, highest first."""
    return [
        _probe(build, "best seller comes first",
               "from shop import report\n"
               "out = report.top_products({'a': 5, 'b': 9, 'c': 1, 'd': 7}, limit=3)\n"
               "assert out[0][0] == 'b', out"),
        _probe(build, "ordering is descending",
               "from shop import report\n"
               "out = report.top_products({'a': 5, 'b': 9, 'c': 1, 'd': 7}, limit=3)\n"
               "vals = [v for _, v in out]\nassert vals == sorted(vals, reverse=True), out"),
        _probe(build, "limit is respected",
               "from shop import report\n"
               "assert len(report.top_products({'a': 1, 'b': 2, 'c': 3}, limit=2)) == 2"),
    ]


# ── refactor scenarios ───────────────────────────────────────────────────────
def _src(build, rel):
    path = os.path.join(build, rel)
    return open(path, encoding="utf-8").read() if os.path.exists(path) else ""


def g_r1(build):
    """The three report builders must share one row formatter."""
    src = _src(build, "shop/report.py")
    pattern = re.compile(r"\[:24\]\.ljust\(24\)")
    dupes = len(pattern.findall(src))
    return [
        ("row formatting written once, not three times", dupes <= 1),
        ("report module still imports", _probe(build, "x", "from shop import report")[1]),
        _probe(build, "stock_report output unchanged",
               "from shop import report\nfrom shop.inventory import Inventory\n"
               "from shop.models import Product\n"
               "inv = Inventory([Product('A', 'Widget', 1.0, 4, 'tools')])\n"
               "assert report.stock_report(inv) == '| ' + 'Widget'.ljust(24) + ' | ' + "
               "'4'.rjust(6) + ' |', repr(report.stock_report(inv))"),
    ]


def g_r2(build):
    """Magic numbers in orders.py must move to named module-level constants.

    Checked with the AST rather than by name, so the agent is free to pick its own
    constant names: no meaningful numeric literal may survive inside a function
    body, and the module must define UPPER_CASE numeric constants at top level.
    """
    import ast
    src = _src(build, "shop/orders.py")
    try:
        tree = ast.parse(src)
    except SyntaxError as e:
        return [("orders.py parses", False), (f"syntax error: {e.msg}", False)]

    consts = {t.id for node in tree.body if isinstance(node, ast.Assign)
              for t in node.targets
              if isinstance(t, ast.Name) and t.id.isupper()
              and isinstance(node.value, ast.Constant)
              and isinstance(node.value.value, (int, float))}
    inline = []
    for fn in [n for n in ast.walk(tree) if isinstance(n, ast.FunctionDef)]:
        for node in ast.walk(fn):
            if isinstance(node, ast.Constant) and isinstance(node.value, (int, float)) \
                    and not isinstance(node.value, bool) and node.value not in (0, 1, 0.0, 1.0, 2):
                inline.append(f"{fn.name}:{node.value}")
    return [
        (f"no magic numbers left in function bodies (found {inline or 'none'})", not inline),
        (f"module-level numeric constants defined (found {len(consts)})", len(consts) >= 2),
        # R2 is a PURE refactor on a seed that still carries B4's inverted shipping.
        # "Behaviour must not change" therefore means the seed's behaviour, bug and
        # all, an agent that quietly fixes the bug here has changed behaviour.
        _probe(build, "shipping behaviour is byte-identical to the seed",
               "from shop import orders\nfrom shop.models import Order, OrderLine\n"
               "def o(v):\n    return Order('X', 'c', [OrderLine('S', 1, v)])\n"
               "small, large = orders.shipping_cost(o(10.0)), orders.shipping_cost(o(100.0))\n"
               "assert small == 0.0, ('small changed', small)\n"
               "assert abs(large - 4.90) < 1e-9, ('large changed', large)"),
    ]


def g_r3(build):
    """Order total must be reachable from one public helper on Order."""
    return [
        _probe(build, "Order.total() exists and works",
               "from shop.models import Order, OrderLine\n"
               "o = Order('X', 'c', [OrderLine('S', 2, 10.0)])\n"
               "assert hasattr(o, 'total'), 'no Order.total'\n"
               "assert abs(o.total() - 0) > 0"),
        _probe(build, "Order.total matches orders.order_total",
               "from shop.models import Order, OrderLine\nfrom shop import orders\n"
               "o = Order('X', 'c', [OrderLine('S', 2, 10.0)])\n"
               "assert abs(o.total() - orders.order_total(o)) < 1e-9"),
    ]


# ── multi-file / feature scenarios ───────────────────────────────────────────
def g_f1(build):
    """New currency-formatting helper, used by the CLI."""
    return [
        _probe(build, "format_money exists and pads to 2 decimals",
               "from shop import report\n"
               "assert report.format_money(3.5) in ('3.50', '\\u20ac3.50', 'EUR 3.50'), "
               "report.format_money(3.5)"),
        _probe(build, "handles zero",
               "from shop import report\nassert report.format_money(0) is not None"),
    ]


def g_f2(build):
    """Order history filtering by country."""
    return [
        _probe(build, "orders.filter_by_country returns only matches",
               "from shop import orders\nfrom shop.models import Order, OrderLine\n"
               "a = Order('1', 'x', [OrderLine('S', 1, 1.0)], country='GR')\n"
               "b = Order('2', 'y', [OrderLine('S', 1, 1.0)], country='DE')\n"
               "out = orders.filter_by_country([a, b], 'GR')\n"
               "assert [o.order_id for o in out] == ['1'], out"),
        _probe(build, "empty match returns empty list",
               "from shop import orders\nassert orders.filter_by_country([], 'GR') == []"),
    ]


def g_f3(build):
    """A CLI subcommand that prints the low-stock report."""
    rc, out, err = _run_python(
        build, "import sys; sys.path.insert(0, %r); "
               "from shop import cli; raise SystemExit(cli.main(['low-stock']))" % build)
    return [
        ("cli low-stock exits 0", rc == 0),
        ("cli low-stock prints a row", "|" in out),
    ]


# ── awareness (read-only) ────────────────────────────────────────────────────
def answer_grader(required=(), forbidden=(), any_of=()):
    """Grade a free-text answer against ground truth, case-insensitively."""
    def grade(build, answer):
        a = (answer or "").lower()
        rows = [(f"mentions {r}", r.lower() in a) for r in required]
        rows += [(f"does not claim {f}", f.lower() not in a) for f in forbidden]
        if any_of:
            rows.append((f"mentions one of {'/'.join(any_of)}",
                         any(x.lower() in a for x in any_of)))
        _ = build
        return rows
    return grade


GRADERS = {
    "B1": g_b1, "B2": g_b2, "B3": g_b3, "B4": g_b4, "B5": g_b5, "B6": g_b6,
    "R1": g_r1, "R2": g_r2, "R3": g_r3,
    "F1": g_f1, "F2": g_f2, "F3": g_f3,
}


def pct(rows):
    return round(sum(1 for _, ok in rows if ok) / len(rows), 3) if rows else 0.0


# ── mechanism scenarios ──────────────────────────────────────────────────────
# These grade the durable-context layer itself rather than a coding task. They
# are graded over the Turn (and, for M1, over a SECOND turn), because what they
# measure, did a fact survive, was the undo exact, did the Brain save work,
# is not visible in the file tree alone.

def undo_was_exact(build, pristine_dir):
    """Every file identical to the pristine seed, byte for byte.

    The V4 scenario asks an agent to put something back "exactly". Scoring that
    by "the file exists again" passes a reconstruction that silently dropped a
    comment or changed a line ending. This compares bytes.
    """
    import filecmp
    import os

    diffs, missing, extra = [], [], []
    for root, dirs, files in os.walk(pristine_dir):
        dirs[:] = [d for d in dirs if d not in (".git", ".axium", "__pycache__")]
        for name in files:
            src = os.path.join(root, name)
            rel = os.path.relpath(src, pristine_dir)
            dst = os.path.join(build, rel)
            if not os.path.exists(dst):
                missing.append(rel)
            elif not filecmp.cmp(src, dst, shallow=False):
                diffs.append(rel)
    for root, dirs, files in os.walk(build):
        dirs[:] = [d for d in dirs if d not in (".git", ".axium", "__pycache__")]
        for name in files:
            rel = os.path.relpath(os.path.join(root, name), build)
            if not os.path.exists(os.path.join(pristine_dir, rel)):
                extra.append(rel)
    return diffs, missing, extra
