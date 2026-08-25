"""Router wiring, the new durable-context layer, with no API calls.

`providers.call` is replaced by a scripted fake, so these tests assert what the
agent DOES with facts, brain, checkpoints, skills and the planner rather than
what a model happens to say. Every test isolates its own temp directory: the
fact store and the Brain both write to disk.
"""

import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from axium import brain, config, facts, planner, providers, router, skills  # noqa: E402
from axium import verify                                                    # noqa: E402
from axium import tools as tools_mod                                        # noqa: E402
from axium import trajectory                                                # noqa: E402
from axium.checkpoints import Checkpoints                                   # noqa: E402


def _read(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


def _res(text="", tool_calls=(), model="fake"):
    content = [{"type": "text", "text": text}] if text else []
    for i, (name, args) in enumerate(tool_calls):
        content.append({"type": "tool_use", "id": f"t{i}", "name": name, "input": args})
    return {"content": content, "model": model, "usage": {}, "stop_reason": "end_turn",
            "latency_s": 0.0}


class FakeProvider:
    """Scripted replies, in order, with the calls recorded for assertions."""

    def __init__(self, script):
        self.script = list(script)
        self.calls = []

    def __call__(self, cfg, model, system, messages, **kw):
        self.calls.append({"model": model, "system": system, "messages": messages,
                           "tools": kw.get("tools"), "kw": kw})
        if self.script:
            nxt = self.script.pop(0)
            return nxt(self) if callable(nxt) else nxt
        return _res("done")


class WiringCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="axium-wire-")
        self.work = os.path.join(self.tmp, "proj")
        os.makedirs(self.work)
        with open(os.path.join(self.work, "app.py"), "w", encoding="utf-8") as f:
            f.write("def total(x):\n    return x\n")
        self.cfg = config.Config(path=os.path.join(self.tmp, "config.json"))
        self.cfg.settings.working_directory = self.work
        self.cfg.settings.distill_skills = False
        self._real_call = providers.call

    def tearDown(self):
        providers.call = self._real_call

    def _install(self, script):
        fake = FakeProvider(script)
        providers.call = fake
        return fake


class TestFactsSurviveTheSystemPrompt(WiringCase):
    def test_fact_lands_in_system_prompt_below_the_cache_marker(self):
        store = facts.FactStore(os.path.join(self.tmp, "f.db"))
        store.remember("Shipping is free on orders over 50 euro.", type="rule",
                       key="shipping.free_threshold", importance=0.9,
                       scope=os.path.basename(self.work))
        agent = router.Agent(self.cfg, workdir=self.work, facts=store)
        prompt = agent.system_prompt()

        self.assertIn("free on orders over 50 euro", prompt)
        # Below the marker: facts change most turns, and everything above the
        # marker is sent with a cache breakpoint. The instructions legitimately
        # NAME the block above the marker, so match the block header itself.
        marker = "\n\n[MEMORY]\n"
        self.assertIn(marker, prompt)
        self.assertIn("\n[FACTS]\n", prompt)
        self.assertGreater(prompt.index("\n[FACTS]\n"), prompt.index(marker))

    def test_brain_sits_above_the_cache_marker(self):
        brain.write_profile(self.work, "Stack: Python. Entry point: app.py.")
        agent = router.Agent(self.cfg, workdir=self.work)
        prompt = agent.system_prompt()
        self.assertIn("[PROJECT BRAIN]", prompt)
        self.assertLess(prompt.index("[PROJECT BRAIN]"), prompt.index("\n\n[MEMORY]\n"))

    def test_extraction_runs_after_the_turn_and_persists(self):
        fake = self._install([
            _res("classifier says", ()),                 # classify -> MEDIUM
            _res("Noted."),                              # tool loop, no calls
            _res("rule|shipping.free|0.9|Free shipping over 50 euro."),   # extract
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        turn = agent.run("Remember: free shipping over 50 euro.")

        self.assertTrue(turn.facts_learned, "the turn learned nothing")
        self.assertEqual(turn.facts_learned[0]["key"], "shipping.free")
        # And it is durable: a fresh store on the same file still sees it.
        again = facts.FactStore(self.cfg.resolve_data_path(self.cfg.settings.facts_file))
        self.assertTrue(any("50 euro" in f["value"] for f in again.all()))
        _ = fake

    def test_correction_floors_importance(self):
        self._install([
            _res("MEDIUM"),
            _res("Sorry, fixing."),
            _res("preference|style.tabs|0.3|Use tabs, not spaces."),
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        turn = agent.run("No, that's wrong. I said use tabs, not spaces.")
        self.assertTrue(turn.facts_learned)
        self.assertGreaterEqual(turn.facts_learned[0]["importance"],
                                facts.CORRECTION_FLOOR)

    def test_facts_disabled_means_no_store_and_no_extraction(self):
        self.cfg.settings.facts_enabled = False
        self._install([_res("MEDIUM"), _res("ok")])
        agent = router.Agent(self.cfg, workdir=self.work)
        self.assertIsNone(agent.facts)
        turn = agent.run("Remember: free shipping over 50 euro.")
        self.assertEqual(turn.facts_learned, [])
        # With the store gone, neither the block nor the instructions about it
        # appear: describing a block the agent never sees invites it to invent one.
        self.assertNotIn("[FACTS]", agent.system_prompt())


class TestCheckpointsWiring(WiringCase):
    def test_write_then_undo_restores_bytes_exactly(self):
        target = os.path.join(self.work, "app.py")
        before = _read(target)
        self._install([
            _res("MEDIUM"),
            _res("", [("write_file", {"path": "app.py", "content": "BROKEN",
                                      "replace": True})]),
            _res("Done."),
            _res("COMPLETE"),                            # heartbeat
            _res("NONE"),                                # extraction
            _res("rewrote app.py"),                      # journal summary
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        turn = agent.run("Replace app.py")
        self.assertIn("app.py", turn.changed)
        self.assertEqual(_read(target), "BROKEN")

        res = agent.checkpoints.undo()
        self.assertTrue(res["ok"], res)
        self.assertEqual(_read(target), before)

    def test_undo_deletes_files_the_turn_created(self):
        cp = Checkpoints(self.work)
        cp.begin("t")
        ctx = tools_mod.new_context(self.work, checkpoints=cp)
        tools_mod.execute("write_file", {"path": "new.py", "content": "x = 1\n"}, ctx)
        self.assertTrue(os.path.exists(os.path.join(self.work, "new.py")))
        cp.commit()
        res = cp.undo()
        self.assertFalse(os.path.exists(os.path.join(self.work, "new.py")))
        self.assertEqual(res["deleted"], ["new.py"])

    def test_undo_turn_tool_reports_what_it_did(self):
        cp = Checkpoints(self.work)
        cp.begin("t")
        ctx = tools_mod.new_context(self.work, checkpoints=cp)
        tools_mod.execute("patch_file", {"path": "app.py", "old_text": "return x",
                                         "new_text": "return x * 2"}, ctx)
        out = tools_mod.execute("undo_turn", {}, ctx)
        self.assertIn("Restored 1", out)
        self.assertIn("return x\n", _read(os.path.join(self.work, "app.py")))

    def test_checkpoints_disabled_means_no_snapshots(self):
        self.cfg.settings.checkpoints_enabled = False
        agent = router.Agent(self.cfg, workdir=self.work)
        self.assertIsNone(agent.checkpoints)

    def test_second_write_does_not_overwrite_the_original_pre_state(self):
        target = os.path.join(self.work, "app.py")
        original = _read(target)
        cp = Checkpoints(self.work)
        cp.begin("t")
        ctx = tools_mod.new_context(self.work, checkpoints=cp)
        tools_mod.execute("write_file", {"path": "app.py", "content": "v1\n",
                                         "replace": True}, ctx)
        tools_mod.execute("write_file", {"path": "app.py", "content": "v2\n",
                                         "replace": True}, ctx)
        cp.commit()
        cp.undo()
        self.assertEqual(_read(target), original)

    def test_checkpoint_ids_are_unique_within_a_millisecond(self):
        cp = Checkpoints(self.work)
        ids = []
        for i in range(5):
            ids.append(cp.begin(f"turn {i}"))
            cp.record(os.path.join(self.work, "app.py"))
        self.assertEqual(len(set(ids)), 5, f"duplicate ids: {ids}")

    def test_a_named_checkpoint_is_undone_out_of_order(self):
        target = os.path.join(self.work, "app.py")
        original = _read(target)
        cp = Checkpoints(self.work)
        first = cp.begin("first")
        ctx = tools_mod.new_context(self.work, checkpoints=cp)
        tools_mod.execute("write_file", {"path": "app.py", "content": "v1\n",
                                         "replace": True}, ctx)
        cp.commit()
        cp.begin("second")
        tools_mod.execute("write_file", {"path": "other.py", "content": "x\n"}, ctx)
        cp.commit()

        res = cp.undo(first)
        self.assertTrue(res["ok"], res)
        self.assertEqual(_read(target), original)
        self.assertTrue(os.path.exists(os.path.join(self.work, "other.py")),
                        "undoing one turn must not touch another")

    def test_begin_commits_an_abandoned_checkpoint(self):
        target = os.path.join(self.work, "app.py")
        original = _read(target)
        cp = Checkpoints(self.work)
        cp.begin("turn that errored")
        ctx = tools_mod.new_context(self.work, checkpoints=cp)
        tools_mod.execute("write_file", {"path": "app.py", "content": "HALF\n",
                                         "replace": True}, ctx)
        # No commit: the turn blew up. Opening the next one must not drop the
        # previous turn's undo point.
        cp.begin("next turn")
        self.assertEqual(len(cp.list()), 1)
        cp.undo()
        self.assertEqual(_read(target), original)

    def test_subagent_gets_no_checkpoint(self):
        agent = router.Agent(self.cfg, workdir=self.work, depth=1)
        self.assertIsNone(agent.checkpoints)


class TestPlannerWiring(WiringCase):
    def test_complex_task_gets_a_plan_injected(self):
        self._install([
            _res("COMPLEX: Refactor the pricing module into a package."),
            _res("1. Read app.py and list its symbols\n"
                 "2. Split total() into pricing/total.py\n"
                 "3. Run the tests to prove nothing broke"),
            _res("Planned."),
            _res("NONE"),
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        turn = agent.run("Clean up the pricing code somehow")
        self.assertTrue(turn.plan)
        self.assertIn("[PLAN]", agent.system_prompt(plan=turn.plan))

    def test_useless_plan_is_dropped(self):
        self._install([
            _res("COMPLEX: do the thing"),
            _res("I'm sorry, I cannot plan that."),
            _res("ok"),
            _res("NONE"),
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        self.assertEqual(agent.run("do something vague").plan, "")

    def test_planner_disabled_skips_the_call(self):
        self.cfg.settings.planner_enabled = False
        fake = self._install([_res("COMPLEX: brief"), _res("ok"), _res("NONE")])
        agent = router.Agent(self.cfg, workdir=self.work)
        turn = agent.run("refactor everything")
        self.assertEqual(turn.plan, "")
        # classify + tool loop + extract = 3. A planner call would make it 4.
        self.assertEqual(len(fake.calls), 3)


class TestSkillsMode(WiringCase):
    def test_selected_skill_is_rendered_into_the_prompt(self):
        root = os.path.join(self.work, ".axium", "skills", "deploy-check")
        os.makedirs(root)
        with open(os.path.join(root, "SKILL.md"), "w", encoding="utf-8") as f:
            f.write("1. Run the tests\n2. Only then deploy\n")
        self._install([
            _res("SKILLS: deploy-check"),
            _res("Following the skill."),
            _res("NONE"),
        ])
        agent = router.Agent(self.cfg, workdir=self.work, mode="skills")
        turn = agent.run("ship it")
        self.assertEqual(turn.skills, ["deploy-check"])
        self.assertIn("Only then deploy",
                      skills.render(["deploy-check"], workdir=self.work))

    def test_hallucinated_skill_name_is_dropped(self):
        self._install([_res("SKILLS: does-not-exist"), _res("ok"), _res("NONE")])
        agent = router.Agent(self.cfg, workdir=self.work, mode="skills")
        self.assertEqual(agent.run("ship it").skills, [])


class TestJournalAndTrajectory(WiringCase):
    def test_a_changing_turn_writes_a_journal_entry(self):
        self._install([
            _res("MEDIUM"),
            _res("", [("append_file", {"path": "app.py", "content": "# note\n"})]),
            _res("Added a note."),
            _res("COMPLETE"),                            # heartbeat
            _res("NONE"),                                # extraction
            _res("appended a comment to app.py"),        # journal summary
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        agent.run("add a comment")
        self.assertIn("appended a comment", brain.recent_journal(self.work))

    def test_a_read_only_turn_writes_no_journal_entry(self):
        self._install([_res("MEDIUM"), _res("Nothing changed."), _res("NONE")])
        agent = router.Agent(self.cfg, workdir=self.work)
        agent.run("what does app.py do?")
        self.assertEqual(brain.recent_journal(self.work), "")

    def test_failure_is_mined_into_a_gotcha(self):
        self._install([
            _res("MEDIUM"),
            {"content": [], "model": "fake", "usage": {}, "error": "connect timeout to db",
             "latency_s": 0.0},
            {"content": [], "model": "fake", "usage": {}, "error": "connect timeout to db",
             "latency_s": 0.0},
            _res("NONE"),
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        agent.run("run the migration")
        stored = agent.facts.all()
        self.assertTrue(any(f["type"] == "gotcha" for f in stored), stored)


class TestUnits(unittest.TestCase):
    """Pure-function checks that need no agent."""

    def test_parse_extraction_survives_junk(self):
        rows = facts.parse_extraction(
            "rule|a.b|0.9|Real fact.\ngarbage line\n|||\nnote|c|notanumber|Second.")
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[1]["importance"], 0.5)

    def test_correction_detection_en_and_el(self):
        self.assertTrue(facts.looks_like_correction("No, that's wrong"))
        self.assertTrue(facts.looks_like_correction("οχι, μην το κανεις ετσι"))
        self.assertFalse(facts.looks_like_correction("add a test for the parser"))

    def test_parse_selection_drops_unknown(self):
        self.assertEqual(skills.parse_selection("SKILLS: a, ghost", ["a", "b"]), ["a"])
        self.assertEqual(skills.parse_selection("NONE", ["a"]), [])

    def test_write_skill_refuses_a_traversing_name(self):
        root = tempfile.mkdtemp(prefix="axium-skills-")
        escaped = os.path.join(os.path.dirname(root), "escape")
        try:
            for bad in ("../escape", "a/b", "..", "Bad Name", "under_score"):
                self.assertEqual(
                    trajectory.write_skill(
                        {"name": bad, "description": "d", "body": "1. go"}, root),
                    "", f"accepted {bad!r}")
            self.assertFalse(os.path.exists(escaped))
        finally:
            shutil.rmtree(root, ignore_errors=True)

    def test_write_skill_refuses_to_overwrite_an_existing_skill(self):
        root = tempfile.mkdtemp(prefix="axium-skills-")
        try:
            skill = {"name": "ship", "description": "d", "body": "1. go"}
            self.assertTrue(trajectory.write_skill(skill, root))
            human = os.path.join(root, "ship", "SKILL.md")
            with open(human, "w", encoding="utf-8") as f:
                f.write("# Hand written\n")
            self.assertEqual(trajectory.write_skill(skill, root), "")
            self.assertEqual(_read(human), "# Hand written\n")
        finally:
            shutil.rmtree(root, ignore_errors=True)

    def test_parse_skill_rejects_junk(self):
        self.assertIsNone(trajectory.parse_skill("not json"))
        self.assertIsNone(trajectory.parse_skill('{"name": "Bad Name", "body": "x"}'))
        good = trajectory.parse_skill(
            '```json\n{"name": "ship-it", "description": "d", "body": "1. go"}\n```')
        self.assertEqual(good["name"], "ship-it")

    def test_planner_is_useful_needs_real_steps(self):
        self.assertFalse(planner.is_useful("ok"))
        self.assertFalse(planner.is_useful("I cannot help with that request at all."))
        self.assertTrue(planner.is_useful("1. Read app.py\n2. Patch total()\n3. Test"))

    def test_planner_accepts_alternative_step_markers(self):
        self.assertTrue(planner.is_useful("1) Read app.py\n2) Patch total()\n3) Run tests"))
        self.assertTrue(planner.is_useful("1 - Read app.py\n2 - Patch total()"))

    def test_planner_rejects_a_pasted_line_numbered_listing(self):
        listing = "1024 def total(x):\n1025     return x\n1026 # end of file"
        self.assertFalse(planner.is_useful(listing))

    def test_planner_rejects_unnumbered_prose_and_single_steps(self):
        self.assertFalse(planner.is_useful(
            "You should probably look at the pricing module and then change the "
            "discount threshold, after which the tests ought to pass fine."))
        self.assertFalse(planner.is_useful(
            "1. Refactor the pricing module into a package as the user asked."))

    def test_planner_prompt_orders_and_truncates(self):
        out = planner.build_prompt("Fix the discount", "Stack: Python.", "- (rule) Free over 50.")
        self.assertLess(out.index("ALREADY KNOWN"), out.index("STANDING FACTS"))
        self.assertLess(out.index("STANDING FACTS"), out.index("[TASK]"))
        bare = planner.build_prompt("Fix it", "", "   ")
        self.assertTrue(bare.startswith("[TASK]"))
        huge = planner.build_prompt("keep me", "x" * 20000, "y" * 20000)
        self.assertIn("keep me", huge)
        self.assertLess(len(huge), planner.MAX_CONTEXT_CHARS + 2200)

    def test_mine_failure_ignores_noise(self):
        self.assertIsNone(trajectory.mine_failure("x", "short"))
        self.assertIsNone(trajectory.mine_failure("x", "!!!!!!!!!!!!!!"))
        self.assertIsNotNone(trajectory.mine_failure("x", "ConnectionError: refused by host"))


if __name__ == "__main__":
    unittest.main(verbosity=2)


class TestRuntimeVerification(unittest.TestCase):
    """The gap `get_diagnostics` leaves: code that parses but does not run."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="axium-verify-")
        os.makedirs(os.path.join(self.tmp, "pkg"))
        for name, body in [("__init__.py", ""), ("mod.py", "def total(x):\n    return x\n")]:
            with open(os.path.join(self.tmp, "pkg", name), "w", encoding="utf-8") as f:
                f.write(body)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _write(self, rel, body):
        with open(os.path.join(self.tmp, rel), "w", encoding="utf-8") as f:
            f.write(body)

    def test_a_file_that_parses_but_fails_to_import_is_caught(self):
        # This is the whole point: ast.parse accepts it, importing does not.
        self._write("pkg/mod.py", "import definitely_not_a_real_module_xyz\n")
        from axium import tools as t
        ctx = t.new_context(self.tmp)
        self.assertIn("No syntax problems",
                      t.execute("get_diagnostics", {"path": "pkg/mod.py"}, ctx),
                      "the syntax check should see nothing wrong: that is the gap")
        res = verify.verify(self.tmp, ["pkg/mod.py"])
        self.assertFalse(res.ok)
        self.assertEqual(res.kind, "import")
        self.assertIn("definitely_not_a_real_module_xyz", res.detail)

    def test_working_code_passes(self):
        self.assertTrue(verify.verify(self.tmp, ["pkg/mod.py"]).ok)

    def test_a_failing_test_suite_is_caught(self):
        os.makedirs(os.path.join(self.tmp, "tests"))
        self._write("tests/acceptance.py", "raise SystemExit(1)\n")
        res = verify.verify(self.tmp, ["pkg/mod.py"])
        self.assertFalse(res.ok)
        self.assertEqual(res.kind, "tests")

    def test_an_unverifiable_project_skips_rather_than_fails(self):
        # "we could not verify" and "it is broken" are different claims, and
        # reporting the second teaches the agent to ignore the check.
        bare = tempfile.mkdtemp(prefix="axium-bare-")
        try:
            with open(os.path.join(bare, "notes.md"), "w", encoding="utf-8") as f:
                f.write("# not code\n")
            res = verify.verify(bare, ["notes.md"])
            self.assertTrue(res.ok)
            self.assertTrue(res.skipped)
        finally:
            shutil.rmtree(bare, ignore_errors=True)

    def test_a_turn_that_changed_nothing_is_skipped(self):
        self.assertTrue(verify.verify(self.tmp, []).skipped)

    def test_import_failure_is_reported_before_test_failure(self):
        # Handing the agent a downstream symptom sends it to the wrong file.
        os.makedirs(os.path.join(self.tmp, "tests"))
        self._write("tests/acceptance.py", "raise SystemExit(1)\n")
        self._write("pkg/mod.py", "import definitely_not_a_real_module_xyz\n")
        self.assertEqual(verify.verify(self.tmp, ["pkg/mod.py"]).kind, "import")

    def test_feedback_names_the_cause_and_is_empty_when_fine(self):
        self.assertEqual(verify.Result().as_feedback(), "")
        self.assertEqual(verify.Result(skipped=True).as_feedback(), "")
        msg = verify.Result(ok=False, kind="import", detail="boom").as_feedback()
        self.assertIn("boom", msg)
        self.assertIn("does not run", msg)


class TestEditEscalation(WiringCase):
    """Three failed edits means the model has a stale picture, not bad luck.

    The feature ships default-OFF (it never fired once in 21 benchmark runs), so
    these turn it on explicitly: a test of a mechanism has to enable it.
    """

    def setUp(self):
        super().setUp()
        self.cfg.settings.edit_escalation = True

    def _script_failing_patches(self, n):
        calls = [_res("MEDIUM")]
        for _ in range(n):
            calls.append(_res("", [("patch_file", {"path": "app.py",
                                                   "old_text": "NOT PRESENT",
                                                   "new_text": "x"})]))
        calls.append(_res("Gave up."))
        return calls

    def test_the_next_call_escalates_after_three_failed_edits(self):
        fake = self._install(self._script_failing_patches(4))
        agent = router.Agent(self.cfg, workdir=self.work)
        agent.run("patch it")
        # Call 0 is the classifier. Loop call 1 is primary by routing; 2 and 3
        # are continuation; by call 4 three edits have failed, so it escalates.
        loop_models = [c["model"] for c in fake.calls[1:]]
        self.assertIn(self.cfg.models.primary, loop_models[3:],
                      f"never escalated: {loop_models}")

    def test_a_successful_edit_resets_the_count(self):
        fake = self._install([
            _res("MEDIUM"),
            _res("", [("patch_file", {"path": "app.py", "old_text": "NOPE", "new_text": "x"})]),
            _res("", [("patch_file", {"path": "app.py", "old_text": "NOPE", "new_text": "x"})]),
            _res("", [("patch_file", {"path": "app.py", "old_text": "return x",
                                      "new_text": "return x + 1"})]),
            _res("", [("patch_file", {"path": "app.py", "old_text": "NOPE", "new_text": "x"})]),
            _res("Done."), _res("COMPLETE"), _res("NONE"), _res("patched"),
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        agent.run("patch it")
        # Two fails, a success, one fail: never three in a row, so no escalation.
        loop_models = [c["model"] for c in fake.calls[1:5]]
        self.assertNotIn(self.cfg.models.primary, loop_models[1:],
                         f"escalated without three consecutive failures: {loop_models}")

    def test_a_failed_command_is_not_a_strike(self):
        # run_command exiting non-zero is often the agent correctly finding a
        # broken test. Counting it would escalate on honest work.
        fake = self._install([
            _res("MEDIUM"),
            _res("", [("run_command", {"command": "exit 1"})]),
            _res("", [("run_command", {"command": "exit 1"})]),
            _res("", [("run_command", {"command": "exit 1"})]),
            _res("", [("run_command", {"command": "exit 1"})]),
            _res("Done."), _res("COMPLETE"), _res("NONE"),
        ])
        agent = router.Agent(self.cfg, workdir=self.work)
        agent.run("run it")
        loop_models = [c["model"] for c in fake.calls[2:5]]
        self.assertNotIn(self.cfg.models.primary, loop_models,
                         f"a failing command escalated: {loop_models}")

    def test_escalation_disabled_never_escalates(self):
        self.cfg.settings.edit_escalation = False
        fake = self._install(self._script_failing_patches(5))
        agent = router.Agent(self.cfg, workdir=self.work)
        agent.run("patch it")
        self.assertNotIn(self.cfg.models.primary, [c["model"] for c in fake.calls[2:]])
