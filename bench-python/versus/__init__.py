"""Axium vs Orange, one benchmark, two agents, identical work.

`bench/` measures Axium against itself (model A vs model B). This package measures
Axium against Orange: two differently-designed agents driven through the SAME five
multi-turn sessions, on byte-identical copies of the same seed project, graded by
graders neither agent can see.

The comparison is only meaningful because nothing here trusts either agent's own
bookkeeping. File changes come from hashing the tree before and after; tool calls
come from wrapping each agent's dispatcher; cost comes from each agent's own meter
but is reconciled against a shared price table.
"""
__all__ = ["scenarios", "graders", "adapters", "runner", "report"]
