"""Axium, self-hosted autonomous coding agent (Python implementation).

A faithful port of the Rust agent in ../src: same tool names, same classifier ->
tool-loop -> review pipeline, same cost-routing between a primary and a cheap
continuation model. The port exists so the whole stack can be run and benchmarked
without a C toolchain.
"""
__version__ = "1.0.0"

from .config import load as load_config          # noqa: F401
from .router import Agent, run_once, Turn        # noqa: F401
from .metrics import Meter                       # noqa: F401
