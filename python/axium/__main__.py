"""Entry point: python -m axium [options]"""
import argparse
import logging
import sys

from . import config as config_mod, providers
from .cli import run
from .router import describe_routing


def main(argv=None):
    ap = argparse.ArgumentParser(prog="axium", description="Axium autonomous coding agent")
    ap.add_argument("--config", help="path to config.json")
    ap.add_argument("--dir", help="working directory for tools")
    ap.add_argument("--model", help="override the primary model")
    ap.add_argument("--mode", choices=["simple", "supercharge", "skills"],
                    help="processing mode for this session")
    ap.add_argument("--message", "-m", help="run one turn non-interactively and exit")
    ap.add_argument("--check", action="store_true",
                    help="print configured providers and model routing, then exit")
    ap.add_argument("-v", "--verbose", action="store_true")
    a = ap.parse_args(argv)

    logging.basicConfig(level=logging.INFO if a.verbose else logging.WARNING,
                        format="%(levelname)s %(name)s: %(message)s")

    cfg = config_mod.load(a.config)
    if a.model:
        cfg.models.primary = a.model
        cfg.models.primary_provider = ""
    if a.mode:
        cfg.settings.mode = a.mode
    if a.dir:
        cfg.settings.working_directory = a.dir

    if a.check:
        found = providers.probe(cfg)
        print(f"config:    {cfg.path}")
        print(f"providers: {', '.join(found) if found else '(none configured)'}")
        print(f"workdir:   {cfg.settings.working_directory}")
        print("routing:")
        print(describe_routing(cfg))
        return 0 if found else 1

    if a.message:
        from .memory import Memory
        from .router import Agent
        mem = Memory(cfg.resolve_data_path(cfg.settings.memory_file))
        agent = Agent(cfg, workdir=a.dir or cfg.settings.working_directory, memory=mem)
        turn = agent.run(a.message)
        print(turn.text)
        print(turn.meter.summary_line(), file=sys.stderr)
        return 1 if turn.error else 0

    return run(cfg, workdir=a.dir)


if __name__ == "__main__":
    sys.exit(main())
