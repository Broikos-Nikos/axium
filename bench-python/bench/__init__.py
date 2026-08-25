"""Axium benchmark harness.

Runs the real agent loop against a freshly generated seed project across 20
scenarios, grading each on two axes (did it do the task / did it break anything)
and recording tokens, cost, latency and tool behaviour for every run.
"""
