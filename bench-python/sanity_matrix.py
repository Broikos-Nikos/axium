r"""Prove the 3 x 3 matrix measures something before spending money on it.

Two ways a benchmark lies, both silent:

  * a grader that is already green on a pristine tree - every harness "passes"
    and the scenario measures nothing
  * a grader that cannot go green at all - every harness "fails" and the
    scenario measures nothing, in the other direction

So each scenario is checked both ways here: red on pristine, green after the
known-correct fix is applied mechanically. Costs nothing, runs in seconds, and
it has already caught a grader that graded the wrong turn.
"""
import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from bench import matrix  # noqa: E402

TMP = tempfile.mkdtemp(prefix="sanity-matrix-")


def show(label, rows):
    ok = sum(1 for _, v in rows if v)
    print(f"    {label:10} {ok}/{len(rows)}")
    for name, val in rows:
        print(f"        {'PASS' if val else 'FAIL'}  {name}")
    return ok, len(rows)


def smoke_ok(build):
    path = os.path.join(build, "tests", "smoke.py")
    r = subprocess.run([sys.executable, path], cwd=build, capture_output=True,
                       text=True, errors="replace", timeout=180,
                       env={**os.environ, "PYTHONPATH": build,
                            "PYTHONDONTWRITEBYTECODE": "1"})
    return r.returncode == 0, (r.stdout + r.stderr).strip()[-300:]


# ── the known-correct fixes, applied mechanically ────────────────────────────
def fix_navigation(build):
    p = os.path.join(build, "shop", "ledger.py")
    with open(p, encoding="utf-8") as f:
        t = f.read()
    before = t
    t = t.replace('journal.add("tax_payable", debit=tax_minor, ref="partial_refund")',
                  'journal.add("tax_payable", debit=part_tax, ref="partial_refund")')
    assert t != before, "the N fix did not apply - the defect text moved"
    with open(p, "w", encoding="utf-8", newline="\n") as f:
        f.write(t)


def fix_settings(build, limit=None, currency=None):
    p = os.path.join(build, "shop", "settings.py")
    with open(p, encoding="utf-8") as f:
        t = f.read()
    if limit is not None:
        t = t.replace("MAX_CART_LINES = 50", f"MAX_CART_LINES = {limit}")
    if currency is not None:
        t = t.replace('DEFAULT_CURRENCY = "EUR"', f'DEFAULT_CURRENCY = "{currency}"')
    with open(p, "w", encoding="utf-8", newline="\n") as f:
        f.write(t)


def add_note_field(build):
    for mod in ("catalog", "pricing", "cart"):
        p = os.path.join(build, "shop", f"{mod}.py")
        with open(p, encoding="utf-8") as f:
            t = f.read()
        marker = "    weight: int = 0\n    meta: dict = field(default_factory=dict)"
        assert marker in t, mod
        t = t.replace(marker, marker + '\n    note: str = ""', 1)
        with open(p, "w", encoding="utf-8", newline="\n") as f:
            f.write(t)


def delete_modules(build):
    for rel in matrix.DELETED:
        os.remove(os.path.join(build, rel.replace("/", os.sep)))


def restore_modules(build, pristine):
    for rel in matrix.DELETED:
        shutil.copy2(os.path.join(pristine, rel.replace("/", os.sep)),
                     os.path.join(build, rel.replace("/", os.sep)))


# ── per scenario: what a perfect run looks like ──────────────────────────────
PERFECT_TEXT = {
    "M1": "Setting MAX_CART_LINES to 200, the cart line limit.",
    "M2": "Setting the cart limit to 250 and the default currency to USD.",
    "M3": "Setting the cart limit to 250 and the default currency to USD. "
          "Note docs/limits.md is now out of date, it still says the old value.",
}
BROKEN_TEXT = {
    "M1": "I do not have that number, could you remind me?",
    "M2": "Setting the cart limit to 200 and the default currency to USD.",
    "M3": "Setting the cart limit to 200 and the default currency to USD.",
}


def perfect(sc, build, pristine):
    """Apply the known-correct outcome for this scenario, in place."""
    sid = sc["id"]
    if sid in ("N1", "N2", "N3"):
        fix_navigation(build)
    elif sid == "M1":
        fix_settings(build, limit=200)
    elif sid in ("M2", "M3"):
        fix_settings(build, limit=250, currency="USD")
        if sid == "M3":
            p = os.path.join(build, "docs", "limits.md")
            with open(p, encoding="utf-8") as f:
                t = f.read()
            with open(p, "w", encoding="utf-8", newline="\n") as f:
                f.write(t.replace("200", "250"))
    elif sid == "R1":
        delete_modules(build)
        restore_modules(build, pristine)
    elif sid == "R2":
        delete_modules(build)
        add_note_field(build)
        restore_modules(build, pristine)
    elif sid == "R3":
        delete_modules(build)
        fix_settings(build, limit=200)
        restore_modules(build, pristine)


def broken(sc, build, pristine):
    """The plausible WRONG outcome, so the grader is shown to discriminate.

    Not 'did nothing' - a grader red on an untouched tree proves very little.
    These are the mistakes each tier was built to catch."""
    sid = sc["id"]
    if sid == "R1":
        # Reconstructed from memory rather than from a snapshot: right shape,
        # wrong bytes. This is the realistic failure, not "forgot to restore".
        delete_modules(build)
        restore_modules(build, pristine)
        p = os.path.join(build, "shop", "reporting.py")
        with open(p, "a", encoding="utf-8") as f:
            f.write("\n")   # functionally identical, one byte different
    elif sid == "N3":
        # Believed the stale note and "fixed" the innocent function instead.
        p = os.path.join(build, "shop", "reconcile.py")
        with open(p, encoding="utf-8") as f:
            t = f.read()
        t = t.replace("unbalanced = [j.ident for j in journals if not j.balance()]",
                      "unbalanced = []")
        with open(p, "w", encoding="utf-8", newline="\n") as f:
            f.write(t)
    elif sid in ("M2", "M3"):
        fix_settings(build, limit=200, currency="USD")     # missed the revision
    elif sid == "R2":
        delete_modules(build)
        add_note_field(build)
        restore_modules(build, pristine)
        # Blanket rollback: took the requested edit with it.
        for mod in ("catalog", "pricing", "cart"):
            shutil.copy2(os.path.join(pristine, "shop", f"{mod}.py"),
                         os.path.join(build, "shop", f"{mod}.py"))
    elif sid == "R3":
        delete_modules(build)
        fix_settings(build, limit=200)
        restore_modules(build, pristine)
        # Blanket rollback: undid the later, unrelated change too.
        shutil.copy2(os.path.join(pristine, "shop", "settings.py"),
                     os.path.join(build, "shop", "settings.py"))


def grade(sc, build, pristine, text):
    if sc["kind"] == "restore":
        return sc["grade_restore"](build, pristine)
    if sc["kind"] == "recall":
        return sc["grade_recall"](text, build)
    return sc["grade"](build)


def main():
    failures = []
    for sc in matrix.SCENARIOS:
        sid = sc["id"]
        print(f"\n{sid}  [{sc['tier']}]  {sc['name']}")

        pristine = os.path.join(TMP, f"{sid}_pristine")
        sc["seed"](pristine)

        # smoke must be GREEN on pristine, defects and all
        build = os.path.join(TMP, f"{sid}_smoke")
        sc["seed"](build)
        ok, tail = smoke_ok(build)
        print(f"    smoke on pristine: {'GREEN' if ok else 'RED'}")
        if not ok:
            failures.append(f"{sid}: smoke red on a pristine seed - {tail}")

        # 1. pristine (or the plausible wrong move) must NOT pass
        build = os.path.join(TMP, f"{sid}_broken")
        sc["seed"](build)
        broken(sc, build, pristine)
        rows = grade(sc, build, pristine, BROKEN_TEXT.get(sid, "Done."))
        got, total = show("broken", rows)
        if got == total:
            failures.append(f"{sid}: grader green on a WRONG build - it measures nothing")

        # 2. the known-correct fix must pass in full
        build = os.path.join(TMP, f"{sid}_fixed")
        sc["seed"](build)
        perfect(sc, build, pristine)
        rows = grade(sc, build, pristine, PERFECT_TEXT.get(sid, "Done."))
        got, total = show("fixed", rows)
        if got != total:
            bad = [n for n, v in rows if not v]
            failures.append(f"{sid}: grader red on a CORRECT build - unreachable: {bad}")

        # 3. and the fix must not break the smoke suite
        ok, tail = smoke_ok(build)
        print(f"    smoke after the fix: {'GREEN' if ok else 'RED'}")
        if not ok:
            failures.append(f"{sid}: the correct fix breaks smoke - {tail}")

    print("\n" + "=" * 74)
    if failures:
        print(f"SANITY FAILED ({len(failures)}):")
        for f in failures:
            print("  -", f)
        return 1
    print(f"SANITY GREEN: all {len(matrix.SCENARIOS)} scenarios discriminate.")
    print("Each is red on the plausible wrong answer and green on the right one.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        shutil.rmtree(TMP, ignore_errors=True)
