"""Locate the `axium` Python package this harness measures.

`bench-python/` is a standalone project so its results stand on their own, but it
has to import the agent it is benchmarking. Rather than vendoring a copy (which
would drift from the real thing and quietly benchmark a stale agent), it locates
the package at import time.

Resolution order, first hit wins:

1. `AXIUM_PYTHON` in the environment — an explicit path to the directory that
   *contains* the `axium` package. Set this when the agent lives somewhere else.
2. `../python` relative to this file — the layout in the axium repo.
3. Whatever is already importable (a `pip install`ed axium, say).

A wrong or missing path fails here with a message naming the env var, rather than
thirty frames deep in a runner with a bare ImportError.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
DEFAULT = os.path.join(REPO, "python")


def axium_root():
    """The directory containing the `axium` package."""
    env = os.environ.get("AXIUM_PYTHON", "").strip()
    if env:
        return os.path.abspath(env)
    return DEFAULT


def ensure_on_path():
    """Put the agent package on `sys.path`. Idempotent; returns the root used."""
    root = axium_root()
    if os.path.isdir(os.path.join(root, "axium")):
        if root not in sys.path:
            sys.path.insert(0, root)
        return root
    # Already importable (installed, or a path someone else set up).
    try:
        import axium  # noqa: F401
        return os.path.dirname(os.path.dirname(os.path.abspath(axium.__file__)))
    except ImportError:
        pass
    raise SystemExit(
        f"bench-python cannot find the axium package.\n"
        f"  looked in: {root}\n"
        f"  fix: set AXIUM_PYTHON to the directory that CONTAINS the 'axium' "
        f"package, e.g.\n"
        f"       set AXIUM_PYTHON=C:\\path\\to\\axium\\python"
    )


AXIUM_ROOT = ensure_on_path()

# This project's own packages (`bench`, `versus`) must be importable too, so the
# runners work from any working directory rather than only from here.
if HERE not in sys.path:
    sys.path.insert(0, HERE)
