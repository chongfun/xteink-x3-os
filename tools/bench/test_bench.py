#!/usr/bin/env python3
from __future__ import annotations

import argparse
import contextlib
import io
import json
import re
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any, ClassVar
from unittest.mock import patch

import bench


class BenchParserTests(unittest.TestCase):
    def test_parse_structured_render(self) -> None:
        event = bench.parse_line(
            "bench: render view=Reading mode=Fast page=12 chapter=5 layout_ms=22 flush_ms=438 prestage_ms=15 t_ms=96260",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "render")
        self.assertEqual(event["view"], "Reading")
        self.assertEqual(event["mode"], "Fast")
        self.assertEqual(event["flush_ms"], 438)

    def test_parse_prestage_event(self) -> None:
        event = bench.parse_line(
            "bench: prestage staged=true elapsed_ms=24 t_ms=96284",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "prestage")
        self.assertEqual(event["elapsed_ms"], 24)
        self.assertTrue(event["staged"])

    def test_prestage_values_prefers_standalone_events(self) -> None:
        events = [
            {"event": "render", "view": "Reading", "layout_ms": 13, "t_ms": 100},
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "t_ms": 124},
        ]
        self.assertEqual(bench.prestage_values(events), [24])

    def test_prestage_values_falls_back_to_pre_split_render_events(self) -> None:
        """Captures from before the render/prestage split still summarize."""
        events = [
            {"event": "render", "view": "Reading", "prestage_ms": 28, "t_ms": 100},
            {"event": "render", "view": "Reading", "prestage_ms": 27, "t_ms": 200},
        ]
        self.assertEqual(bench.prestage_values(events), [28, 27])

    def test_prestage_values_combines_standalone_and_legacy_render_events(self) -> None:
        """Log containing both firmware event shapes combines samples in ordering."""
        events = [
            {"event": "render", "view": "Reading", "prestage_ms": 28, "t_ms": 100},
            {"event": "render", "view": "Reading", "layout_ms": 13, "t_ms": 200},
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "t_ms": 224},
        ]
        self.assertEqual(bench.prestage_values(events), [28, 24])

    def test_page_turn_duration_ends_at_the_render_event(self) -> None:
        """press-to-settled must not absorb the prestage that follows it."""
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1424},
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "t_ms": 1448},
        ]
        self.assertEqual(bench.page_turn_durations(events), [424])

    def test_a_failed_sleep_flush_parses_to_one_record(self) -> None:
        """The firmware prints a human line beside the structured one.

        Parsing both recorded one failure twice, so a single failed flush was
        reported as two failed sleep phases. The error println stays in the
        firmware — error paths are unconditional there — so the parser is the
        side that has to ignore it.
        """
        lines = [
            "display: sleep framebuffer flush failed",
            "bench: sleep phase=refresh ok=false elapsed_ms=4200 t_ms=61000",
        ]
        events = [event for line in lines for event in bench.parse_line(line, "sleep-sync")]
        self.assertEqual(
            [(event["event"], event.get("phase"), event.get("ok")) for event in events],
            [("sleep", "refresh", False)],
        )

    def test_parse_legacy_render(self) -> None:
        event = bench.parse_line(
            "bench: render Reading Fast page=12 ch=5 layout=24ms flush=438ms prestage=16ms t=93958",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "render")
        self.assertEqual(event["view"], "Reading")
        self.assertEqual(event["mode"], "Fast")
        self.assertTrue(event["legacy"])

    def test_button_normalization(self) -> None:
        event = bench.parse_line(
            "bench: input button=Some(Next) aux=2061 nav=5 page_raw=2937 t_ms=10524",
            "page-turn",
        )[0]
        self.assertEqual(event["button"], "Next")

    def test_parse_deep_sleep_wake_line(self) -> None:
        event = bench.parse_line(
            "main: deep_sleep_wake=true (gpio=true, sleep_image=true)",
            "sleep-sync",
        )[0]
        self.assertEqual(event["event"], "boot")
        self.assertTrue(event["deep_sleep_wake"])
        self.assertTrue(event["gpio"])
        self.assertTrue(event["sleep_image"])

    def test_parse_cold_boot_line(self) -> None:
        event = bench.parse_line(
            "main: deep_sleep_wake=false (gpio=false, sleep_image=false)",
            "sleep-sync",
        )[0]
        self.assertEqual(event["event"], "boot")
        self.assertFalse(event["gpio"])

    def test_parse_boot_stage_line(self) -> None:
        event = bench.parse_line("main: spawn display t_ms=142", "sleep-sync")[0]
        self.assertEqual(event["event"], "boot_stage")
        self.assertEqual(event["stage"], "main: spawn display")
        self.assertEqual(event["t_ms"], 142)

    def test_boot_stage_line_without_t_ms_is_ignored(self) -> None:
        """Logs from firmware predating the t_ms stamps must not parse."""
        self.assertEqual(bench.parse_line("main: spawn display", "sleep-sync"), [])
        self.assertEqual(bench.parse_line("sd: session enter", "page-turn"), [])


class BenchReportTests(unittest.TestCase):
    def test_page_turn_duration_pairs_input_with_next_reading_render(self) -> None:
        events = [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 100},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "mode": "Fast",
                "t_ms": 560,
            },
        ]
        self.assertEqual(bench.page_turn_durations(events), [460])

    def test_budget_warning_for_slow_page_turn(self) -> None:
        events = [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 100},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "mode": "Fast",
                "t_ms": 800,
            },
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("page-turn median" in warning for warning in warnings))

    def test_suite_signal_warning_for_empty_capture(self) -> None:
        warnings = bench.evaluate_suite_signals(
            [
                {"suite": "storage-cache", "event": "run_start"},
                {"suite": "storage-cache", "event": "run_end"},
            ]
        )
        self.assertEqual(warnings, ["storage-cache: no parsed bench telemetry"])

    def test_strict_report_uses_suite_signal_validation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "empty.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"suite": "storage-cache", "event": "run_start"}),
                        json.dumps({"suite": "storage-cache", "event": "run_end"}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            warnings = bench.summarize_paths([path], None, validate_suites=True)
        self.assertEqual(warnings, ["storage-cache: no parsed bench telemetry"])

    @patch("builtins.print")
    def test_summarize_paths_default_reports_latest_run(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"suite": "run1", "event": "run_start"}),
                        json.dumps(
                            {"suite": "run1", "event": "render", "view": "Reading", "mode": "Fast"}
                        ),
                        json.dumps({"suite": "run2", "event": "run_start"}),
                        json.dumps({"suite": "run2", "event": "refresh", "busy_ms": 100}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)

            # Since latest_only=True, we should only see run2's events.
            mock_print.assert_any_call("events:        2")
            mock_print.assert_any_call("renders:       0")
            mock_print.assert_any_call(
                "bench report: latest run only (run2; 1 earlier run(s) in the log — pass --all to pool)"
            )

    @patch("builtins.print")
    def test_summarize_paths_all_reports_every_run(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"suite": "run1", "event": "run_start"}),
                        json.dumps(
                            {"suite": "run1", "event": "render", "view": "Reading", "mode": "Fast"}
                        ),
                        json.dumps({"suite": "run2", "event": "run_start"}),
                        json.dumps({"suite": "run2", "event": "refresh", "busy_ms": 100}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None, latest_only=False)

            # Since latest_only=False, we should see both run1 and run2's events.
            mock_print.assert_any_call("events:        4")
            mock_print.assert_any_call("renders:       1")


class BudgetLoadingTests(unittest.TestCase):
    def test_none_path_is_intentionally_disabled(self) -> None:
        self.assertEqual(bench.load_budgets(None), ({}, None))

    def test_missing_file_reports_problem(self) -> None:
        budgets, problem = bench.load_budgets(Path("does-not-exist.toml"))
        self.assertEqual(budgets, {})
        self.assertIsNotNone(problem)

    def _write_minimal_log(self, tmp: str) -> Path:
        path = Path(tmp) / "log.jsonl"
        path.write_text(
            json.dumps({"suite": "page-turn", "event": "render", "view": "Reading", "t_ms": 100})
            + "\n",
            encoding="utf-8",
        )
        return path

    def test_strict_report_fails_loudly_when_budgets_unloadable(self) -> None:
        """--strict must exit non-zero rather than verify nothing.

        The file parses; it is the section with no keys in it that makes the
        document unusable, which is a real budget file this could be handed.
        """
        with tempfile.TemporaryDirectory() as tmp:
            log = self._write_minimal_log(tmp)
            budgets = Path(tmp) / "benches.toml"
            budgets.write_text("[page-turn]\n", encoding="utf-8")
            with self.assertRaises(SystemExit) as ctx:
                bench.summarize_paths([log], budgets, validate_suites=True)
        self.assertIn("--strict cannot enforce budgets", str(ctx.exception))

    @patch("builtins.print")
    def test_non_strict_report_warns_when_budgets_unloadable(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log = self._write_minimal_log(tmp)
            budgets = Path(tmp) / "benches.toml"
            budgets.write_text("[page-turn]\n", encoding="utf-8")
            warnings = bench.summarize_paths([log], budgets)
        self.assertEqual(warnings, [])
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("budgets not checked", printed)

    @patch("builtins.print")
    def test_strict_report_flags_empty_log(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "empty.jsonl"
            path.write_text("", encoding="utf-8")
            warnings = bench.summarize_paths([path], None, validate_suites=True)
        self.assertEqual(warnings, ["no events parsed"])

    def test_checked_in_budgets_have_no_dead_keys(self) -> None:
        """Every key in checked-in budget files must be read somewhere in bench.py.

        Scans the TOML textually rather than parsing it, so this runs on the
        interpreter `tools/check.sh` actually invokes. Skipping here on 3.9
        meant the dead-key guard did not run under the repo's own python --
        the same "gate that is silently absent" shape this file exists to
        stop. A dead key is a budget that reads as enforced and is not, which
        is worse than no budget at all.
        """
        source = Path(bench.__file__).read_text(encoding="utf-8")
        for budget_file in (bench.DEFAULT_BUDGETS, bench.DEFAULT_BUDGETS_X3):
            text = budget_file.read_text(encoding="utf-8")
            keys = re.findall(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=", text, re.MULTILINE)
            self.assertTrue(
                keys, f"no budget keys found in {budget_file.name} -- the scan itself is broken"
            )
            for key in keys:
                self.assertIn(
                    f'"{key}"', source, f"budget key {key} in {budget_file.name} is read by nothing"
                )


class PageTurnTrustTests(unittest.TestCase):
    def _burst_events(self) -> list:
        # Operator triple-taps during one refresh. The render settles at 1500
        # having begun at 1500-10-400 = 1090, so it answers the two presses
        # that preceded it (one coalesced) and the third is still unanswered
        # when the capture ends. Both buckets, one fixture.
        return [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 1000},
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 1080},
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 1160},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "t_ms": 1500,
                "layout_ms": 10,
                "flush_ms": 400,
            },
        ]

    def test_burst_capture_is_untrusted(self) -> None:
        stats = bench.page_turn_stats(self._burst_events())
        # Measured from the newest press the render answered (1080), not the
        # oldest — charging the oldest is what made a burst look like a slow
        # device rather than a fast operator.
        self.assertEqual(stats.durations, [420])
        self.assertEqual(stats.presses, 3)
        self.assertEqual(stats.coalesced_presses, 1)
        self.assertEqual(stats.unmatched_presses, 1)
        self.assertEqual(stats.untrusted_presses, 2)
        self.assertFalse(stats.median_trusted)

    def test_a_repaint_is_not_charged_to_the_next_press(self) -> None:
        """The defect this pairing exists to prevent.

        Until #56 the firmware repainted twice per turn. Under render-driven
        FIFO pairing the second repaint consumed the *following* press and
        credited it with a near-zero duration, which is where the real
        fixture's 2 ms samples came from. The repaint begins before that
        press lands, so it must answer nothing.
        """
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            # The turn the reader actually sees.
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 1500,
                "layout_ms": 10,
                "flush_ms": 400,
            },
            {"event": "input", "button": "Next", "t_ms": 1600},
            # Redundant repaint: began at 1650-10-400 = 1240, before the
            # second press existed, so it cannot be that press's frame.
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 1650,
                "layout_ms": 10,
                "flush_ms": 400,
            },
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 2100,
                "layout_ms": 10,
                "flush_ms": 400,
            },
        ]
        stats = bench.page_turn_stats(events)
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.reading_renders, 3)
        self.assertEqual(stats.coalesced_presses, 0)
        self.assertEqual(stats.unmatched_presses, 0)
        self.assertTrue(stats.median_trusted)

    def test_deliberate_capture_is_trusted(self) -> None:
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500},
            {"event": "input", "button": "Next", "t_ms": 3000},
            {"event": "render", "view": "Reading", "t_ms": 3500},
        ]
        stats = bench.page_turn_stats(events)
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.unmatched_presses, 0)
        self.assertTrue(stats.median_trusted)

    def test_untrusted_median_is_not_checked_against_budget(self) -> None:
        warnings = bench.evaluate_budgets(
            self._burst_events(),
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("median excludes" in warning for warning in warnings), warnings)
        self.assertFalse(any("above warning budget" in warning for warning in warnings))

    def test_budget_floor_warns_on_suspiciously_fast_median(self) -> None:
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1030},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_min_ms": 250}},
        )
        self.assertTrue(any("below plausibility floor" in warning for warning in warnings))

    def test_suite_signals_flag_untrusted_page_turn(self) -> None:
        warnings = bench.evaluate_suite_signals(self._burst_events())
        self.assertTrue(any("produced no page turn" in warning for warning in warnings))

    @patch("builtins.print")
    def test_report_suppresses_untrusted_median(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(event) for event in self._burst_events()) + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("EXCLUDED", printed)
        self.assertIn("median suppressed", printed)
        self.assertNotIn("page turn      median=", printed)
        self.assertIn("presses=3 page_turns=1", printed)
        self.assertIn("coalesced=1", printed)
        self.assertIn("unmatched=1", printed)


class PageTurnEpochTests(unittest.TestCase):
    """Pairing must not reach across a device time epoch.

    `t_ms` is device uptime: it restarts at every reboot, and each capture in
    a pooled log carries its own. Sorting globally by it interleaves clocks.
    """

    def test_two_runs_do_not_pair_across_each_other(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500},
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1200},
            {"event": "render", "view": "Reading", "t_ms": 1700},
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        # Globally sorted these are press,press,render,render: the first
        # render answers both, inventing a coalesced press and a 300 ms turn.
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.coalesced_presses, 0)
        self.assertEqual(stats.unmatched_presses, 0)

    def test_runs_are_split_even_when_uptime_never_goes_backwards(self) -> None:
        """The run split is load-bearing on its own, not just via reboots.

        Two captures against a device that was never reset have increasing
        `t_ms` across the boundary, so no regression marks it and the boot
        segmentation cannot see it. Here the first capture ends immediately
        after a press — the operator stopped recording — and without the run
        split that press is answered by the *next capture's* first render,
        inventing a cross-capture duration out of nothing.
        """
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "run_start", "suite": "page-turn"},
            {"event": "render", "view": "Reading", "t_ms": 2000},
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [])
        self.assertEqual(stats.unmatched_presses, 1)

    def test_a_reboot_inside_one_run_splits_the_pairing(self) -> None:
        events = [
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 1600},
            # Woken: uptime restarts and overlaps the pre-sleep range.
            {"event": "input", "button": "Next", "t_ms": 1200},
            {"event": "render", "view": "Reading", "t_ms": 1700},
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [500, 500])
        self.assertEqual(stats.coalesced_presses, 0)


class RenderBeginTests(unittest.TestCase):
    """`req_ms` is the boundary; the subtraction is only a fallback.

    The app stamps `req_ms` as it freezes the request, which is when the state
    the frame shows stopped being able to change; the firmware's dequeue of
    that request is the separate, later `deq_ms`, reported but never paired
    on. Inferring the start as `t_ms - layout_ms - flush_ms` omits
    the work outside those two intervals — catalog and TOC reads ahead of
    layout, and after the flush a framebuffer copy plus a chapter-tracking
    read that touches the card at chapter boundaries. Every omission pushes
    the estimate *later* than the truth, which is the direction that mispairs.
    """

    def _render(self, **over) -> dict:
        event = {
            "event": "render",
            "view": "Reading",
            "t_ms": 2000,
            "layout_ms": 10,
            "flush_ms": 400,
        }
        event.update(over)
        return event

    def test_req_ms_wins_over_the_inferred_start(self) -> None:
        self.assertEqual(bench.render_begin_ms(self._render(req_ms=1500)), 1500)

    def test_the_inferred_start_is_the_fallback_for_older_logs(self) -> None:
        self.assertEqual(bench.render_begin_ms(self._render()), 1590)

    def test_a_press_during_the_queue_wait_belongs_to_the_next_frame(self) -> None:
        """A render can wait in the channel behind the display task's work.

        `req_ms` is stamped when the app freezes the state, not when the
        display task dequeues. Frame A was frozen at 1100 and did not reach
        the panel pipeline until 1500, so the press at 1200 arrived while A
        was already queued and immutable: it belongs to B. Pairing on the
        dequeue instant would have charged it to A and reported a 700 ms
        turn instead of A's real 800 ms and B's 1100 ms.
        """
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "input", "button": "Next", "t_ms": 1200},
            self._render(req_ms=1100, deq_ms=1500, t_ms=1900),
            self._render(req_ms=1950, deq_ms=1950, t_ms=2300),
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [900, 1100])
        self.assertEqual(stats.coalesced_presses, 0)
        self.assertEqual(stats.unmatched_presses, 0)

    def test_pairing_survives_past_the_32_bit_uptime_horizon(self) -> None:
        """`requested_at_ms` is a `u64`, so there is no wrap to reconstruct.

        Held as a boundary case because it was nearly a `u32`. Truncating at
        49.7 days would have restarted `req_ms` near zero while the press and
        settle timestamps kept counting, so every press after the wrap looked
        later than the render it belonged to, went unanswered, and eventually
        suppressed the median or failed `--strict` — on long soaks, which is
        the run most likely to reach it and least likely to be re-run.
        """
        wrap = 2**32
        events = [
            {"event": "input", "button": "Next", "t_ms": wrap - 500},
            self._render(req_ms=wrap - 400, deq_ms=wrap - 300, t_ms=wrap + 100),
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [600])
        self.assertEqual(stats.unmatched_presses, 0)

    def test_an_unstamped_request_falls_back(self) -> None:
        """0 means unstamped, not "frozen at boot"."""
        self.assertEqual(bench.render_begin_ms(self._render(req_ms=0)), 1590)

    def test_a_press_after_the_request_was_frozen_is_not_credited(self) -> None:
        """The defect `req_ms` exists to close.

        The press lands at 1550: after the request was frozen at 1500, but
        before the inferred start of 1590. It cannot be in this frame, yet
        the subtraction would credit it with a 450 ms turn.
        """
        events = [
            {"event": "input", "button": "Next", "t_ms": 1550},
            self._render(req_ms=1500),
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [])
        self.assertEqual(stats.unmatched_presses, 1)

        without = [
            {"event": "input", "button": "Next", "t_ms": 1550},
            self._render(),
        ]
        self.assertEqual(bench.page_turn_stats_over_epochs(without).durations, [450])


class BudgetCoverageTests(unittest.TestCase):
    """A budget with nothing to measure must not read as enforced."""

    def test_a_configured_budget_with_no_samples_warns(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1500, "layout_ms": 12},
            # No refresh events at all, so the Fast-refresh budget covers
            # nothing -- and used to pass --strict while covering nothing.
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"fast_refresh_busy_warn_ms": 500}},
        )
        self.assertTrue(
            any("fast_refresh_busy_warn_ms" in w and "nothing was measured" in w for w in warnings),
            warnings,
        )

    def test_a_budget_with_samples_does_not_warn_about_coverage(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "refresh", "mode": "Fast", "busy_ms": 380},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"fast_refresh_busy_warn_ms": 500}},
        )
        self.assertFalse(any("nothing was measured" in w for w in warnings), warnings)

    def test_another_suites_budgets_are_not_faulted(self) -> None:
        """A page-turn capture holds no storage telemetry, and should not be
        told off for it -- otherwise every report warns about every suite."""
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "refresh", "mode": "Fast", "busy_ms": 380},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {
                "page-turn": {"fast_refresh_busy_warn_ms": 500},
                "storage-cache": {"catalog_load_warn_ms": 500},
            },
        )
        self.assertFalse(any("catalog_load_warn_ms" in w for w in warnings), warnings)

    def test_a_workflow_suite_is_gated_by_the_budgets_it_exercises(self) -> None:
        """`reader-soak` turns pages, so the page-turn budgets apply to it.

        Sections are named after the suite that owns them, but resolving the
        name literally left every reader-soak and thermal-run capture with no
        section in play and therefore nothing enforced.
        """
        events = [
            {"event": "run_start", "suite": "reader-soak"},
            {"event": "refresh", "mode": "Fast", "busy_ms": 900},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"fast_refresh_busy_warn_ms": 500}},
        )
        self.assertTrue(
            any("Fast refresh busy" in w and "above warning budget" in w for w in warnings),
            warnings,
        )


class BudgetSuiteIsolationTests(unittest.TestCase):
    """Pooled runs (`--all`) must not decide each other's verdicts."""

    def test_another_suites_page_turns_do_not_move_the_median(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1400, "req_ms": 1000},
            {"event": "input", "button": "Next", "t_ms": 2000},
            {"event": "render", "view": "Reading", "t_ms": 2400, "req_ms": 2000},
            # A sleep-sync run in the same file: its turns straddle the idle
            # timeout and the wake, so pooling them puts the page-turn median
            # at 2200 ms and fails a suite that was well inside its budget.
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 5000, "req_ms": 1000},
            {"event": "input", "button": "Next", "t_ms": 6000},
            {"event": "render", "view": "Reading", "t_ms": 10000, "req_ms": 6000},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertFalse(any("page-turn median" in w for w in warnings), warnings)

    def test_another_suites_storage_samples_do_not_fail_the_catalog_budget(self) -> None:
        events = [
            {"event": "run_start", "suite": "storage-cache"},
            {
                "event": "storage_catalog",
                "action": "load",
                "elapsed_ms": 100,
            },
            {"event": "run_start", "suite": "reader-soak"},
            # A cold catalog read inside a soak: real, but not what the
            # storage-cache budget describes.
            {
                "event": "storage_catalog",
                "action": "load",
                "elapsed_ms": 4000,
            },
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"storage-cache": {"catalog_load_warn_ms": 500}},
        )
        self.assertFalse(any("catalog load" in w for w in warnings), warnings)

    def test_a_thermal_run_is_gated_by_the_workflow_it_selected(self) -> None:
        """`--suite sleep-sync` must bring the sleep-sync budgets with it.

        The flag chose the workload and was then discarded, so every thermal
        run resolved to page-turn: a sleep-cycle capture could hold no sleep,
        no wake and no Full refresh and still pass `--strict`, which is the
        selected-but-unchecked shape the branch exists to remove.
        """
        events = [
            {"event": "run_start", "suite": "thermal-run", "workflow": "sleep-sync"},
            {"event": "refresh", "mode": "Full", "busy_ms": 5000},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"sleep-sync": {"full_refresh_busy_max_ms": 4300}},
        )
        self.assertTrue(any("above budget ceiling" in w for w in warnings), warnings)

    def test_a_thermal_run_with_no_recorded_workflow_is_reported(self) -> None:
        """Captured before the workflow was recorded: guessing it is worse.

        The argparse default is `page-turn`, so assuming it would gate a
        sleep-cycle capture against page-turn budgets and call that enforced.
        """
        events = [
            {"event": "run_start", "suite": "thermal-run"},
            {"event": "refresh", "mode": "Fast", "busy_ms": 400},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"fast_refresh_busy_warn_ms": 500}},
        )
        self.assertTrue(any("thermal-run has no budget section" in w for w in warnings), warnings)

    def test_a_suite_no_budget_section_claims_is_reported(self) -> None:
        events = [
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "refresh", "mode": "Full", "busy_ms": 3500},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"fast_refresh_busy_warn_ms": 500}},
        )
        self.assertTrue(any("sleep-sync has no budget section" in w for w in warnings), warnings)

    def test_an_unlabelled_log_still_measures_everything(self) -> None:
        """Hand-built fixtures and captures predating suite tagging."""
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 2000, "req_ms": 1000},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("page-turn median" in w for w in warnings), warnings)


class PerRunCoverageTests(unittest.TestCase):
    """Pooling is for the statistic, never for "was anything measured?".

    `--all` reports several runs at once. Coverage checked on the pooled list
    lets a complete capture answer for an incomplete one: neither run holds
    the signal set it owed, but between them every check finds a sample.
    """

    def test_a_sibling_run_does_not_cover_a_missing_budget_sample(self) -> None:
        events = [
            # A sleep-sync run that reached its sleep but recorded no Full
            # refresh — the panel work the budget exists to bound.
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
            # A second one that refreshed but never slept.
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "refresh", "mode": "Full", "busy_ms": 3500},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"sleep-sync": {"full_refresh_busy_min_ms": 3000}},
        )
        self.assertTrue(
            any(
                "full_refresh_busy_min_ms" in w
                and "nothing was measured" in w
                and "run 1 of 2" in w
                for w in warnings
            ),
            warnings,
        )

    def test_a_run_with_its_own_samples_is_not_faulted(self) -> None:
        events = [
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "refresh", "mode": "Full", "busy_ms": 3400},
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "refresh", "mode": "Full", "busy_ms": 3500},
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"sleep-sync": {"full_refresh_busy_min_ms": 3000}},
        )
        self.assertFalse(any("nothing was measured" in w for w in warnings), warnings)

    def test_a_sibling_run_does_not_cover_a_missing_suite_signal(self) -> None:
        events = [
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "refresh", "mode": "Full", "busy_ms": 3500},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(
            any("run 2 of 2: no completed sleep captured" in w for w in warnings),
            warnings,
        )

    def test_the_pooled_statistic_still_spans_every_run(self) -> None:
        """Coverage is per run; the median it protects is still the pool's."""
        events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1400, "req_ms": 1000},
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 3000, "req_ms": 1000},
        ]
        # Medians of 400 and 2000 pool to 1200, over the budget; neither run
        # alone would be, and the pooled figure is the one being gated.
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("page-turn median 1200ms" in w for w in warnings), warnings)


class TerminalSleepTests(unittest.TestCase):
    """Asking for a sleep is not sleeping.

    `phase=requested` opens the transition and `refresh`/`power_down_*` are
    steps inside it that a failed handshake reaches too. Accepting any `sleep`
    event let a sleep-sync capture that never put the panel down pass
    `--strict` with its Full-refresh budget satisfied and no failed phase to
    report.
    """

    REQUESTED_ONLY: ClassVar[list[dict[str, Any]]] = [
        {"event": "run_start", "suite": "sleep-sync"},
        {"event": "refresh", "mode": "Full", "busy_ms": 3500},
        {"event": "sleep", "phase": "requested", "screen_on": True, "t_ms": 40000},
    ]

    def test_a_requested_sleep_is_not_a_completed_one(self) -> None:
        warnings = bench.evaluate_suite_signals(self.REQUESTED_ONLY)
        self.assertTrue(any("no completed sleep captured" in w for w in warnings), warnings)

    def test_a_strict_report_fails_on_a_sleep_that_never_completed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(event) for event in self.REQUESTED_ONLY) + "\n",
                encoding="utf-8",
            )
            with (
                patch("builtins.print"),
                patch.object(
                    bench,
                    "load_budgets",
                    return_value=({"sleep-sync": {"full_refresh_busy_min_ms": 3000}}, None),
                ),
            ):
                warnings = bench.summarize_paths(
                    [path], bench.DEFAULT_BUDGETS, validate_suites=True
                )
        # The specific warning, not merely a non-empty list: any other budget
        # or coverage complaint would otherwise pass this test for the wrong
        # reason and leave the sleep gate itself uncovered end to end.
        self.assertTrue(
            any("no completed sleep captured" in w for w in warnings),
            f"strict report passed a sleep that never completed: {warnings}",
        )

    def test_a_completed_sleep_satisfies_the_check(self) -> None:
        events = self.REQUESTED_ONLY + [
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 44000},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertFalse(any("completed sleep" in w for w in warnings), warnings)

    def test_the_x4_pre_command_marker_is_not_a_completed_sleep(self) -> None:
        """`display: sleep deep` is printed *before* the deep-sleep command.

        ssd1677 prints it once the power-down handshake returns, and the
        command after it can still fail. Accepting it let a capture truncated
        at that line pass `--strict`, and — since the parsed event carries no
        `t_ms` — latched a boot segment that a later `complete ok=false`
        could not clear, so a reset was filed as a wake.
        """
        parsed = bench.parse_line("display: sleep deep", "sleep-sync")[0]
        self.assertFalse(bench.is_terminal_sleep(parsed))
        warnings = bench.evaluate_suite_signals(self.REQUESTED_ONLY + [parsed])
        self.assertTrue(any("no completed sleep captured" in w for w in warnings), warnings)

    def test_a_reset_after_an_x4_pre_command_marker_is_not_a_wake(self) -> None:
        run = [
            {"event": "sleep", "phase": "deep", "legacy": True},
            {"event": "sleep", "phase": "complete", "ok": False, "t_ms": 44000},
            {"event": "render", "view": "Reading", "t_ms": 60000},
            # Uptime restarts: an unexplained reboot, not a wake.
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "reset"])
        self.assertEqual(len(warnings), 1)

    def test_the_x3_panel_marker_counts_for_captures_without_complete(self) -> None:
        """`phase=deep_sleep` is printed after the deep-sleep command lands.

        It is the only terminal marker in logs from before `complete ok=true`
        was printed on the success path at all.
        """
        events = self.REQUESTED_ONLY + [
            {"event": "sleep", "phase": "deep_sleep", "elapsed_ms": 40, "t_ms": 44000},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertFalse(any("completed sleep" in w for w in warnings), warnings)

    def test_a_failed_completion_is_not_a_completed_sleep(self) -> None:
        events = self.REQUESTED_ONLY + [
            {"event": "sleep", "phase": "complete", "ok": False, "t_ms": 44000},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("no completed sleep captured" in w for w in warnings), warnings)
        self.assertTrue(any("failed sleep phase captured" in w for w in warnings), warnings)


class PageTurnTrustPoolTests(unittest.TestCase):
    """Trust is decided per capture, before the runs are added together."""

    # 1 turn and 1 press that produced nothing: 50% untrusted on its own.
    CADENCE_RUN: ClassVar[list[dict[str, Any]]] = [
        {"event": "run_start", "suite": "page-turn"},
        {"event": "input", "button": "Next", "t_ms": 1000},
        {"event": "input", "button": "Next", "t_ms": 1100},
        {"event": "render", "view": "Reading", "t_ms": 1010, "req_ms": 1000},
    ]

    @staticmethod
    def clean_run(turns: int, duration: int) -> list[dict]:
        events: list[dict] = [{"event": "run_start", "suite": "page-turn"}]
        for turn in range(turns):
            press = 1000 + turn * 10000
            events.append({"event": "input", "button": "Next", "t_ms": press})
            events.append(
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": press + duration,
                    "req_ms": press,
                }
            )
        return events

    def _pool(self, events: list[dict]) -> bench.PageTurnPool:
        return bench.page_turn_pool(bench.labelled_runs(events))

    def test_a_clean_sibling_does_not_launder_a_cadence_run(self) -> None:
        pool = self._pool(self.CADENCE_RUN + self.clean_run(20, 400))
        # Pooled, 1 unanswered press in 22 is under the 10% threshold, so the
        # merged population reads as trusted and its median is published.
        self.assertTrue(pool.every.median_trusted)
        self.assertEqual([label for label, _stats in pool.untrusted], ["page-turn run 1 of 2"])
        self.assertNotIn(10, pool.trusted.durations)
        self.assertEqual(pool.trusted.durations, [400] * 20)

    def test_the_printed_median_leaves_the_cadence_run_out(self) -> None:
        """The non-strict `report --all` path publishes this number."""
        events = self.CADENCE_RUN + self.clean_run(20, 400)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(event) for event in events) + "\n",
                encoding="utf-8",
            )
            with patch("builtins.print") as mock_print:
                bench.summarize_paths([path], None, latest_only=False)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("EXCLUDED page-turn run 1 of 2", printed)
        # A min of 10ms would be the cadence run's sample leaking in.
        self.assertIn("page turn      median=400ms p95=400ms min=400ms", printed)
        # The input accounting still covers every press, including that run's.
        self.assertIn("presses=22", printed)

    def test_the_budget_gate_leaves_the_cadence_run_out(self) -> None:
        events = self.CADENCE_RUN + self.clean_run(20, 400)
        # The 10 ms sample is what an untrusted run contributes; the budget
        # floor exists to catch exactly that.
        warnings = bench.evaluate_budgets(
            events,
            {
                "page-turn": {
                    "median_press_to_settled_ms": 550,
                    "median_press_to_settled_min_ms": 250,
                }
            },
        )
        self.assertTrue(any("excludes page-turn run 1 of 2" in w for w in warnings), warnings)
        self.assertFalse(any("below plausibility floor" in w for w in warnings), warnings)

    def test_a_run_that_paired_nothing_is_named_too(self) -> None:
        """Presses but no answering render: excluded, so it must be reported.

        `trusted` drops it either way; reporting only the cadence case meant a
        pooled report printed the healthy run's median with no sign that a
        whole capture had produced nothing.
        """
        stranded = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "input", "button": "Next", "t_ms": 2000},
        ]
        events = stranded + self.clean_run(20, 400)
        pool = self._pool(events)
        self.assertEqual([label for label, _stats in pool.untrusted], ["page-turn run 1 of 2"])
        self.assertEqual(pool.trusted.durations, [400] * 20)

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(event) for event in events) + "\n",
                encoding="utf-8",
            )
            with patch("builtins.print") as mock_print:
                bench.summarize_paths([path], None, latest_only=False)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn(
            "EXCLUDED page-turn run 1 of 2: 2 presses and no input-to-Reading-render sample",
            printed,
        )

    def test_a_run_with_no_presses_at_all_is_not_reported(self) -> None:
        """Nothing was attempted in it, so there is nothing to report."""
        idle = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "refresh", "mode": "Fast", "busy_ms": 410},
        ]
        pool = self._pool(idle + self.clean_run(3, 400))
        self.assertEqual(pool.untrusted, [])

    def test_every_run_untrusted_suppresses_the_budget_check(self) -> None:
        warnings = bench.evaluate_budgets(
            self.CADENCE_RUN,
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("budget not checked" in w for w in warnings), warnings)

    def test_a_pool_of_trusted_runs_is_still_pooled(self) -> None:
        events = self.clean_run(3, 400) + self.clean_run(3, 2000)
        pool = self._pool(events)
        self.assertEqual(pool.untrusted, [])
        self.assertEqual(sorted(pool.trusted.durations), [400] * 3 + [2000] * 3)


class UnknownWorkflowTests(unittest.TestCase):
    """Malformed workflow metadata must fail closed, not quietly pass."""

    MISSPELLED: ClassVar[list[dict[str, Any]]] = [
        {"event": "run_start", "suite": "sleep-sync", "workflow": "sleep_sync"},
        {"event": "refresh", "mode": "Full", "busy_ms": 3500},
    ]

    def test_an_unknown_workflow_has_no_budget_section_and_says_so(self) -> None:
        warnings = bench.evaluate_budgets(
            self.MISSPELLED,
            {"sleep-sync": {"full_refresh_busy_min_ms": 3000}},
        )
        self.assertTrue(
            any(
                "sleep_sync has no budget section" in w
                and "not a workflow this bench.py knows" in w
                for w in warnings
            ),
            warnings,
        )

    def test_an_unknown_workflow_has_no_signal_requirements_and_says_so(self) -> None:
        warnings = bench.evaluate_suite_signals(self.MISSPELLED)
        self.assertTrue(any("unrecognised workflow" in w for w in warnings), warnings)

    def test_an_unlabelled_run_says_nothing_was_checked(self) -> None:
        warnings = bench.evaluate_suite_signals(
            [{"event": "refresh", "mode": "Full", "busy_ms": 3500}]
        )
        self.assertTrue(any("no suite label" in w for w in warnings), warnings)


class PooledFileTests(unittest.TestCase):
    """`report a.jsonl b.jsonl` concatenates streams; runs must not merge.

    A legacy log opens with telemetry rather than a `run_start`, so it had no
    boundary of its own and joined whichever run preceded it. Whether its
    samples contaminated a labelled suite's budgets or were dropped from every
    section depended only on the order of the paths, and neither outcome was
    reported.
    """

    def _write(self, directory: str, name: str, events: list[dict]) -> Path:
        path = Path(directory) / name
        path.write_text("\n".join(json.dumps(event) for event in events) + "\n", encoding="utf-8")
        return path

    LABELLED: ClassVar[list[dict[str, Any]]] = [
        {"suite": "page-turn", "event": "run_start"},
        {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 1000},
        {
            "suite": "page-turn",
            "event": "render",
            "view": "Reading",
            "t_ms": 1400,
            "req_ms": 1000,
        },
    ]
    # Turns four times the budget: pooled into the labelled run they take its
    # median from 400 ms to 4000 ms.
    LEGACY: ClassVar[list[dict[str, Any]]] = [
        {"event": "input", "button": "Next", "t_ms": 1000},
        {"event": "render", "view": "Reading", "t_ms": 5000, "req_ms": 1000},
        {"event": "input", "button": "Next", "t_ms": 6000},
        {"event": "render", "view": "Reading", "t_ms": 11000, "req_ms": 6000},
    ]

    # Supplied directly rather than read from benches.toml: this test is
    # about pooling, and pinning the threshold here keeps it from moving when
    # the shipped budget does.
    BUDGETS = ({"page-turn": {"median_press_to_settled_ms": 550}}, None)

    def _warnings(self, order: list[str]) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            self._write(tmp, "labelled.jsonl", self.LABELLED)
            self._write(tmp, "legacy.jsonl", self.LEGACY)
            with (
                patch("builtins.print"),
                patch.object(bench, "load_budgets", return_value=self.BUDGETS),
            ):
                return bench.summarize_paths(
                    [Path(tmp) / name for name in order],
                    bench.DEFAULT_BUDGETS,
                    validate_suites=True,
                    latest_only=False,
                )

    def test_the_verdict_does_not_depend_on_path_order(self) -> None:
        # A run's position in the pooled stream legitimately changes with the
        # order, and warnings name it so an operator can find the capture.
        # Everything else about the verdict must be identical.
        def verdict(order: list[str]) -> list[str]:
            return sorted(
                re.sub(r" run \d+ of \d+", "", warning) for warning in self._warnings(order)
            )

        self.assertEqual(
            verdict(["labelled.jsonl", "legacy.jsonl"]),
            verdict(["legacy.jsonl", "labelled.jsonl"]),
        )

    def test_legacy_samples_do_not_contaminate_a_labelled_budget(self) -> None:
        for order in (
            ["labelled.jsonl", "legacy.jsonl"],
            ["legacy.jsonl", "labelled.jsonl"],
        ):
            with self.subTest(order=order):
                warnings = self._warnings(order)
                self.assertFalse(any("page-turn median" in w for w in warnings), warnings)

    def test_an_unlabelled_run_in_a_pool_is_reported(self) -> None:
        warnings = self._warnings(["labelled.jsonl", "legacy.jsonl"])
        self.assertTrue(any("carry no suite label" in w for w in warnings), warnings)

    def test_a_lone_legacy_log_is_still_measured(self) -> None:
        """Nothing to be ambiguous about, so the old behaviour stands."""
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "legacy.jsonl", self.LEGACY)
            with (
                patch("builtins.print"),
                patch.object(bench, "load_budgets", return_value=self.BUDGETS),
            ):
                warnings = bench.summarize_paths([path], bench.DEFAULT_BUDGETS, latest_only=False)
        self.assertTrue(any("page-turn median" in w for w in warnings), warnings)
        self.assertFalse(any("carry no suite label" in w for w in warnings), warnings)


class CaptureWorkflowTests(unittest.TestCase):
    """`run_start` has to carry the workflow for anything downstream to use it."""

    def test_thermal_run_records_its_underlying_workflow(self) -> None:
        args = argparse.Namespace(suite="sleep-sync")
        self.assertEqual(bench.capture_workflow(args, bench.SUITES["thermal-run"]), "sleep-sync")

    def test_every_other_suite_is_its_own_workflow(self) -> None:
        args = argparse.Namespace()
        for name, suite in bench.SUITES.items():
            with self.subTest(suite=name):
                self.assertEqual(bench.capture_workflow(args, suite), name)


class StorageBuildReportTests(unittest.TestCase):
    @patch("builtins.print")
    def test_report_summarizes_build_telemetry(self, mock_print) -> None:
        events = [
            {
                "suite": "storage-cache",
                "event": "storage_build",
                "elapsed_ms": 14948,
                "spine_ms": 13871,
                "write_ms": 4340,
                "sections": 51,
                "pages": 441,
                "rd_calls": 3026,
                "rd_blocks": 3026,
                "wr_calls": 2000,
                "wr_blocks": 2000,
                "key": "E3C2056B",
            },
            {
                "suite": "storage-cache",
                "event": "storage_build",
                "elapsed_ms": 14147,
                "spine_ms": 13000,
                "write_ms": 4200,
                "sections": 51,
                "pages": 441,
                "rd_calls": 3017,
                "rd_blocks": 3017,
                "wr_calls": 1990,
                "wr_blocks": 1990,
                "key": "E3C2056B",
            },
            {
                "suite": "storage-cache",
                "event": "storage_first_page",
                "elapsed_ms": 900,
                "pages": 12,
                "sections": 1,
                "key": "E3C2056B",
            },
            {
                "suite": "storage-cache",
                "event": "storage_background_build",
                "book_id": 5,
                "pages": 441,
                "elapsed_ms": 60000,
            },
        ]
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(event) for event in events) + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("storage build  median=14548ms", printed)
        self.assertIn("build spine    median=13436ms", printed)
        self.assertIn("build write    median=4270ms", printed)
        self.assertIn(
            "build io:      builds=2 rd_calls=6043 rd_blocks=6043 wr_calls=3990 wr_blocks=3990",
            printed,
        )
        self.assertIn("first page     median=900ms", printed)
        self.assertIn("bg build       median=60000ms", printed)


class BootSegmentTests(unittest.TestCase):
    def test_unexpected_regression_warns(self) -> None:
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "reset"])
        self.assertEqual(len(warnings), 1)
        self.assertIn("t_ms went backwards", warnings[0])

    def test_wake_after_deep_sleep_does_not_warn(self) -> None:
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 61000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "wake"])
        self.assertEqual(warnings, [])

    def test_boot_marker_splits_without_warning(self) -> None:
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "boot", "deep_sleep_wake": True, "gpio": True, "sleep_image": True},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "wake"])
        self.assertEqual(warnings, [])

    def test_failed_panel_sleep_is_not_a_wake(self) -> None:
        """`phase=complete` prints on both outcomes, carrying the result in `ok`.

        When the panel handshake fails the power task deliberately stays
        awake and retries, so a reboot after it is a genuine unexplained
        reset. Latching on the sleep line alone relabelled it a wake and
        suppressed the warning — in `reader-soak` and `sleep-sync`, where
        sleeps are frequent, that suppressed the check across most of a run.
        """
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "sleep", "phase": "complete", "ok": False, "t_ms": 61000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "reset"])
        self.assertEqual(len(warnings), 1)

    def test_activity_after_a_terminal_sleep_clears_the_latch(self) -> None:
        """A sleep that reported success but was followed by more work."""
        run = [
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 61000},
            # The device kept running, so it never deep-slept.
            {"event": "render", "view": "Reading", "t_ms": 80000},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "reset"])
        self.assertEqual(len(warnings), 1)

    def test_wake_without_a_retained_sleep_image_counts_as_cold(self) -> None:
        """A wake that lost its panel image pays the full waveform.

        Its first paint belongs with the cold cluster; pooling it with fast
        wakes hides exactly the case the wake path exists to avoid.
        """
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "boot", "deep_sleep_wake": False, "gpio": True, "sleep_image": False},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, _ = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "cold"])

    def test_a_marker_without_the_combined_field_uses_its_two_halves(self) -> None:
        """`deep_sleep_wake` is `gpio && sleep_image`; older markers say only
        the halves, and the same rule has to be spelled out from them."""
        cases = [
            ({"gpio": True, "sleep_image": True}, "wake"),
            # Absent rather than false: nothing contradicts the wake pin.
            ({"gpio": True}, "wake"),
            ({"gpio": True, "sleep_image": False}, "cold"),
            ({"gpio": False, "sleep_image": True}, "cold"),
        ]
        for fields, expected in cases:
            with self.subTest(fields=fields):
                run = [
                    {"event": "render", "view": "Reading", "t_ms": 60000},
                    dict({"event": "boot"}, **fields),
                    {"event": "render", "view": "Home", "t_ms": 3200},
                ]
                segments, _ = bench.boot_segments(run)
                self.assertEqual([kind for _, kind in segments], ["attach", expected])

    def test_a_marker_with_no_fields_falls_back_to_the_sleep_latch(self) -> None:
        """Nothing in the line to go on, so a preceding sleep is the evidence."""
        after_sleep = [
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
            {"event": "boot"},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, _ = bench.boot_segments(after_sleep)
        self.assertEqual([kind for _, kind in segments], ["attach", "wake"])

        without_sleep = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "boot"},
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, _ = bench.boot_segments(without_sleep)
        self.assertEqual([kind for _, kind in segments], ["attach", "cold"])

    def test_a_reset_after_a_completed_sleep_is_not_a_wake(self) -> None:
        """The boot marker's own verdict outranks the preceding sleep.

        A device that browns out or resets instead of waking through the
        Power button prints `deep_sleep_wake=false` (no GPIO wake cause),
        and it pays the full cold waveform. Reading the earlier successful
        sleep as proof of a wake filed that boot in the fast cluster and
        pulled its median up with a cold sample.
        """
        run = [
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
            {
                "event": "boot",
                "deep_sleep_wake": False,
                "gpio": False,
                "sleep_image": True,
            },
            {"event": "render", "view": "Home", "t_ms": 3200},
        ]
        segments, _ = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach", "cold"])

    def test_small_t_ms_inversion_is_skew_not_a_reboot(self) -> None:
        """Interrupt-priority input can print out of order by a hair.

        A real reboot restarts the uptime clock, so its regression is the
        whole session. Without a guard a 1 ms inversion fabricated a boot,
        a boot-to-first-paint sample, and a --strict failure.
        """
        run = [
            {"event": "render", "view": "Reading", "t_ms": 60000},
            {"event": "input", "button": "Next", "t_ms": 59999},
            {"event": "render", "view": "Reading", "t_ms": 60500},
        ]
        segments, warnings = bench.boot_segments(run)
        self.assertEqual([kind for _, kind in segments], ["attach"])
        self.assertEqual(warnings, [])

    def test_boot_paints_are_reported_per_kind(self) -> None:
        """A cold boot and a wake must never share a median."""
        run = [
            {"event": "boot", "deep_sleep_wake": False, "gpio": False, "sleep_image": False},
            {"event": "render", "view": "Home", "t_ms": 3310},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
            {"event": "render", "view": "Reading", "t_ms": 690},
        ]
        paints, _, _ = bench.boot_report([run])
        self.assertEqual(paints, {"cold": [3310], "wake": [690]})

    def test_sd_session_enter_is_not_a_boot_stage(self) -> None:
        """It fires several times before first paint, so its median is noise."""
        self.assertIsNone(bench.BOOT_STAGE_RE.search("sd: session enter t_ms=640"))
        self.assertIsNotNone(bench.BOOT_STAGE_RE.search("display: started t_ms=42"))

    def test_boot_report_reads_first_paint_from_witnessed_boots(self) -> None:
        cold_run = [
            {"event": "run_start", "suite": "storage-cache", "reset_before": True},
            {"event": "storage_catalog", "action": "load", "elapsed_ms": 30, "t_ms": 1100},
            {"event": "render", "view": "Home", "t_ms": 3200},
            {"event": "render", "view": "Library", "t_ms": 9000},
        ]
        attach_run = [
            {"event": "run_start", "suite": "page-turn", "reset_before": False},
            {"event": "render", "view": "Reading", "t_ms": 500000},
        ]
        paints, stages, warnings = bench.boot_report([cold_run, attach_run])
        self.assertEqual(paints, {"cold": [3200]})
        self.assertEqual(stages, {})
        self.assertEqual(warnings, [])

    def test_boot_report_collects_stages_before_first_paint(self) -> None:
        run = [
            {"event": "run_start", "suite": "sleep-sync", "reset_before": True},
            {"event": "boot_stage", "stage": "main: spawn display", "t_ms": 140},
            {"event": "boot_stage", "stage": "display: started", "t_ms": 150},
            {"event": "render", "view": "Home", "t_ms": 3200},
            # Per-session line after the paint must not count as boot stage.
            {"event": "boot_stage", "stage": "sd: session enter", "t_ms": 9000},
        ]
        paints, stages, _ = bench.boot_report([run])
        self.assertEqual(paints, {"cold": [3200]})
        self.assertEqual(
            stages,
            {"main: spawn display": [140], "display: started": [150]},
        )


class SplitRunsTests(unittest.TestCase):
    def test_markerless_input(self) -> None:
        events = [{"event": "render"}, {"event": "input"}]
        self.assertEqual(bench.split_runs(events), [events])

    def test_multiple_run_start_segments(self) -> None:
        e1 = {"event": "run_start", "id": 1}
        e2 = {"event": "render"}
        e3 = {"event": "run_start", "id": 2}
        e4 = {"event": "input"}
        self.assertEqual(
            bench.split_runs([e1, e2, e3, e4]),
            [[e1, e2], [e3, e4]],
        )

    def test_pre_marker_events(self) -> None:
        e1 = {"event": "render"}
        e2 = {"event": "run_start"}
        e3 = {"event": "input"}
        self.assertEqual(
            bench.split_runs([e1, e2, e3]),
            [[e1], [e2, e3]],
        )

    def test_empty_latest_run(self) -> None:
        e1 = {"event": "run_start"}
        e2 = {"event": "render"}
        e3 = {"event": "run_start"}
        self.assertEqual(
            bench.split_runs([e1, e2, e3]),
            [[e1, e2], [e3]],
        )
        self.assertEqual(bench.split_runs([]), [])


class CaptureLinesTests(unittest.TestCase):
    @patch("bench.serial_lines")
    def test_initial_oserror_propagates(self, mock_serial) -> None:
        import errno

        def gen():
            raise OSError(errno.ENOENT, "Not found")
            yield

        mock_serial.return_value = gen()
        with self.assertRaises(OSError):
            list(bench.capture_lines("/dev/port"))

    @patch("builtins.print")
    @patch("bench.time.sleep")
    @patch("bench.os.path.exists")
    @patch("bench.serial_lines")
    def test_reconnects_after_oserror(
        self, mock_serial, mock_exists, mock_sleep, mock_print
    ) -> None:
        import errno
        from unittest.mock import call

        def gen1():
            yield ""
            yield "data\n"
            raise OSError(errno.ENODEV, "Vanished")

        def gen2():
            yield ""
            yield "more data\n"

        mock_serial.side_effect = [gen1(), gen2()]
        mock_exists.side_effect = [False, True]

        lines = list(bench.capture_lines("/dev/port"))

        self.assertEqual(lines, ["data\n", "more data\n"])
        mock_sleep.assert_has_calls([call(0.5), call(0.5)])
        mock_print.assert_any_call(
            "port: /dev/port vanished (device asleep?); wake it to resume capture", flush=True
        )
        mock_print.assert_any_call("port: back; resuming capture", flush=True)

    @patch("builtins.print")
    @patch("bench.time.sleep")
    @patch("bench.time.monotonic")
    @patch("bench.os.path.exists")
    @patch("bench.serial_lines")
    def test_stop_at_expiration_while_absent(
        self, mock_serial, mock_exists, mock_monotonic, mock_sleep, mock_print
    ) -> None:
        import errno

        def gen():
            yield ""
            yield "data\n"
            raise OSError(errno.ENODEV, "Vanished")

        mock_serial.return_value = gen()
        mock_exists.return_value = False
        mock_monotonic.return_value = 100.0

        lines = list(bench.capture_lines("/dev/port", stop_at=50.0))

        self.assertEqual(lines, ["data\n"])
        mock_print.assert_any_call(
            "port: capture window ended while the device was away", flush=True
        )
        mock_sleep.assert_not_called()


class PageTurnCounterTests(unittest.TestCase):
    """The capture stops on turns, so an unprompted repaint is not one.

    Counting Reading renders let the boot paint, or any storage-driven
    repaint, consume one of the requested samples: `--turns 50` came home
    with 49 and nothing downstream could tell, because the report pairs
    properly and simply reported what it found.
    """

    PRESS_AND_TURN: ClassVar[list[str]] = [
        "bench: input button=Some(Next) aux=0 nav=0 page_raw=1 t_ms=1000\n",
        (
            "bench: render view=Reading mode=Fast page=2 chapter=1 layout_ms=10 "
            "flush_ms=400 req_ms=1000 prestage_ms=15 t_ms=1430\n"
        ),
    ]
    # No press before it: the paint a boot or a storage re-render produces.
    UNPROMPTED_RENDER = (
        "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 "
        "flush_ms=400 req_ms=100 prestage_ms=15 t_ms=530\n"
    )

    def _counts(self, lines: list[str], target: int) -> dict:
        return bench.process_capture_stream(
            lines,
            "page-turn",
            stop_target=("page_turn", target),
            print_lines=False,
        )

    def test_an_unprompted_render_does_not_consume_a_requested_turn(self) -> None:
        lines = [self.UNPROMPTED_RENDER] + self.PRESS_AND_TURN * 2
        counts = self._counts(lines, 2)
        self.assertEqual(counts.get("page_turn"), 2)
        self.assertEqual(counts.get("reading_render"), 3)

    def test_the_capture_runs_until_the_turns_are_paired(self) -> None:
        """Two turns requested, one repaint in the middle: still two turns."""
        lines = self.PRESS_AND_TURN + [self.UNPROMPTED_RENDER] + self.PRESS_AND_TURN
        counts = self._counts(lines, 2)
        self.assertEqual(counts.get("page_turn"), 2)

    @staticmethod
    def _turns(events: list) -> tuple:
        counter = bench.PageTurnCounter()
        for event in events:
            counter.observe(event)
        return counter.turns, len(bench.page_turn_stats_over_epochs(events).durations)

    @staticmethod
    def _reading(t_ms: int) -> list:
        return [
            {"event": "input", "button": "Next", "t_ms": t_ms},
            {"event": "render", "view": "Reading", "t_ms": t_ms + 470, "req_ms": t_ms},
        ]

    # The operator's last tap before the device sleeps, which no render
    # answers. `t_ms` restarts on the far side of the wake, so it sits at the
    # head of the pending FIFO with a timestamp nothing after it can reach.
    STRANDED_PRESS: ClassVar[dict[str, Any]] = {
        "event": "input",
        "button": "Next",
        "t_ms": 59000,
    }
    WAKE: ClassVar[dict[str, Any]] = {
        "event": "boot",
        "deep_sleep_wake": True,
        "gpio": True,
        "sleep_image": True,
    }

    def test_a_press_stranded_by_a_sleep_does_not_stop_the_count(self) -> None:
        """One unanswered press used to halt the counter for the whole run.

        Every render after the wake compared against a `t_ms` from before it,
        popped nothing, and counted no turn -- so `--turns N` never reached N
        and the capture ran to its deadline, while the report, which splits by
        boot segment, showed the turns that did happen.
        """
        events = (
            self._reading(1000)
            + self._reading(4000)
            + [self.STRANDED_PRESS, self.WAKE]
            + self._reading(600)
            + self._reading(3600)
        )
        self.assertEqual(self._turns(events), (4, 4))

    def test_a_bare_uptime_regression_ends_the_epoch_too(self) -> None:
        """A watchdog or brownout reset prints no boot marker."""
        events = (
            self._reading(1000) + [self.STRANDED_PRESS] + self._reading(600) + self._reading(3600)
        )
        self.assertEqual(self._turns(events), (3, 3))

    def test_a_millisecond_inversion_is_not_a_reboot(self) -> None:
        """`bench: input` can preempt the display task mid-print.

        The counter uses the same skew guard as `boot_segments`, so a hair of
        print inversion must not drop the pending press behind it.
        """
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1470, "req_ms": 1000},
            {"event": "input", "button": "Next", "t_ms": 1469},
            {"event": "render", "view": "Reading", "t_ms": 2000, "req_ms": 1600},
        ]
        self.assertEqual(self._turns(events), (2, 2))

    def test_the_live_counter_agrees_with_the_report(self) -> None:
        """The stop rule and the reported figure must not drift apart."""
        lines = (
            [self.UNPROMPTED_RENDER]
            + self.PRESS_AND_TURN * 3
            + [
                # A press answered by a Home render is navigation, not a turn.
                "bench: input button=Some(Next) aux=0 nav=0 page_raw=1 t_ms=9000\n",
                (
                    "bench: render view=Home mode=Fast page=0 chapter=0 layout_ms=10 "
                    "flush_ms=400 req_ms=9000 t_ms=9430\n"
                ),
            ]
        )
        events = [event for line in lines for event in bench.parse_line(line, "page-turn")]
        counter = bench.PageTurnCounter()
        for event in events:
            counter.observe(event)
        self.assertEqual(counter.turns, len(bench.page_turn_stats_over_epochs(events).durations))

    def test_a_short_capture_is_reported_against_what_was_asked_for(self) -> None:
        events = [
            {"event": "run_start", "suite": "page-turn", "requested_page_turns": 50},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1430, "req_ms": 1000},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("1 of 50 requested page turns" in w for w in warnings), warnings)

    def test_a_complete_capture_is_not_faulted(self) -> None:
        events: list[dict] = [
            {"event": "run_start", "suite": "page-turn", "requested_page_turns": 2},
        ]
        for turn in range(2):
            press = 1000 + turn * 5000
            events.append({"event": "input", "button": "Next", "t_ms": press})
            events.append(
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": press + 430,
                    "req_ms": press,
                }
            )
        self.assertEqual(bench.evaluate_suite_signals(events), [])


class ReaderSoakSignalTests(unittest.TestCase):
    """The soak's guidance promises a sleep/wake cycle; strict must ask for it."""

    @staticmethod
    def _reading(t_ms: int) -> list[dict]:
        return [
            {"event": "input", "button": "Next", "t_ms": t_ms},
            {"event": "render", "view": "Reading", "t_ms": t_ms + 430, "req_ms": t_ms},
        ]

    def test_a_soak_that_never_slept_is_reported(self) -> None:
        events = [{"event": "run_start", "suite": "reader-soak"}] + self._reading(1000)
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("no completed sleep captured" in w for w in warnings), warnings)

    def test_a_soak_that_slept_but_never_woke_is_reported(self) -> None:
        events = (
            [{"event": "run_start", "suite": "reader-soak"}]
            + self._reading(1000)
            + [{"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000}]
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("no wake followed it" in w for w in warnings), warnings)

    def test_a_full_sleep_wake_cycle_passes(self) -> None:
        events = (
            [{"event": "run_start", "suite": "reader-soak"}]
            + self._reading(1000)
            + [
                {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
                {
                    "event": "boot",
                    "deep_sleep_wake": True,
                    "gpio": True,
                    "sleep_image": True,
                },
            ]
            + self._reading(600)
        )
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_wake_before_the_sleep_does_not_satisfy_the_cycle(self) -> None:
        """Started while asleep, woken to begin, and never woken again."""
        events = (
            [
                {"event": "run_start", "suite": "reader-soak"},
                {
                    "event": "boot",
                    "deep_sleep_wake": True,
                    "gpio": True,
                    "sleep_image": True,
                },
            ]
            + self._reading(1000)
            + [{"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000}]
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("no wake followed it" in w for w in warnings), warnings)

    def test_a_failed_sleep_phase_is_reported_despite_a_later_cycle(self) -> None:
        events = (
            [{"event": "run_start", "suite": "reader-soak"}]
            + self._reading(1000)
            + [
                {"event": "sleep", "phase": "refresh", "ok": False, "t_ms": 20000},
                {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
                {
                    "event": "boot",
                    "deep_sleep_wake": True,
                    "gpio": True,
                    "sleep_image": True,
                },
            ]
            + self._reading(600)
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertEqual(
            [w for w in warnings if "failed sleep phase captured" in w],
            ["reader-soak: failed sleep phase captured"],
        )


class WokeAfterSleepTests(unittest.TestCase):
    """`woke_after_sleep` asks about order, not about presence."""

    WAKE_BOOT: ClassVar[dict[str, Any]] = {
        "event": "boot",
        "deep_sleep_wake": True,
        "gpio": True,
        "sleep_image": True,
    }

    def test_sleep_then_wake_is_a_cycle(self) -> None:
        run = [
            {"event": "render", "view": "Reading", "t_ms": 1000},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 2000},
            self.WAKE_BOOT,
            {"event": "render", "view": "Reading", "t_ms": 300},
        ]
        self.assertTrue(bench.woke_after_sleep(run))

    def test_wake_then_sleep_is_not(self) -> None:
        run = [
            self.WAKE_BOOT,
            {"event": "render", "view": "Reading", "t_ms": 300},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 2000},
        ]
        self.assertFalse(bench.woke_after_sleep(run))

    def test_a_cold_reboot_after_a_sleep_is_not_a_wake(self) -> None:
        run = [
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 2000},
            {"event": "boot", "deep_sleep_wake": False, "gpio": False},
            {"event": "render", "view": "Reading", "t_ms": 300},
        ]
        self.assertFalse(bench.woke_after_sleep(run))

    def test_a_second_sleep_wake_pair_still_counts(self) -> None:
        run = [
            self.WAKE_BOOT,
            {"event": "render", "view": "Reading", "t_ms": 300},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 2000},
            self.WAKE_BOOT,
            {"event": "render", "view": "Reading", "t_ms": 300},
        ]
        self.assertTrue(bench.woke_after_sleep(run))


class HasFailedSleepTests(unittest.TestCase):
    def test_any_failed_phase_counts(self) -> None:
        self.assertTrue(
            bench.has_failed_sleep([{"event": "sleep", "phase": "power_down_start", "ok": False}])
        )

    def test_a_successful_run_has_none(self) -> None:
        self.assertFalse(
            bench.has_failed_sleep(
                [
                    {"event": "sleep", "phase": "requested"},
                    {"event": "sleep", "phase": "complete", "ok": True},
                    {"event": "render", "ok": False},
                ]
            )
        )


class BenchCaptureLoopTests(unittest.TestCase):
    def test_capture_waits_for_paired_prestage_across_intervening_log(self) -> None:
        """Capture continues past unrelated logs until event=='prestage' arrives."""
        lines = [
            "bench: input button=Some(Next) aux=0 nav=0 page_raw=1 t_ms=50\n",
            "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 req_ms=50 t_ms=500\n",
            "[LOG_INF] Unrelated firmware message\n",
            "bench: prestage staged=true elapsed_ms=24 t_ms=124\n",
            "bench: render view=Reading mode=Fast page=2 chapter=1 layout_ms=10 flush_ms=400 t_ms=200\n",
        ]
        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        counts = bench.process_capture_stream(
            lines,
            "page-turn",
            stop_target=("page_turn", 1),
            print_lines=False,
            event_callback=event_cb,
        )

        self.assertEqual(counts.get("page_turn"), 1)
        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage"), 1)
        self.assertEqual([event["event"] for event in written], ["input", "render", "prestage"])

    def test_capture_stops_immediately_for_structured_combined_render(self) -> None:
        """Structured combined render with prestage_ms stops without waiting for standalone prestage."""
        lines = [
            "bench: input button=Some(Next) aux=0 nav=0 page_raw=1 t_ms=50\n",
            "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 prestage_ms=15 req_ms=50 t_ms=500\n",
            "bench: prestage staged=true elapsed_ms=24 t_ms=124\n",
        ]
        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        counts = bench.process_capture_stream(
            lines,
            "page-turn",
            stop_target=("page_turn", 1),
            print_lines=False,
            event_callback=event_cb,
        )

        self.assertEqual(counts.get("page_turn"), 1)
        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage", 0), 0)
        self.assertEqual([event["event"] for event in written], ["input", "render"])

    def test_capture_bounded_fallback_when_prestage_missing(self) -> None:
        """Capture stops boundedly if prestage telemetry never arrives."""
        lines = [
            "bench: input button=Some(Next) aux=0 nav=0 page_raw=1 t_ms=50\n",
            "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 req_ms=50 t_ms=500\n",
        ] + [f"[LOG_INF] Intervening log {i}\n" for i in range(10)]
        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        counts = bench.process_capture_stream(
            lines,
            "page-turn",
            stop_target=("page_turn", 1),
            print_lines=False,
            event_callback=event_cb,
        )

        self.assertEqual(counts.get("page_turn"), 1)
        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage", 0), 0)
        self.assertEqual([event["event"] for event in written], ["input", "render"])

    def test_capture_silent_device_fallback_when_prestage_missing(self) -> None:
        """Capture stops when deadline expires even if serial stream is completely silent (no newlines)."""
        import time

        deadline_val: list[float] = []

        def silent_lines():
            yield "bench: input button=Some(Next) aux=0 nav=0 page_raw=1 t_ms=50\n"
            yield "bench: render view=Reading mode=Fast page=1 chapter=1 layout_ms=10 flush_ms=400 req_ms=50 t_ms=500\n"
            while True:
                time.sleep(0.01)
                if deadline_val and time.monotonic() >= deadline_val[0]:
                    return

        written: list[dict] = []

        def event_cb(e: dict) -> None:
            written.append(e)

        started = time.monotonic()
        counts = bench.process_capture_stream(
            silent_lines(),
            "page-turn",
            stop_target=("page_turn", 1),
            print_lines=False,
            event_callback=event_cb,
            pending_prestage_timeout_s=0.05,
            on_deadline_set=lambda d: deadline_val.append(d),
        )
        elapsed = time.monotonic() - started

        self.assertEqual(counts.get("page_turn"), 1)
        self.assertEqual(counts.get("reading_render"), 1)
        self.assertEqual(counts.get("prestage", 0), 0)
        self.assertEqual([event["event"] for event in written], ["input", "render"])
        self.assertLess(elapsed, 1.0)


class PositiveIntTests(unittest.TestCase):
    """Zero is a request for nothing, not a request for no limit."""

    def test_a_positive_value_passes_through(self) -> None:
        self.assertEqual(bench.positive_int("30"), 30)

    def test_zero_is_rejected(self) -> None:
        with self.assertRaises(argparse.ArgumentTypeError):
            bench.positive_int("0")

    def test_a_negative_value_is_rejected(self) -> None:
        with self.assertRaises(argparse.ArgumentTypeError):
            bench.positive_int("-5")

    def test_a_non_integer_is_rejected(self) -> None:
        with self.assertRaises(argparse.ArgumentTypeError):
            bench.positive_int("20s")

    def test_every_duration_and_count_flag_uses_it(self) -> None:
        """`--seconds 0` used to disable the deadline and run forever.

        The same hole reached `reader-soak --minutes 0`, `thermal-run
        --minutes 0`, `--turns 0` and `--cycles 0`, because the capture loop
        asked `if seconds:` and could not tell an explicit zero from an
        unspecified duration.
        """
        parser = argparse.ArgumentParser(prog="bench")
        sub = parser.add_subparsers(dest="command", required=True)
        for name in bench.SUITES:
            bench.add_capture_parser(sub, name)
        for argv in (
            ["page-turn", "--seconds", "0"],
            ["page-turn", "--turns", "0"],
            ["reader-soak", "--minutes", "0"],
            ["sleep-sync", "--cycles", "0"],
            ["thermal-run", "--minutes", "0"],
        ):
            with self.subTest(argv=argv), self.assertRaises(SystemExit), patch("sys.stderr"):
                parser.parse_args(argv)


class StopReasonTests(unittest.TestCase):
    def test_reaching_the_count_completes_the_capture(self) -> None:
        self.assertEqual(
            bench.observed_stop_reason({"page_turn": 50}, ("page_turn", 50), None),
            "count",
        )

    def test_an_expired_deadline_completes_the_capture(self) -> None:
        self.assertEqual(bench.observed_stop_reason({}, None, time.monotonic() - 1), "duration")

    def test_a_stream_that_simply_ended_did_not_complete(self) -> None:
        """The device stopped talking; nobody asked for that."""
        reason = bench.observed_stop_reason(
            {"page_turn": 3}, ("page_turn", 50), time.monotonic() + 600
        )
        self.assertEqual(reason, "stream-ended")
        self.assertNotIn(reason, bench.COMPLETED_STOP_REASONS)

    def test_an_expired_deadline_is_the_reason_even_when_the_count_is_short(
        self,
    ) -> None:
        """`--turns 50 --seconds 60` that ran out of time stopped on time.

        "duration" is a *completed* reason: the capture reached a stop
        condition it was given. Whether it collected enough is a separate
        question, answered by the request shortfall checks, not here.
        """
        self.assertEqual(
            bench.observed_stop_reason({"page_turn": 3}, ("page_turn", 50), time.monotonic() - 1),
            "duration",
        )


class CaptureContractTests(unittest.TestCase):
    """What the operator asked for is written down, and checked afterwards."""

    TURN: ClassVar[list[str]] = [
        "bench: input button=Some(Next) aux=0 nav=0 page_raw=1 t_ms=1000\n",
        (
            "bench: render view=Reading mode=Fast page=2 chapter=1 layout_ms=10 "
            "flush_ms=400 req_ms=1000 prestage_ms=15 t_ms=1430\n"
        ),
    ]

    @staticmethod
    def _capture(
        command: str,
        lines: list,
        *,
        interrupt: bool = False,
        reset_before: bool = False,
        on_reset=None,
        seen: dict[str, Any] | None = None,
        seconds: int | None = None,
        printed: list[str] | None = None,
        **extra,
    ):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "log.jsonl"
            args = argparse.Namespace(
                command=command,
                port="/dev/bench-test",
                out=out,
                seconds=seconds,
                reset_before=reset_before,
                espflash="espflash",
                strict=False,
                note=[],
                book=None,
                **extra,
            )

            def fake_capture_lines(port, stop_at=None, get_stop_at=None):
                if seen is not None:
                    seen["stop_at"] = stop_at
                if interrupt:
                    raise KeyboardInterrupt
                return iter(lines)

            def record(*parts, **_kwargs):
                if printed is not None:
                    printed.append(" ".join(str(part) for part in parts))

            patches = [
                patch.object(bench, "capture_lines", fake_capture_lines),
                patch.object(bench, "summarize_paths", return_value=[]),
                patch("builtins.print", record),
                # The capture echoes the serial stream with sys.stdout.write.
                patch("sys.stdout", io.StringIO()),
            ]
            if on_reset is not None:
                patches.append(patch.object(bench, "reset_device", on_reset))
            # An ExitStack rather than start()/stop() loops: a patch that
            # fails to apply half way through the list would otherwise leave
            # the ones before it permanently installed, and print or
            # summarize_paths staying patched poisons every later test.
            with contextlib.ExitStack() as stack:
                for entered in patches:
                    stack.enter_context(entered)
                bench.run_capture(args)
            return [
                json.loads(line)
                for line in out.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]

    @staticmethod
    def _marker(events: list, name: str) -> dict:
        return next(event for event in events if event["event"] == name)

    def test_every_suite_records_what_it_was_asked_for(self) -> None:
        """Only the page-turn count used to be written down."""
        cases = [
            ("page-turn", {"turns": 7}, {"page_turns": 7}),
            ("sleep-sync", {"cycles": 4}, {"sleep_cycles": 4}),
            ("reader-soak", {"minutes": 30}, {"seconds": 1800}),
            (
                "thermal-run",
                {"minutes": 45, "suite": "sleep-sync"},
                {"seconds": 2700},
            ),
            (
                "storage-cache",
                {"cold": True, "warm": True},
                {"storage_modes": ["cold", "warm"]},
            ),
        ]
        for command, extra, expected in cases:
            with self.subTest(command=command):
                events = self._capture(command, [], **extra)
                self.assertEqual(self._marker(events, "run_start")["requested"], expected)

    def test_an_unrequested_mode_is_not_recorded(self) -> None:
        events = self._capture("storage-cache", [], cold=True, warm=False)
        self.assertEqual(
            self._marker(events, "run_start")["requested"], {"storage_modes": ["cold"]}
        )

    def test_reaching_the_requested_count_records_a_complete_run(self) -> None:
        events = self._capture("page-turn", self.TURN, turns=1)
        end = self._marker(events, "run_end")
        self.assertEqual(end["stop_reason"], "count")
        self.assertTrue(end["completed"])

    def test_an_interrupted_capture_is_recorded_as_incomplete(self) -> None:
        """Ctrl-C used to write the same run_end a finished capture wrote."""
        events = self._capture("page-turn", [], turns=50, interrupt=True)
        end = self._marker(events, "run_end")
        self.assertEqual(end["stop_reason"], "interrupt")
        self.assertFalse(end["completed"])

    def test_ctrl_c_completes_a_capture_that_asked_for_no_stop_condition(self) -> None:
        """`storage-cache` with no --seconds stops on Ctrl-C by design."""
        events = self._capture("storage-cache", [], cold=False, warm=False, interrupt=True)
        end = self._marker(events, "run_end")
        self.assertEqual(end["stop_reason"], "operator")
        self.assertTrue(end["completed"])

    def test_the_capture_window_starts_after_the_reset(self) -> None:
        """`--reset-before --seconds 20` must collect twenty seconds.

        The deadline was set before the reset, so espflash and the device's
        re-enumeration ate part of the requested window: the capture collected
        materially less than it asked for while reporting about 20s elapsed.
        """
        seen: dict = {}

        def slow_reset(espflash, port):
            time.sleep(0.05)
            seen["reset_done"] = time.monotonic()

        events = self._capture(
            "storage-cache",
            [],
            cold=False,
            warm=False,
            reset_before=True,
            on_reset=slow_reset,
            seen=seen,
            seconds=20,
        )
        self.assertGreaterEqual(seen["stop_at"] - seen["reset_done"], 20 - 0.001)
        end = self._marker(events, "run_end")
        self.assertGreater(end["command_elapsed_s"], end["elapsed_s"])


class CaptureCompletionReportTests(unittest.TestCase):
    """`--strict` must not certify a capture that did not run its course."""

    @staticmethod
    def _start(suite: str, requested: dict) -> dict:
        return {
            "event": "run_start",
            "suite": suite,
            "workflow": suite,
            "host_time": 1785015654.8,
            "requested": requested,
        }

    @staticmethod
    def _end(**fields) -> dict:
        return {"event": "run_end", "elapsed_s": 1800.0, "completed": True, **fields}

    @staticmethod
    def _reading(t_ms: int) -> list:
        return [
            {"event": "input", "button": "Next", "t_ms": t_ms},
            {"event": "render", "view": "Reading", "t_ms": t_ms + 430, "req_ms": t_ms},
        ]

    def _soak(self, requested: dict, end: dict) -> list:
        return (
            [self._start("reader-soak", requested)]
            + self._reading(1000)
            + [
                {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
                {
                    "event": "boot",
                    "deep_sleep_wake": True,
                    "gpio": True,
                    "sleep_image": True,
                },
            ]
            + self._reading(600)
            + [end]
        )

    def test_a_complete_capture_is_not_faulted(self) -> None:
        events = self._soak({"seconds": 1800}, self._end())
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_truncated_log_is_reported(self) -> None:
        """run_start with no run_end: killed, or the file was cut."""
        events = self._soak({"seconds": 1800}, {"event": "boot_stage", "stage": "x"})
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("recorded no run_end" in w for w in warnings), warnings)

    def test_an_interrupted_soak_is_reported(self) -> None:
        events = self._soak(
            {"seconds": 1800},
            self._end(elapsed_s=42.0, completed=False, stop_reason="interrupt"),
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(
            any("did not complete (stopped by interrupt)" in w for w in warnings),
            warnings,
        )

    def test_a_soak_short_of_its_window_is_reported(self) -> None:
        """The minimum valid input/render/sleep/wake sequence is not 30 minutes."""
        events = self._soak({"seconds": 1800}, self._end(elapsed_s=95.0))
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("95s of the 1800s requested" in w for w in warnings), warnings)

    def test_a_capture_predating_the_contract_is_reported_not_assumed(self) -> None:
        """An old real capture cannot prove it finished; say so."""
        events = self._soak({}, {"event": "run_end", "elapsed_s": 1800.0})
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("recorded no completion status" in w for w in warnings), warnings)

    def test_a_hand_built_log_is_not_asked_for_a_run_end(self) -> None:
        """Fixtures and hand-assembled logs carry no host_time and no run_end."""
        events = (
            [{"event": "run_start", "suite": "reader-soak"}]
            + self._reading(1000)
            + [
                {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
                {
                    "event": "boot",
                    "deep_sleep_wake": True,
                    "gpio": True,
                    "sleep_image": True,
                },
            ]
            + self._reading(600)
        )
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_sleep_sync_run_short_of_its_cycles_is_reported(self) -> None:
        """Interrupted after one valid cycle, `--cycles 10` used to pass."""
        events = [
            self._start("sleep-sync", {"sleep_cycles": 10}),
            {"event": "refresh", "mode": "Full", "busy_ms": 3500},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
            self._end(elapsed_s=61.0),
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("1 of 10 requested sleep cycles" in w for w in warnings), warnings)

    def test_a_sleep_sync_run_that_completed_its_cycles_passes(self) -> None:
        events = [self._start("sleep-sync", {"sleep_cycles": 2})]
        for cycle in range(2):
            events.append(
                {
                    "event": "sleep",
                    "phase": "complete",
                    "ok": True,
                    "t_ms": 40000 + cycle * 1000,
                }
            )
        events.append(self._end())
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_failed_completion_is_not_a_cycle(self) -> None:
        """`phase=complete` prints on both outcomes and carries the result."""
        events = [
            self._start("sleep-sync", {"sleep_cycles": 1}),
            {"event": "sleep", "phase": "complete", "ok": False, "t_ms": 40000},
            self._end(),
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("0 of 1 requested sleep cycles" in w for w in warnings), warnings)

    def test_the_x3_panel_marker_does_not_double_count_a_cycle(self) -> None:
        """uc8253 prints phase=deep_sleep beside the display task's complete."""
        events = [
            {"event": "sleep", "phase": "deep_sleep", "elapsed_ms": 20, "t_ms": 39990},
            {"event": "sleep", "phase": "complete", "ok": True, "t_ms": 40000},
        ]
        self.assertEqual(bench.completed_sleep_cycles(events), 1)

    def test_the_live_stop_rule_and_the_report_count_the_same_cycles(self) -> None:
        lines = [
            "bench: sleep phase=requested screen_on=true t_ms=39000\n",
            "bench: sleep phase=deep_sleep elapsed_ms=20 t_ms=39990\n",
            "bench: sleep phase=complete ok=true elapsed_ms=30 t_ms=40000\n",
            "bench: sleep phase=complete ok=false elapsed_ms=30 t_ms=50000\n",
        ]
        counts = bench.process_capture_stream(
            lines, "sleep-sync", stop_target=None, print_lines=False
        )
        events = [e for line in lines for e in bench.parse_line(line, "sleep-sync")]
        self.assertEqual(counts.get("sleep_complete"), 1)
        self.assertEqual(bench.completed_sleep_cycles(events), 1)

    def test_a_failed_completion_does_not_end_the_capture(self) -> None:
        """The stop rule counted a failed sleep as a delivered cycle."""
        lines = [
            "bench: sleep phase=complete ok=false elapsed_ms=30 t_ms=40000\n",
            "bench: sleep phase=complete ok=true elapsed_ms=30 t_ms=50000\n",
        ]
        counts = bench.process_capture_stream(
            lines, "sleep-sync", stop_target=("sleep_complete", 1), print_lines=False
        )
        self.assertEqual(counts.get("sleep_complete"), 1)

    def test_a_page_turn_run_short_of_its_turns_is_still_reported(self) -> None:
        events = [
            self._start("page-turn", {"page_turns": 50}),
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1430, "req_ms": 1000},
            self._end(),
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("1 of 50 requested page turns" in w for w in warnings), warnings)


class StorageModeTests(unittest.TestCase):
    """`--cold` and `--warm` select a path, so the capture must show it."""

    @staticmethod
    def _start(modes: list) -> dict:
        return {
            "event": "run_start",
            "suite": "storage-cache",
            "workflow": "storage-cache",
            "host_time": 1785015654.8,
            "requested": {"storage_modes": modes},
        }

    END: ClassVar[dict[str, Any]] = {"event": "run_end", "elapsed_s": 20.0, "completed": True}

    WARM_OPEN: ClassVar[dict[str, Any]] = {
        "event": "storage_open",
        "ram_hit": False,
        "elapsed_ms": 72,
    }
    COLD_BUILD: ClassVar[dict[str, Any]] = {"event": "storage_build", "elapsed_ms": 14948}
    COLD_OPEN: ClassVar[dict[str, Any]] = {
        "event": "storage_open",
        "ram_hit": False,
        "elapsed_ms": 15034,
    }

    def test_a_warm_only_capture_fails_the_cold_request(self) -> None:
        events = [self._start(["cold", "warm"]), self.WARM_OPEN, self.END]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("--cold was requested" in w for w in warnings), warnings)
        self.assertFalse(any("--warm was requested" in w for w in warnings), warnings)

    def test_a_capture_showing_both_paths_passes(self) -> None:
        events = [
            self._start(["cold", "warm"]),
            self.COLD_BUILD,
            self.COLD_OPEN,
            self.WARM_OPEN,
            self.END,
        ]
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_successful_catalog_scan_is_cold_evidence(self) -> None:
        events = [
            self._start(["cold"]),
            {"event": "storage_catalog", "action": "scan", "ok": True, "elapsed_ms": 900},
            self.END,
        ]
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_catalog_load_is_warm_evidence(self) -> None:
        events = [
            self._start(["warm"]),
            {"event": "storage_catalog", "action": "load", "ok": True, "elapsed_ms": 31},
            self.END,
        ]
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_ram_hit_is_warm_evidence(self) -> None:
        events = [
            self._start(["warm"]),
            {"event": "storage_open", "ram_hit": True, "elapsed_ms": 0},
            self.END,
        ]
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_capture_with_no_storage_telemetry_fails_both(self) -> None:
        events = [
            self._start(["cold", "warm"]),
            {"event": "render", "view": "Reading", "t_ms": 500},
            self.END,
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("--cold was requested" in w for w in warnings), warnings)
        self.assertTrue(any("--warm was requested" in w for w in warnings), warnings)

    def test_an_unrecognised_mode_fails_closed(self) -> None:
        events = [self._start(["tepid"]), self.WARM_OPEN, self.END]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("unrecognised storage mode tepid" in w for w in warnings), warnings)


class StorageOpenPopulationTests(unittest.TestCase):
    """A RAM hit, a cache load and a cache build are three different things.

    Measured on this repo's own captures: 0-15 ms, 57-95 ms, and 14-64
    *seconds*. Pooling them produced a percentile describing none of them.
    """

    RUN: ClassVar[list[dict[str, Any]]] = [
        {"event": "storage_catalog", "action": "load", "elapsed_ms": 32},
        {"event": "storage_build", "elapsed_ms": 14948},
        # The legacy `storage: open complete` line, printed just before the
        # structured event and carrying no ram_hit or elapsed_ms.
        {"event": "storage_open", "status": "Ready", "pages": 441, "legacy": True},
        {"event": "storage_open", "ram_hit": False, "elapsed_ms": 15034},
        {"event": "storage_open", "ram_hit": True, "elapsed_ms": 15},
        {"event": "storage_open", "ram_hit": False, "elapsed_ms": 89},
    ]

    def test_opens_are_split_by_the_work_they_did(self) -> None:
        kinds = bench.storage_open_kinds(self.RUN)
        self.assertEqual(bench.values(kinds["cold"], "elapsed_ms"), [15034])
        self.assertEqual(bench.values(kinds["ram"], "elapsed_ms"), [15])
        self.assertEqual(bench.values(kinds["warm"], "elapsed_ms"), [89])

    def test_the_legacy_line_does_not_swallow_the_build(self) -> None:
        """It parses to a storage_open too, and precedes the structured one."""
        self.assertEqual(len(bench.storage_open_kinds(self.RUN)["cold"]), 1)

    def test_a_build_does_not_cross_a_run_boundary(self) -> None:
        """`--all` concatenates captures; a pending build must not follow."""
        events = [
            {"event": "storage_build", "elapsed_ms": 14948},
            {"event": "run_start", "suite": "storage-cache"},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 72},
        ]
        kinds = bench.storage_open_kinds(events)
        self.assertEqual(bench.values(kinds["warm"], "elapsed_ms"), [72])
        self.assertEqual(kinds["cold"], [])

    def test_a_build_does_not_cross_a_reboot(self) -> None:
        events = [
            {"event": "storage_build", "elapsed_ms": 14948},
            {"event": "boot", "deep_sleep_wake": False},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 72},
        ]
        self.assertEqual(bench.storage_open_kinds(events)["cold"], [])

    def test_an_open_with_no_ram_hit_belongs_to_no_population(self) -> None:
        kinds = bench.storage_open_kinds(self.RUN)
        self.assertEqual(sum(len(events) for events in kinds.values()), 3)

    def test_a_cold_build_does_not_fail_the_warm_budget(self) -> None:
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"}
        ] + self.RUN
        warnings = bench.evaluate_budgets(
            events, {"storage-cache": {"warm_book_open_warn_ms": 150}}
        )
        self.assertEqual(warnings, [])

    def test_a_slow_warm_open_still_fails_the_warm_budget(self) -> None:
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 900},
        ]
        warnings = bench.evaluate_budgets(
            events, {"storage-cache": {"warm_book_open_warn_ms": 150}}
        )
        self.assertTrue(any("warm book open p95 900ms above" in w for w in warnings), warnings)

    def test_a_capture_with_only_cold_opens_has_nothing_to_gate(self) -> None:
        """Fail closed: the budget covered no sample in this run."""
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"},
            {"event": "storage_build", "elapsed_ms": 14948},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 15034},
        ]
        warnings = bench.evaluate_budgets(
            events, {"storage-cache": {"warm_book_open_warn_ms": 150}}
        )
        self.assertTrue(any("no warm storage_open events" in w for w in warnings), warnings)

    # folder-nav budgets. Sized from the first device capture of the suite
    # (2026-09-09, X3, 20 unattended round trips: enter 37-87 ms, leave
    # 38-40 ms), so the fixtures below use those populations.
    FOLDER_BUDGETS: ClassVar[dict[str, dict[str, int]]] = {
        "folder-nav": {"folder_enter_warn_ms": 200, "folder_leave_warn_ms": 150}
    }
    FOLDER_START: ClassVar[dict[str, str]] = {
        "suite": "folder-nav",
        "workflow": "folder-nav",
        "event": "run_start",
    }

    def test_a_measured_folder_walk_passes_both_budgets(self) -> None:
        events = [self.FOLDER_START] + [
            {"event": "folder_enter", "rows": 14, "depth": 1, "ok": True, "ms": 87},
            {"event": "folder_leave", "rows": 20, "depth": 0, "ok": True, "ms": 39},
        ] * 5
        self.assertEqual(bench.evaluate_budgets(events, self.FOLDER_BUDGETS), [])

    def test_a_slow_folder_enter_fails_its_budget(self) -> None:
        events = [
            self.FOLDER_START,
            {"event": "folder_enter", "rows": 14, "depth": 1, "ok": True, "ms": 900},
            {"event": "folder_leave", "rows": 20, "depth": 0, "ok": True, "ms": 39},
        ]
        warnings = bench.evaluate_budgets(events, self.FOLDER_BUDGETS)
        self.assertTrue(any("folder enter p95 900ms above" in w for w in warnings), warnings)
        self.assertFalse(any("folder leave" in w for w in warnings), warnings)

    # A selftest scenario ends its own capture with `result=done`, and the
    # operator's --seconds is the ceiling the docs tell them to pass. Holding
    # that run to the window failed every storage-cache selftest on "185s of
    # the 500s requested" with `done` in the same log.
    def _selftest_run(self, stop_reason: str, result: str) -> list[dict[str, object]]:
        return [
            {
                "suite": "storage-cache",
                "workflow": "storage-cache",
                "event": "run_start",
                "requested": {"seconds": 500},
            },
            {"event": "render", "view": "Reading", "t_ms": 1000},
            {"event": "input", "button": "Next", "t_ms": 900},
            {
                "event": "selftest_done",
                "scenario": "storage-cache",
                "result": result,
            },
            {"event": "run_end", "stop_reason": stop_reason, "elapsed_s": 185.0},
        ]

    def test_a_scenario_that_finished_itself_owes_no_duration(self) -> None:
        warnings = bench.evaluate_suite_signals(self._selftest_run("selftest", "done"))
        self.assertFalse(any("of the 500s requested" in w for w in warnings), warnings)

    def test_a_clock_stop_still_owes_the_duration(self) -> None:
        """Same short elapsed, but the clock stopped it: the window was the
        contract and it was not met."""
        warnings = bench.evaluate_suite_signals(self._selftest_run("duration", "done"))
        self.assertTrue(any("of the 500s requested" in w for w in warnings), warnings)

    def test_a_scenario_that_ended_without_done_earns_no_exemption(self) -> None:
        warnings = bench.evaluate_suite_signals(self._selftest_run("selftest", "nav-failed"))
        self.assertTrue(any("of the 500s requested" in w for w in warnings), warnings)
        self.assertTrue(any("rather than done" in w for w in warnings), warnings)

    # The last folder leave's repaint. The Back press renders at once, before
    # storage answers, and the SD leave prints in about 40 ms, so the first
    # render to settle after `folder_leave` is usually that older Back render.
    @staticmethod
    def _capture(
        lines: list[str], target: int, timeout_s: float = 60.0
    ) -> tuple[dict[str, int], list[dict[str, object]]]:
        import io
        import json

        out = io.StringIO()
        counts = bench.process_capture_stream(
            (line + "\n" for line in lines),
            "folder-nav",
            ("folder_leave", target),
            out=out,
            print_lines=False,
            # Generous by default so the ordering, not the clock, decides.
            pending_prestage_timeout_s=timeout_s,
        )
        return counts, [json.loads(line) for line in out.getvalue().splitlines()]

    ENTER = "bench: folder_enter rows=10 books=4 depth=1 ms=41 t_ms=1000"
    BACK = "bench: input button=Some(Back) aux=2000 nav=0 page_raw=0 t_ms=1400"
    LEAVE = "bench: folder_leave rows=17 depth=0 ok=true ms=38 t_ms=1450"
    # Requested at the press, before the leave; settles after it.
    BACK_RENDER = (
        "bench: render view=Library mode=Fast page=0 chapter=0 layout_ms=5 flush_ms=405 "
        "req_ms=1401 deq_ms=1402 t_ms=1810"
    )
    # The parent listing, requested once storage answered.
    PARENT_RENDER = (
        "bench: render view=Library mode=Fast page=0 chapter=0 layout_ms=5 flush_ms=405 "
        "req_ms=1830 deq_ms=1831 t_ms=2240"
    )

    START = "bench-selftest: scenario=folder-nav view=home"
    COMPLETED = "bench-selftest: scenario=folder-nav completed=folder_leave count=1"
    INVALID_QUIET = "bench-selftest: scenario=folder-nav invalid=not-quiescent"
    LEAVE_FAILED = "bench-selftest: scenario=folder-nav result=leave-failed"

    def test_a_selftest_keeps_the_verdict_that_follows_the_qualifying_render(self) -> None:
        """The parent render satisfies the manual boundary but precedes the
        firmware's quiet check on the leave. A selftest capture must run on
        through that verdict, here a failure, and --strict must see it."""
        counts, events = self._capture(
            [
                self.START,
                self.ENTER,
                self.BACK,
                self.LEAVE,
                self.BACK_RENDER,
                self.PARENT_RENDER,
                self.INVALID_QUIET,
                self.LEAVE_FAILED,
            ],
            1,
        )
        kinds = [e.get("event") for e in events]
        self.assertIn("selftest_invalid", kinds)
        self.assertIn("selftest_done", kinds)
        reason = bench.observed_stop_reason(counts, ("folder_leave", 1), None)
        self.assertEqual(reason, "selftest")
        run = [{"suite": "folder-nav", "workflow": "folder-nav", "event": "run_start"}] + events
        warnings = bench.evaluate_suite_signals(run)
        self.assertTrue(any("not-quiescent" in w for w in warnings), warnings)
        self.assertTrue(any("result=leave-failed" in w for w in warnings), warnings)

    def test_a_selftest_stops_on_its_checkpoint_not_on_telemetry(self) -> None:
        counts, events = self._capture(
            [
                self.START,
                self.ENTER,
                self.BACK,
                self.LEAVE,
                self.BACK_RENDER,
                self.PARENT_RENDER,
                self.COMPLETED,
                "bench: folder_enter rows=1 books=0 depth=1 ms=40 t_ms=9000",
            ],
            1,
        )
        # Stopped at the checkpoint: the later line never entered the capture.
        self.assertFalse(any(e.get("t_ms") == 9000 for e in events))
        self.assertEqual(bench.observed_stop_reason(counts, ("folder_leave", 1), None), "count")

    def test_a_selftest_page_turn_keeps_the_last_quiet_verdict(self) -> None:
        """The 2nd pairing and its prestage reach the host before the quiet
        check on that turn has run; the invalid it writes must be captured,
        and the capture stops on the checkpoint that follows it. The firmware
        checkpoints after every turn, so the fixture does too."""
        import io
        import json

        lines = ["bench-selftest: scenario=page-turn view=home"]
        t = 1000
        for i in range(1, 3):
            lines.append(
                f"bench: input button=Some(Next) action=Next aux=2000 nav=0 page_raw=0 t_ms={t}"
            )
            lines.append(
                f"bench: render view=Reading mode=Fast page={i} chapter=0 layout_ms=1 "
                f"flush_ms=405 prestage_ms=24 req_ms={t + 2} deq_ms={t + 3} t_ms={t + 430}"
            )
            lines.append(f"bench: prestage staged=true elapsed_ms=24 t_ms={t + 455}")
            if i == 2:
                lines.append("bench-selftest: scenario=page-turn invalid=not-quiescent")
            lines.append(f"bench-selftest: scenario=page-turn completed=page_turn count={i}")
            t += 2000
        out = io.StringIO()
        counts = bench.process_capture_stream(
            (line + "\n" for line in lines),
            "page-turn",
            ("page_turn", 2),
            out=out,
            print_lines=False,
        )
        events = [json.loads(line) for line in out.getvalue().splitlines()]
        self.assertIn("selftest_invalid", [e.get("event") for e in events])
        self.assertEqual(bench.observed_stop_reason(counts, ("page_turn", 2), None), "count")

    def test_sleep_cycles_ignore_page_turn_checkpoints(self) -> None:
        """sleep-sync turns six pages before each sleep. Those checkpoints must
        not be read as sleep cycles: the run stops on the second completed
        sleep, not the second page turn, and a checkpoint count that restarts
        every boot must not stall a --cycles target it can never reach."""
        import io

        def boot(t: int) -> list[str]:
            lines = ["bench-selftest: scenario=sleep-sync view=home"]
            for i in range(1, 7):
                lines.append(
                    "bench: input button=Some(Next) action=Next aux=2000 nav=0 page_raw=0 "
                    f"t_ms={t + i * 500}"
                )
                lines.append(f"bench-selftest: scenario=sleep-sync completed=page_turn count={i}")
            # The one line the harness counts as a finished cycle.
            lines.append(f"bench: sleep phase=complete ok=true t_ms={t + 4000}")
            return lines

        lines = boot(1000) + boot(20000) + boot(40000)
        out = io.StringIO()
        counts = bench.process_capture_stream(
            (line + "\n" for line in lines),
            "sleep-sync",
            ("sleep_complete", 2),
            out=out,
            print_lines=False,
        )
        self.assertEqual(counts.get("sleep_complete"), 2)
        self.assertEqual(bench.observed_stop_reason(counts, ("sleep_complete", 2), None), "count")

    def test_a_page_turn_checkpoint_does_not_satisfy_a_folder_target(self) -> None:
        counts, _ = self._capture(
            [
                self.START,
                self.ENTER,
                self.BACK,
                self.LEAVE,
                self.BACK_RENDER,
                self.PARENT_RENDER,
                "bench-selftest: scenario=folder-nav completed=page_turn count=5",
            ],
            1,
        )
        self.assertNotEqual(bench.observed_stop_reason(counts, ("folder_leave", 1), None), "count")

    def test_a_late_attach_is_still_self_driven(self) -> None:
        """No banner: the capture joined after boot. The injected press carries
        action=, which a hand cannot produce, so the stream is self-driven and
        the count waits for the checkpoint, keeping the verdict that follows
        the qualifying render. Without the announce the first checkpoint seen
        is only the baseline: the round trip it certifies may have begun
        before the capture opened, so the target is owed from the next one."""
        round_trip = [
            "bench: input button=Some(Confirm) action=Confirm aux=2000 nav=0 page_raw=0 t_ms=1300",
            self.ENTER,
            self.BACK,
            self.LEAVE,
            self.BACK_RENDER,
            self.PARENT_RENDER,
        ]
        # joined mid-scenario: this is the scenario's 8th round trip
        lines = [
            *round_trip,
            self.INVALID_QUIET,
            "bench-selftest: scenario=folder-nav completed=folder_leave count=8",
        ]
        counts, events = self._capture(lines, 1)
        self.assertIn("selftest_invalid", [e.get("event") for e in events])
        self.assertEqual(counts.get("selftest_completed:folder_leave", 0), 0)
        self.assertNotEqual(bench.observed_stop_reason(counts, ("folder_leave", 1), None), "count")
        counts, _ = self._capture(
            [
                *lines,
                *round_trip,
                "bench-selftest: scenario=folder-nav completed=folder_leave count=9",
            ],
            1,
        )
        self.assertEqual(counts.get("selftest_completed:folder_leave", 0), 1)
        self.assertEqual(bench.observed_stop_reason(counts, ("folder_leave", 1), None), "count")

    @classmethod
    def _late_attach_run(cls, full_round_trips: int, requested: int) -> list:
        """A folder-nav selftest joined during its 16th round trip.

        The stream opens on that round trip's `folder_leave` and checkpoint,
        then carries `full_round_trips` whole ones, then the scenario's own
        `result=done`, as a finite scenario that outran the target would.
        """
        events = [
            {
                "event": "run_start",
                "suite": "folder-nav",
                "workflow": "folder-nav",
                "host_time": 1.0,
                "requested": {"folder_entries": requested},
            }
        ]
        round_trip = [
            "bench: input button=Some(Confirm) action=Confirm aux=2000 nav=0 page_raw=0 t_ms=1300",
            cls.ENTER,
            "bench: input button=Some(Back) action=Back aux=2000 nav=0 page_raw=0 t_ms=1400",
            cls.LEAVE,
            cls.BACK_RENDER,
            cls.PARENT_RENDER,
        ]
        lines = [cls.LEAVE, "bench-selftest: scenario=folder-nav completed=folder_leave count=16"]
        for index in range(full_round_trips):
            lines.extend(round_trip)
            lines.append(
                f"bench-selftest: scenario=folder-nav completed=folder_leave count={17 + index}"
            )
        lines.append("bench-selftest: scenario=folder-nav result=done")
        for line in lines:
            events.extend(bench.parse_line(line, "folder-nav"))
        events.append(
            {"event": "run_end", "elapsed_s": 90.0, "stop_reason": "selftest", "completed": True}
        )
        return events

    def test_a_late_attach_is_judged_on_the_round_trips_it_watched_whole(self) -> None:
        """The stop counts from the baseline; so must the report.

        Four whole round trips after the baseline, then `result=done`: the
        count stop is not reached, the scenario ends the capture, and five
        `folder_leave` records sit in the log. The report must not count the
        partial one the stop rule excluded.
        """
        warnings = bench.evaluate_suite_signals(self._late_attach_run(4, 5))
        self.assertEqual(
            [w for w in warnings if "round trips" in w],
            [
                (
                    "folder-nav: 4 of 5 requested folder round trips captured; "
                    "the run is short of the sample count it was asked for"
                )
            ],
        )
        self.assertEqual(
            [
                w
                for w in bench.evaluate_suite_signals(self._late_attach_run(5, 5))
                if "round trips" in w
            ],
            [],
        )

    def test_a_checkpoint_does_not_stand_in_for_a_missing_measurement(self) -> None:
        """Five counted checkpoints over four measurable round trips.

        The device did complete the fifth round trip, so the checkpoint is
        real, but its `folder_leave` line is gone from the stream and the
        statistics would be drawn from four. Both counts are owed.
        """
        events = self._late_attach_run(5, 5)
        leaves = [i for i, e in enumerate(events) if e.get("event") == "folder_leave"]
        del events[leaves[-1]]
        warnings = bench.evaluate_suite_signals(events)
        self.assertEqual(
            [w for w in warnings if "round trips" in w],
            [
                (
                    "folder-nav: the device completed 5 folder round trips but only "
                    "4 of the 5 requested were measured (5 entries and 4 leaves); the "
                    "telemetry for the rest is missing from the capture"
                )
            ],
        )

    def test_a_round_trip_without_its_enter_is_not_measured(self) -> None:
        """The mirror: five checkpoints, five leaves, one `folder_enter` gone.

        Enter and leave feed separate budgets. Counting leaves alone would
        call this five measured round trips with four enter samples.
        """
        events = self._late_attach_run(5, 5)
        enters = [i for i, e in enumerate(events) if e.get("event") == "folder_enter"]
        del events[enters[-1]]
        warnings = bench.evaluate_suite_signals(events)
        self.assertEqual(
            [w for w in warnings if "round trips" in w],
            [
                (
                    "folder-nav: the device completed 5 folder round trips but only "
                    "4 of the 5 requested were measured (4 entries and 5 leaves); the "
                    "telemetry for the rest is missing from the capture"
                )
            ],
        )

    def test_a_manual_round_trip_is_also_both_halves(self) -> None:
        events = [
            {
                "event": "run_start",
                "suite": "folder-nav",
                "workflow": "folder-nav",
                "host_time": 1.0,
                "requested": {"folder_entries": 5},
            }
        ]
        for index in range(5):
            lines = [self.ENTER, self.BACK, self.LEAVE, self.BACK_RENDER, self.PARENT_RENDER]
            if index == 2:
                lines.remove(self.ENTER)
            for line in lines:
                events.extend(bench.parse_line(line, "folder-nav"))
        events.append(
            {"event": "run_end", "elapsed_s": 90.0, "stop_reason": "count", "completed": True}
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertEqual(
            [w for w in warnings if "round trips" in w],
            [
                (
                    "folder-nav: 4 of 5 requested folder round trips captured (4 entries "
                    "and 5 leaves); the run is short of the sample count it was asked for"
                )
            ],
        )

    def test_a_page_turn_checkpoint_does_not_stand_in_for_a_missing_render(self) -> None:
        """Fifty turns completed, forty-nine paired renders in the log."""
        events = [
            {
                "event": "run_start",
                "suite": "page-turn",
                "workflow": "page-turn",
                "host_time": 1.0,
                "requested": {"page_turns": 50},
            }
        ]
        events.extend(bench.parse_line("bench-selftest: scenario=page-turn view=home", "page-turn"))
        for index in range(50):
            press = 1000 + index * 3000
            events.extend(
                bench.parse_line(
                    "bench: input button=Some(Next) action=Next aux=2000 nav=0 page_raw=0 "
                    f"t_ms={press}",
                    "page-turn",
                )
            )
            if index != 0:
                events.extend(
                    bench.parse_line(
                        "bench: render view=Reading mode=Fast page=1 chapter=0 layout_ms=5 "
                        f"flush_ms=405 req_ms={press} deq_ms={press + 1} t_ms={press + 470}",
                        "page-turn",
                    )
                )
            events.extend(
                bench.parse_line(
                    f"bench-selftest: scenario=page-turn completed=page_turn count={index + 1}",
                    "page-turn",
                )
            )
        events.append(
            {"event": "run_end", "elapsed_s": 160.0, "stop_reason": "count", "completed": True}
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertIn(
            "page-turn: the device completed 50 page turns but only 49 of the 50 requested "
            "were measured; the telemetry for the rest is missing from the capture",
            warnings,
        )

    def test_a_manual_capture_is_still_judged_on_its_telemetry(self) -> None:
        """No selftest record and no `action=`: five leaves are five round trips."""
        events = [
            {
                "event": "run_start",
                "suite": "folder-nav",
                "workflow": "folder-nav",
                "host_time": 1.0,
                "requested": {"folder_entries": 5},
            }
        ]
        for _ in range(5):
            for line in (self.ENTER, self.BACK, self.LEAVE, self.BACK_RENDER, self.PARENT_RENDER):
                events.extend(bench.parse_line(line, "folder-nav"))
        events.append(
            {"event": "run_end", "elapsed_s": 90.0, "stop_reason": "count", "completed": True}
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertEqual([w for w in warnings if "round trips" in w], [])

    def test_the_back_render_does_not_complete_the_last_leave(self) -> None:
        # Stream ends after the Back render: the wait is still open, so the
        # run must not read as complete.
        counts, _ = self._capture([self.ENTER, self.BACK, self.LEAVE, self.BACK_RENDER], 1)
        reason = bench.observed_stop_reason(counts, ("folder_leave", 1), None)
        self.assertEqual(reason, "leave-unrendered")
        self.assertNotIn(reason, bench.COMPLETED_STOP_REASONS)

    def test_a_leave_whose_repaint_never_comes_is_not_complete(self) -> None:
        """The clock path: the deadline passes with only noise after the leave."""
        counts, _ = self._capture(
            [
                self.ENTER,
                self.BACK,
                self.LEAVE,
                self.BACK_RENDER,
                "sd: session exit",
                "sd: session exit",
            ],
            1,
            timeout_s=0.0,
        )
        reason = bench.observed_stop_reason(counts, ("folder_leave", 1), None)
        self.assertEqual(reason, "leave-unrendered")

    def test_the_parent_render_completes_a_manual_capture(self) -> None:
        """No selftest_start: an operator's capture has no checkpoint, so the
        parent render is the best available boundary."""
        counts, events = self._capture(
            [self.ENTER, self.BACK, self.LEAVE, self.BACK_RENDER, self.PARENT_RENDER], 1
        )
        reason = bench.observed_stop_reason(counts, ("folder_leave", 1), None)
        self.assertEqual(reason, "count")
        # And the capture kept the render that completed it.
        self.assertEqual(sum(1 for e in events if e.get("event") == "render"), 2)

    def test_a_press_is_read_by_its_action_when_it_carries_one(self) -> None:
        # Injected under PagesLeft: the key that turns a page is Confirm.
        swapped = bench.parse_line(
            "bench: input button=Some(Confirm) action=Next aux=2000 nav=0 page_raw=0 t_ms=5",
            "page-turn",
        )[0]
        manual_next = bench.parse_line(
            "bench: input button=Some(Next) aux=2000 nav=0 page_raw=0 t_ms=6", "page-turn"
        )[0]
        manual_confirm = bench.parse_line(
            "bench: input button=Some(Confirm) aux=2000 nav=0 page_raw=0 t_ms=7", "page-turn"
        )[0]
        self.assertEqual(swapped.get("button"), "Confirm")
        self.assertIn("page_input", bench.event_counters(swapped))
        self.assertIn("page_input", bench.event_counters(manual_next))
        self.assertNotIn("page_input", bench.event_counters(manual_confirm))

    def test_a_refused_leave_is_not_a_sample_for_the_leave_budget(self) -> None:
        """A refused leave returns fast and is a card fault the suite check
        names on its own; letting it into the percentile would measure how
        quickly the card said no. With only the refusal present, the budget
        covered no sample and fails closed."""
        events = [
            self.FOLDER_START,
            {"event": "folder_enter", "rows": 14, "depth": 1, "ok": True, "ms": 87},
            {"event": "folder_leave", "rows": 20, "depth": 1, "ok": False, "ms": 2},
        ]
        warnings = bench.evaluate_budgets(events, self.FOLDER_BUDGETS)
        self.assertTrue(any("no folder_leave events" in w for w in warnings), warnings)
        self.assertFalse(any("folder leave p95" in w for w in warnings), warnings)

    @patch("builtins.print")
    def test_the_report_prints_one_line_per_path(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(json.dumps(dict(event, suite="storage-cache")) for event in self.RUN)
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("storage open (ram)", printed)
        self.assertIn("storage open (warm)", printed)
        self.assertIn("storage open (cold)", printed)
        # Never a pooled median across the three, as with boot-to-paint.
        self.assertNotRegex(printed, r"(?m)^storage open\s+median")


class BudgetSchemaTests(unittest.TestCase):
    """A malformed budget file is a configuration error, not a silent pass."""

    def test_a_valid_document_has_no_problems(self) -> None:
        self.assertEqual(
            bench.budget_schema_problems({"page-turn": {"median_press_to_settled_ms": 550}}),
            [],
        )

    def test_a_misspelled_key_is_rejected(self) -> None:
        """This left page-turn with no operative latency threshold at all."""
        problems = bench.budget_schema_problems({"page-turn": {"median_press_to_settledd_ms": 550}})
        self.assertTrue(
            any("unknown key median_press_to_settledd_ms" in p for p in problems),
            problems,
        )

    def test_an_unknown_section_is_rejected(self) -> None:
        problems = bench.budget_schema_problems({"page-turns": {"prestage_warn_ms": 40}})
        self.assertTrue(any("unknown section [page-turns]" in p for p in problems))

    def test_a_string_threshold_is_rejected(self) -> None:
        problems = bench.budget_schema_problems(
            {"page-turn": {"median_press_to_settled_ms": "550"}}
        )
        self.assertTrue(any("must be an integer" in p for p in problems), problems)

    def test_a_boolean_threshold_is_rejected(self) -> None:
        """`isinstance(True, int)` holds, so a bool reached the comparison."""
        problems = bench.budget_schema_problems({"page-turn": {"median_press_to_settled_ms": True}})
        self.assertTrue(any("must be an integer" in p for p in problems), problems)

    def test_an_empty_document_is_rejected(self) -> None:
        self.assertEqual(
            bench.budget_schema_problems({}),
            ["the document configures no budget sections"],
        )

    def test_strict_refuses_a_budget_file_with_no_sections(self) -> None:
        """An empty or comments-only file parsed to `{}` and gated nothing.

        `budget_sections_in_play` cannot tell that from budgets deliberately
        disabled, so `--strict` exited 0 having enforced not one threshold —
        over a capture the real file would have failed.
        """
        over_budget = [
            {"suite": "page-turn", "workflow": "page-turn", "event": "run_start"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 9000, "req_ms": 1000},
        ]
        # The same capture against a configured section, to show the empty
        # file was hiding a real violation rather than describing a clean run.
        self.assertTrue(
            any(
                "page-turn median" in warning
                for warning in bench.evaluate_budgets(
                    over_budget, {"page-turn": {"median_press_to_settled_ms": 550}}
                )
            )
        )
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            log.write_text(
                "\n".join(json.dumps(event) for event in over_budget) + "\n",
                encoding="utf-8",
            )
            budgets = Path(tmp) / "empty.toml"
            budgets.write_text("# nothing configured\n", encoding="utf-8")
            with self.assertRaises(SystemExit) as ctx:
                bench.summarize_paths([log], budgets, validate_suites=True)
        self.assertIn("configures no budget sections", str(ctx.exception))

    def test_an_empty_section_is_rejected(self) -> None:
        """It names a workflow, survives section selection, and gates nothing."""
        problems = bench.budget_schema_problems({"page-turn": {}})
        self.assertTrue(any("configures no budget" in p for p in problems), problems)

    def test_a_section_that_is_not_a_table_is_rejected(self) -> None:
        problems = bench.budget_schema_problems({"page-turn": 550})
        self.assertTrue(any("is not a table" in p for p in problems), problems)

    def test_invalid_toml_is_reported_rather_than_raised(self) -> None:
        """A syntax error owes the same answer as a missing parser.

        It used to propagate out of `tomllib.load`, so `--strict` died with a
        traceback instead of the SystemExit it promises, and a plain report
        crashed instead of warning.
        """
        with tempfile.TemporaryDirectory() as tmp:
            budgets = Path(tmp) / "benches.toml"
            budgets.write_text("[page-turn\nnot = toml =\n", encoding="utf-8")
            result, problem = bench.load_budgets(budgets)
        self.assertEqual(result, {})
        self.assertIn("cannot parse", problem)

    def test_an_unreadable_budget_path_is_reported_rather_than_raised(self) -> None:
        """A directory satisfies `exists()` and then fails to open.

        Same contract as a missing parser or a syntax error: every
        involuntary empty result carries its reason, so a plain report warns
        and `--strict` raises its own SystemExit instead of a traceback.
        A directory is used because it raises `IsADirectoryError` on every
        platform, where an unreadable file depends on permission behaviour.
        """
        with tempfile.TemporaryDirectory() as tmp:
            not_a_file = Path(tmp) / "budgets.toml"
            not_a_file.mkdir()
            budgets, problem = bench.load_budgets(not_a_file)
        self.assertEqual(budgets, {})
        self.assertIn("cannot read", problem)

    def test_strict_refuses_to_run_against_an_unreadable_budget_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            log.write_text(
                json.dumps({"suite": "page-turn", "event": "render", "view": "Reading", "t_ms": 1})
                + "\n",
                encoding="utf-8",
            )
            not_a_file = Path(tmp) / "budgets.toml"
            not_a_file.mkdir()
            with self.assertRaises(SystemExit) as ctx:
                bench.summarize_paths([log], not_a_file, validate_suites=True)
        self.assertIn("--strict cannot enforce budgets", str(ctx.exception))
        self.assertIn("cannot read", str(ctx.exception))

    def test_load_budgets_reports_a_malformed_document(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            budgets = Path(tmp) / "benches.toml"
            budgets.write_text("[page-turn]\ntypo_ms = 1\n", encoding="utf-8")
            loaded, problem = bench.load_budgets(budgets)
        self.assertEqual(loaded, {})
        self.assertIn("unknown key typo_ms", problem)

    def test_strict_refuses_to_run_against_a_malformed_budget_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            log.write_text(
                json.dumps({"suite": "page-turn", "event": "render", "view": "Reading", "t_ms": 1})
                + "\n",
                encoding="utf-8",
            )
            # The file has to exist, or load_budgets stops at "does not
            # exist" and the parser -- and with it the schema check this
            # test is about -- is never reached.
            budgets = Path(tmp) / "budgets.toml"
            budgets.write_text("[page-turn]\ntypo_ms = 1\n", encoding="utf-8")
            with self.assertRaises(SystemExit) as ctx:
                bench.summarize_paths([log], budgets, validate_suites=True)
        message = str(ctx.exception)
        self.assertIn("--strict cannot enforce budgets", message)
        self.assertIn("unknown key typo_ms", message)

    def test_the_checked_in_budgets_satisfy_the_schema(self) -> None:
        """The files this repo ships must load, or --strict fails everywhere."""
        for budget_file in (bench.DEFAULT_BUDGETS, bench.DEFAULT_BUDGETS_X3):
            text = budget_file.read_text(encoding="utf-8")
            section = None
            for line in text.splitlines():
                line = line.split("#", 1)[0].strip()
                if not line:
                    continue
                if line.startswith("["):
                    section = line.strip("[]")
                    self.assertIn(
                        section,
                        bench.BUDGET_SCHEMA,
                        f"unknown section {section} in {budget_file.name}",
                    )
                    continue
                key, _, value = (part.strip() for part in line.partition("="))
                self.assertIn(
                    key,
                    bench.BUDGET_SCHEMA[section],
                    f"{key} in {budget_file.name} is not in BUDGET_SCHEMA",
                )
                # No leading minus: a negative millisecond budget is a bound no
                # measurement can cross, which is a gate that is silently off.
                self.assertRegex(
                    value, r"^\d+$", f"{key} in {budget_file.name} is not a non-negative integer"
                )

    def test_every_schema_key_is_read_by_bench(self) -> None:
        """A key in the schema that nothing looks up is a dead budget.

        The registry makes a misspelling in benches.toml impossible; this
        keeps the registry itself from growing a key no code enforces.
        """
        source = Path(bench.__file__).read_text(encoding="utf-8")
        for section, keys in bench.BUDGET_SCHEMA.items():
            for key in keys:
                # The definition in BUDGET_SCHEMA, plus at least one read.
                self.assertGreaterEqual(
                    source.count(f'"{key}"'), 2, f"budget key {key} is read by nothing"
                )
            self.assertIn(f'"{section}"', source, f"budget section {section} is unused")


class PinnedInterpreterTests(unittest.TestCase):
    """A capture runs off the shebang, so it checks its own interpreter.

    It never passes through `tools/check.sh`, and the checks below are written
    against whatever precision `.python-version` names rather than a version
    spelled out here, so re-pinning the file stays a one-line change.
    """

    PINNED: ClassVar[str] = (
        (Path(bench.__file__).resolve().parents[2] / ".python-version")
        .read_text(encoding="utf-8")
        .strip()
    )

    @classmethod
    def _version(cls, *, bump: int | None = None, pad: int = 9) -> tuple:
        """A `sys.version_info` for the pin, optionally off by one component."""
        parts = [int(part) for part in cls.PINNED.split(".")]
        if bump is not None:
            parts[bump] += 1
        while len(parts) < 3:
            parts.append(pad)
        return (*parts[:3], "final", 0)

    def test_the_pinned_series_passes(self) -> None:
        with patch.object(sys, "version_info", self._version()):
            bench.require_pinned_python()

    def test_a_release_outside_the_pin_is_refused(self) -> None:
        """Bumping the last component the pin names must not satisfy it."""
        version = self._version(bump=len(self.PINNED.split(".")) - 1)
        with patch.object(sys, "version_info", version), self.assertRaises(SystemExit) as ctx:
            bench.require_pinned_python()
        message = str(ctx.exception)
        self.assertIn(self.PINNED, message)
        self.assertIn(".python-version", message)

    def test_a_patch_release_within_the_series_passes(self) -> None:
        """`3.14` is a series contract: 3.14.0 and 3.14.9 both satisfy it.

        Fails if the pin is ever made fully specific, which is the right time
        to be told that the contract changed.
        """
        self.assertEqual(len(self.PINNED.split(".")), 2, self.PINNED)
        major, minor = (int(part) for part in self.PINNED.split("."))
        for patch_level in (0, 9):
            with self.subTest(patch=patch_level):
                version = (major, minor, patch_level, "final", 0)
                with patch.object(sys, "version_info", version):
                    bench.require_pinned_python()

    def test_a_copy_outside_the_repo_checks_nothing(self) -> None:
        """Nothing to compare against, so it must not invent a failure."""
        elsewhere = patch.object(bench, "__file__", "/tmp/elsewhere/bench.py")
        with elsewhere, patch.object(sys, "version_info", (3, 9, 6, "final", 0)):
            bench.require_pinned_python()


class CountAndDurationContractTests(unittest.TestCase):
    """A count and a duration are not both minimums.

    `--seconds` is offered by every capture command and `page-turn` and
    `sleep-sync` always carry a count target, so recording both made
    `page-turn --seconds 60` unsatisfiable: the capture stops at whichever
    lands first and the report then faulted it for the other.
    """

    def _request(self, command: str, seconds, **extra) -> dict:
        args = argparse.Namespace(command=command, seconds=seconds, **extra)
        suite = bench.SUITES[command]
        return bench.capture_request(
            args, bench.capture_seconds(args), bench.stop_target_for(args, suite)
        )

    def test_a_counting_suite_records_only_its_count(self) -> None:
        self.assertEqual(self._request("page-turn", 60, turns=50), {"page_turns": 50})
        self.assertEqual(self._request("sleep-sync", 60, cycles=10), {"sleep_cycles": 10})

    def test_a_counting_suite_without_seconds_is_unchanged(self) -> None:
        self.assertEqual(self._request("page-turn", None, turns=50), {"page_turns": 50})

    def test_a_duration_beats_a_count_nobody_typed(self) -> None:
        """`page-turn --seconds 60` owes 60 seconds, not 50 turns.

        50 is this suite's default, not a request. Holding a time-boxed
        capture to it reported almost every one as short of a sample count
        the operator never asked for.
        """
        self.assertEqual(self._request("page-turn", 60, turns=None), {"seconds": 60})
        self.assertEqual(self._request("sleep-sync", 60, cycles=None), {"seconds": 60})

    def test_a_defaulted_count_does_not_stop_a_duration_capture(self) -> None:
        """It must leave the stopping rule as well as the contract.

        Otherwise the capture still ends at turn 50 and then fails the
        duration it does owe — the same unsatisfiable pair, mirrored.
        """
        args = argparse.Namespace(command="page-turn", seconds=60, turns=None)
        self.assertIsNone(bench.stop_target_for(args, bench.SUITES["page-turn"]))

    def test_a_named_count_still_outranks_a_duration(self) -> None:
        args = argparse.Namespace(command="page-turn", seconds=60, turns=50)
        self.assertEqual(bench.stop_target_for(args, bench.SUITES["page-turn"]), ("page_turn", 50))

    def test_the_suite_default_still_applies_with_no_duration(self) -> None:
        args = argparse.Namespace(command="page-turn", seconds=None, turns=None)
        self.assertEqual(bench.stop_target_for(args, bench.SUITES["page-turn"]), ("page_turn", 50))
        args = argparse.Namespace(command="sleep-sync", seconds=None, cycles=None)
        self.assertEqual(
            bench.stop_target_for(args, bench.SUITES["sleep-sync"]),
            ("sleep_complete", 10),
        )

    def test_a_seconds_bounded_page_turn_run_is_not_faulted_for_turns(self) -> None:
        """End to end: the warning that used to fire on every timed capture."""
        events = [
            {
                "event": "run_start",
                "suite": "page-turn",
                "workflow": "page-turn",
                "host_time": 1.0,
                "requested": self._request("page-turn", 60, turns=None),
            },
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1470, "req_ms": 1000},
            {
                "event": "run_end",
                "elapsed_s": 60.0,
                "stop_reason": "duration",
                "completed": True,
            },
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertFalse(any("requested page turns" in w for w in warnings), warnings)
        self.assertEqual(warnings, [])

    def test_a_duration_suite_records_its_duration(self) -> None:
        self.assertEqual(
            self._request("storage-cache", 20, cold=False, warm=False),
            {"seconds": 20},
        )
        self.assertEqual(self._request("reader-soak", None, minutes=30), {"seconds": 1800})
        self.assertEqual(
            self._request("thermal-run", None, minutes=45, suite="page-turn"),
            {"seconds": 2700},
        )

    @staticmethod
    def _page_turn_run(turns: int, requested: int, elapsed_s: float, stop: str) -> list:
        events = [
            {
                "event": "run_start",
                "suite": "page-turn",
                "workflow": "page-turn",
                "host_time": 1.0,
                "requested": {"page_turns": requested},
            }
        ]
        for index in range(turns):
            press = 1000 + index * 3000
            events.append({"event": "input", "button": "Next", "t_ms": press})
            events.append(
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": press + 470,
                    "req_ms": press,
                }
            )
        events.append(
            {
                "event": "run_end",
                "elapsed_s": elapsed_s,
                "stop_reason": stop,
                "completed": True,
            }
        )
        return events

    def test_a_skipped_render_is_answered_but_is_not_a_turn(self) -> None:
        """A14's frame-identity guard settles a press without sending a
        frame. The press is answered, so it is not unmatched, but nothing
        turned and its ~12 ms must stay out of the turn population."""
        events = [
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "render", "view": "Reading", "t_ms": 1350, "req_ms": 1000, "deq_ms": 1001},
            {"event": "input", "button": "Next", "t_ms": 2000},
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 2012,
                "req_ms": 2000,
                "deq_ms": 2001,
                "skipped": True,
            },
        ]
        stats = bench.page_turn_stats_over_epochs(events)
        self.assertEqual(stats.durations, [350], "the skipped render is not a turn")
        self.assertEqual(stats.presses, 2, "both presses still count")
        self.assertEqual(stats.skipped_answered, 1, "the skipped press has its own bucket")
        self.assertEqual(stats.unmatched_presses, 0, "and both presses were answered")
        self.assertEqual(stats.untrusted_fraction, 0.0, "a skip is not a trust problem")

        # A capture from a build without the field reads as it always did.
        older = [{k: v for k, v in e.items() if k != "skipped"} for e in events]
        self.assertEqual(bench.page_turn_stats_over_epochs(older).durations, [350, 12])

    def test_the_count_landing_first_is_a_clean_pass(self) -> None:
        """`--turns 3 --seconds 600`, done in twelve seconds."""
        events = self._page_turn_run(3, 3, 12.0, "count")
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_the_deadline_landing_first_is_reported_as_short(self) -> None:
        """`--turns 50 --seconds 60`, cut off at two turns.

        One complaint, and it is the true one: the run is short of its
        samples. It must not also be faulted for a duration it never owed.
        """
        events = self._page_turn_run(2, 50, 60.0, "duration")
        warnings = bench.evaluate_suite_signals(events)
        self.assertEqual(
            warnings,
            [
                (
                    "page-turn: 2 of 50 requested page turns captured; the run "
                    "is short of the sample count it was asked for"
                )
            ],
        )

    def test_the_startup_banner_names_both_stop_conditions(self) -> None:
        """The count message used to sit behind an `elif` and never print.

        The operator was shown only the duration, then had the report fault
        the run for a count nothing had told them was still in force.
        """
        printed: list = []
        CaptureContractTests._capture("page-turn", [], turns=50, seconds=60, printed=printed)
        banner = "\n".join(printed)
        self.assertIn("50 parsed page_turn(s) or 60s, whichever comes first", banner)
        self.assertIn("--seconds is a ceiling", banner)

    def test_a_count_only_capture_names_just_the_count(self) -> None:
        printed: list = []
        CaptureContractTests._capture("page-turn", [], turns=50, printed=printed)
        banner = "\n".join(printed)
        self.assertIn("stop: after 50 parsed page_turn(s)", banner)
        self.assertNotIn("ceiling", banner)


class BackgroundBuildTests(unittest.TestCase):
    """A background walk's build belongs to no open."""

    def test_a_background_build_does_not_make_the_next_open_cold(self) -> None:
        """The exact lifecycle: build, announcement, then an ordinary open."""
        events = [
            {"event": "storage_build", "elapsed_ms": 14948},
            {"event": "storage_background_build", "book_id": 2, "elapsed_ms": 61000},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 72},
        ]
        kinds = bench.storage_open_kinds(events)
        self.assertEqual(bench.values(kinds["warm"], "elapsed_ms"), [72])
        self.assertEqual(kinds["cold"], [])

    def test_a_foreground_build_still_makes_its_open_cold(self) -> None:
        events = [
            {"event": "storage_build", "elapsed_ms": 14948},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 15034},
        ]
        self.assertEqual(
            bench.values(bench.storage_open_kinds(events)["cold"], "elapsed_ms"),
            [15034],
        )

    def test_the_warm_sample_survives_into_the_budget(self) -> None:
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"},
            {"event": "storage_build", "elapsed_ms": 14948},
            {"event": "storage_background_build", "book_id": 2, "elapsed_ms": 61000},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 900},
        ]
        warnings = bench.evaluate_budgets(
            events, {"storage-cache": {"warm_book_open_warn_ms": 150}}
        )
        self.assertTrue(any("warm book open p95 900ms above" in w for w in warnings), warnings)

    def test_a_requested_warm_path_is_not_lost_to_a_background_build(self) -> None:
        events = [
            {
                "event": "run_start",
                "suite": "storage-cache",
                "workflow": "storage-cache",
                "host_time": 1.0,
                "requested": {"storage_modes": ["warm"]},
            },
            {"event": "storage_build", "elapsed_ms": 14948},
            {"event": "storage_background_build", "book_id": 2, "elapsed_ms": 61000},
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 72},
            {"event": "run_end", "elapsed_s": 90.0, "completed": True},
        ]
        self.assertEqual(bench.evaluate_suite_signals(events), [])


class CatalogResultTests(unittest.TestCase):
    """A failed catalog operation is not evidence, and not a sample."""

    def test_the_scan_line_carries_its_own_result(self) -> None:
        event = bench.parse_line(
            "bench: storage_catalog action=scan ok=false status=Ready count=7 "
            "elapsed_ms=900 t_ms=4200",
            "storage-cache",
        )[0]
        self.assertEqual(event["action"], "scan")
        self.assertFalse(event["ok"])
        # The firmware keeps the reader on its older in-memory catalog, so the
        # status says Ready for a scan that did not happen.
        self.assertEqual(event["status"], "Ready")

    @staticmethod
    def _run(catalog: dict, modes: list) -> list:
        return [
            {
                "event": "run_start",
                "suite": "storage-cache",
                "workflow": "storage-cache",
                "host_time": 1.0,
                "requested": {"storage_modes": modes},
            },
            dict(catalog, event="storage_catalog"),
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 72},
            {"event": "run_end", "elapsed_s": 20.0, "completed": True},
        ]

    def test_a_failed_scan_is_not_cold_evidence(self) -> None:
        events = self._run(
            {"action": "scan", "ok": False, "status": "Ready", "elapsed_ms": 900},
            ["cold"],
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("--cold was requested" in w for w in warnings), warnings)

    def test_a_successful_scan_is_cold_evidence(self) -> None:
        events = self._run(
            {"action": "scan", "ok": True, "status": "Ready", "elapsed_ms": 900},
            ["cold"],
        )
        self.assertEqual([w for w in bench.evaluate_suite_signals(events) if "--cold" in w], [])

    def test_a_scan_predating_the_field_cannot_prove_the_cold_path(self) -> None:
        """Strict evidence is a claim, so it rests only on confirmed success.

        The host tool records `requested.storage_modes` whatever firmware is
        on the device, so a current bench.py against an older build would
        otherwise certify a requested path from a line that cannot say whether
        the scan worked. Nothing regresses by refusing: such a capture never
        had its mode verified in the first place.
        """
        events = self._run(
            {"action": "scan", "status": "Ready", "count": 7, "elapsed_ms": 900},
            ["cold"],
        )
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(
            any("--cold cannot be verified from this capture" in w for w in warnings),
            warnings,
        )

    def test_an_unverified_mode_is_distinguished_from_a_missing_one(self) -> None:
        """Both fail --strict, but the operator fixes them differently."""
        self.assertEqual(
            bench.storage_mode_evidence(
                [{"event": "storage_catalog", "action": "scan", "elapsed_ms": 900}],
                "cold",
            ),
            bench.STORAGE_MODE_UNVERIFIED,
        )
        self.assertEqual(
            bench.storage_mode_evidence([{"event": "render"}], "cold"),
            bench.STORAGE_MODE_ABSENT,
        )
        self.assertEqual(
            bench.storage_mode_evidence(
                [
                    {
                        "event": "storage_catalog",
                        "action": "scan",
                        "ok": True,
                        "elapsed_ms": 900,
                    }
                ],
                "cold",
            ),
            bench.STORAGE_MODE_CONFIRMED,
        )

    def test_a_failed_scan_is_reported_even_when_nothing_was_requested(self) -> None:
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"},
            {"event": "storage_catalog", "action": "scan", "ok": False, "elapsed_ms": 900},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("failed storage operation(s)" in w for w in warnings), warnings)

    def test_a_failed_load_is_not_in_the_catalog_budget(self) -> None:
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"},
            {"event": "storage_catalog", "action": "load", "ok": True, "elapsed_ms": 31},
            {"event": "storage_catalog", "action": "load", "ok": False, "elapsed_ms": 4000},
        ]
        warnings = bench.evaluate_budgets(events, {"storage-cache": {"catalog_load_warn_ms": 500}})
        self.assertEqual([w for w in warnings if "catalog load p95" in w], [], warnings)

    def test_a_failed_load_is_not_warm_evidence(self) -> None:
        events = [
            {
                "event": "run_start",
                "suite": "storage-cache",
                "workflow": "storage-cache",
                "host_time": 1.0,
                "requested": {"storage_modes": ["warm"]},
            },
            {"event": "storage_catalog", "action": "load", "ok": False, "elapsed_ms": 4},
            {"event": "run_end", "elapsed_s": 20.0, "completed": True},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("--warm was requested" in w for w in warnings), warnings)

    @patch("builtins.print")
    def test_the_report_names_the_failed_operations(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(
                    json.dumps(event)
                    for event in [
                        {"suite": "storage-cache", "event": "run_start"},
                        {
                            "suite": "storage-cache",
                            "event": "storage_catalog",
                            "action": "scan",
                            "ok": False,
                            "elapsed_ms": 900,
                        },
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("catalog scan:  1 failed", printed)


class ColdCatalogFallbackTests(unittest.TestCase):
    """A snapshot that was not there yet is the cold path, not a fault.

    `load_catalog_cache` returning false is what queues `RefreshCatalog`, so
    the healthy telemetry for a card whose catalog has not been built is a
    failed-looking load immediately followed by the scan that builds it.
    `--reset-before` makes it the common case.
    """

    MISS: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": False,
        "result": "miss",
        "elapsed_ms": 4,
    }
    SCAN: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "scan",
        "ok": True,
        "status": "Ready",
        "count": 7,
        "elapsed_ms": 900,
    }
    ERROR: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": False,
        "result": "error",
        "elapsed_ms": 12,
    }

    def _run(self, catalog: list, modes: list) -> list:
        return (
            [
                {
                    "event": "run_start",
                    "suite": "storage-cache",
                    "workflow": "storage-cache",
                    "host_time": 1.0,
                    "reset_before": True,
                    "requested": {"storage_modes": modes},
                }
            ]
            + catalog
            + [{"event": "run_end", "elapsed_s": 20.0, "completed": True}]
        )

    def test_the_healthy_cold_boot_sequence_passes(self) -> None:
        events = self._run([self.MISS, self.SCAN], ["cold"])
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_miss_is_not_a_failed_storage_operation(self) -> None:
        self.assertEqual(bench.failed_storage_ops([self.MISS]), [])

    def test_a_card_error_still_is(self) -> None:
        self.assertEqual(bench.failed_storage_ops([self.ERROR]), [self.ERROR])

    def test_a_card_error_fails_strict(self) -> None:
        events = self._run([self.ERROR, self.SCAN], ["cold"])
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("failed storage operation(s)" in w for w in warnings), warnings)

    def test_a_progress_write_failure_is_still_a_failure(self) -> None:
        """Only the catalog load's ok=false is ambiguous; nothing else's is."""
        failed = {"event": "storage_progress", "action": "write", "ok": False}
        self.assertEqual(bench.failed_storage_ops([failed]), [failed])

    def test_a_miss_is_not_warm_evidence(self) -> None:
        events = self._run([self.MISS, self.SCAN], ["warm"])
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("--warm" in w for w in warnings), warnings)

    def test_a_miss_is_not_in_the_catalog_budget(self) -> None:
        """It returns in milliseconds; a real load does not."""
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"},
            {
                "event": "storage_catalog",
                "action": "load",
                "ok": True,
                "result": "hit",
                "elapsed_ms": 31,
            },
            self.MISS,
        ]
        self.assertEqual(bench.values(bench.catalog_samples(events, "load"), "elapsed_ms"), [31])

    INVALID: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": False,
        "result": "invalid",
        "elapsed_ms": 9,
    }

    STALE: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": False,
        "result": "stale",
        "elapsed_ms": 6,
    }

    def test_an_older_catalog_version_is_not_a_failure(self) -> None:
        """The first boot after a CATALOG_VERSION bump.

        Bumping the version is how the on-card format migrates: the old
        snapshot stops loading and the scan rebuilds it, by design and with
        no migration code. Counting that as a storage fault failed
        `--cold --strict` on every device's first boot after a catalog
        upgrade, for behaving exactly as intended.
        """
        self.assertEqual(bench.failed_storage_ops([self.STALE]), [])
        events = self._run([self.STALE, self.SCAN], ["cold"])
        self.assertEqual(bench.evaluate_suite_signals(events), [])

    def test_a_stale_snapshot_is_still_not_a_warm_load(self) -> None:
        """It did not load, so it measures nothing and evidences nothing."""
        self.assertFalse(bench.catalog_load_hit(self.STALE))
        self.assertEqual(bench.catalog_samples([self.STALE], "load"), [])

    def test_an_unusable_snapshot_is_not_a_miss(self) -> None:
        """A header that did not decode, or a record that ended early.

        The card answered and what it held was unusable, which is a finding,
        not the cold path.
        """
        self.assertEqual(bench.failed_storage_ops([self.INVALID]), [self.INVALID])

    def test_a_read_fault_fails_strict_even_though_the_scan_worked(self) -> None:
        """The false pass the firmware taxonomy was widened to close.

        The whole catalog read used to be reduced to a bool inside the SD
        session, so a refused read, a failed seek or a torn file all surfaced
        as `miss` — which the host ignores — and a later successful scan then
        carried the capture to a clean --strict pass.
        """
        for fault in (self.INVALID, self.ERROR):
            with self.subTest(result=fault["result"]):
                events = self._run([fault, self.SCAN], ["cold"])
                warnings = bench.evaluate_suite_signals(events)
                self.assertTrue(any("failed storage operation(s)" in w for w in warnings), warnings)

    UNKNOWN: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": False,
        "result": "timeout",
        "elapsed_ms": 5000,
    }
    HIT: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": True,
        "result": "hit",
        "elapsed_ms": 31,
    }

    def test_an_unknown_result_does_not_pass_in_silence(self) -> None:
        """A newer firmware's token, or a typo, next to a valid hit.

        It is not a hit, not a known fault and not result-less, so every
        predicate answered "no" about it and `--strict` said nothing at all.
        """
        events = [
            {
                "event": "run_start",
                "suite": "storage-cache",
                "workflow": "storage-cache",
                "host_time": 1.0,
                "requested": {"storage_modes": ["warm"]},
            },
            self.UNKNOWN,
            self.HIT,
            {"event": "storage_open", "ram_hit": False, "elapsed_ms": 72},
            {"event": "run_end", "elapsed_s": 20.0, "completed": True},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("does not know ('timeout')" in w for w in warnings), warnings)

    NULL_OK: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": True,
        "result": None,
    }
    NULL_NOT_OK: ClassVar[dict[str, Any]] = {
        "event": "storage_catalog",
        "action": "load",
        "ok": False,
        "result": None,
    }

    def test_an_explicit_null_result_is_not_a_hit(self) -> None:
        """A present-but-unreadable result must not fall back to `ok`.

        `{"ok": true, "result": null}` counted as a confirmed warm hit and
        entered the load budget, because the fallback keyed on the value's
        type rather than the field's presence. Only a line carrying no
        `result` at all is old enough for `ok` to be the whole story.
        """
        self.assertFalse(bench.catalog_load_hit(self.NULL_OK))
        self.assertTrue(bench.unknown_catalog_result(self.NULL_OK))
        self.assertFalse(bench.unverifiable_catalog_op(self.NULL_OK))
        self.assertEqual(bench.catalog_samples([self.NULL_OK], "load"), [])

    def test_an_explicit_null_result_is_unknown_either_way(self) -> None:
        self.assertTrue(bench.unknown_catalog_result(self.NULL_NOT_OK))
        self.assertFalse(bench.catalog_load_hit(self.NULL_NOT_OK))

    def test_a_null_result_fails_strict(self) -> None:
        events = [
            {
                "event": "run_start",
                "suite": "storage-cache",
                "workflow": "storage-cache",
                "host_time": 1.0,
            },
            self.NULL_OK,
            {"event": "run_end", "elapsed_s": 20.0, "completed": True},
        ]
        warnings = bench.evaluate_suite_signals(events)
        self.assertTrue(any("does not know" in w for w in warnings), warnings)

    def test_a_failed_op_with_no_action_is_named_without_one(self) -> None:
        """The label interpolated a missing action as the literal "None"."""
        events = [
            {"suite": "storage-cache", "workflow": "storage-cache", "event": "run_start"},
            {"event": "storage_progress", "ok": False},
        ]
        warnings = [w for w in bench.evaluate_suite_signals(events) if "failed" in w]
        self.assertTrue(warnings)
        self.assertIn("storage_progress", warnings[0])
        self.assertNotIn("None", warnings[0])

    def test_a_non_string_result_is_unknown_too(self) -> None:
        """`parse_value` turns a bare number into an int before we see it."""
        self.assertTrue(
            bench.unknown_catalog_result(
                {"event": "storage_catalog", "action": "load", "result": 0}
            )
        )

    def test_every_known_result_is_accepted(self) -> None:
        for result in bench.CATALOG_LOAD_RESULTS:
            with self.subTest(result=result):
                self.assertFalse(
                    bench.unknown_catalog_result(
                        {"event": "storage_catalog", "action": "load", "result": result}
                    )
                )

    def test_a_result_less_line_is_legacy_not_unknown(self) -> None:
        """The two failure modes stay apart; they need different fixes."""
        legacy = {"event": "storage_catalog", "action": "scan", "elapsed_ms": 900}
        self.assertFalse(bench.unknown_catalog_result(legacy))
        self.assertTrue(bench.unverifiable_catalog_op(legacy))

    def test_an_unknown_result_is_not_warm_evidence_or_a_sample(self) -> None:
        self.assertFalse(bench.catalog_load_hit(self.UNKNOWN))
        self.assertEqual(bench.catalog_samples([self.UNKNOWN], "load"), [])

    @patch("builtins.print")
    def test_the_report_names_an_unrecognised_result(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(
                    json.dumps(dict(event, suite="storage-cache"))
                    for event in [{"event": "run_start"}, self.UNKNOWN]
                )
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("1 reported an unrecognised result (timeout)", printed)

    def test_every_firmware_result_token_is_known_to_the_host(self) -> None:
        """The two sides agree on the vocabulary, or a token means nothing."""
        source = Path(bench.__file__).read_text(encoding="utf-8")
        firmware = Path(__file__).resolve().parents[2] / "fw" / "src" / "library_sd.rs"
        emitted = set(re.findall(r'Self::\w+ => "(\w+)"', firmware.read_text(encoding="utf-8")))
        self.assertTrue(emitted, "no result tokens found -- the scan is broken")
        # "hit" is produced by the caller rather than the fault enum.
        self.assertEqual(emitted | {"hit"}, bench.CATALOG_LOAD_RESULTS)
        for token in emitted | {"hit"}:
            self.assertIn(f'"{token}"', source)

    def test_the_load_line_carries_its_result(self) -> None:
        event = bench.parse_line(
            "bench: storage_catalog action=load ok=false result=miss count=0 "
            "elapsed_ms=4 t_ms=1200",
            "storage-cache",
        )[0]
        self.assertEqual(event["result"], "miss")
        self.assertFalse(event["ok"])

    def test_a_legacy_load_keeps_its_original_meaning(self) -> None:
        """`ok` has been on this line since the harness was written."""
        loaded = {"event": "storage_catalog", "action": "load", "ok": True, "elapsed_ms": 31}
        self.assertTrue(bench.catalog_load_hit(loaded))
        self.assertFalse(bench.unverifiable_catalog_op(loaded))

    @patch("builtins.print")
    def test_the_report_names_a_miss_as_normal(self, mock_print) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log.jsonl"
            path.write_text(
                "\n".join(
                    json.dumps(dict(event, suite="storage-cache"))
                    for event in [{"event": "run_start"}, self.MISS, self.SCAN]
                )
                + "\n",
                encoding="utf-8",
            )
            bench.summarize_paths([path], None)
        printed = "\n".join(str(call.args[0]) for call in mock_print.call_args_list if call.args)
        self.assertIn("1 found no snapshot (normal cold path)", printed)


class BudgetValueTests(unittest.TestCase):
    """An integer is not automatically a usable threshold."""

    def test_a_negative_threshold_is_rejected(self) -> None:
        """`-1` as a floor is a gate no measurement can fall below."""
        problems = bench.budget_schema_problems(
            {"page-turn": {"median_press_to_settled_min_ms": -1}}
        )
        self.assertTrue(any("cannot be negative" in p for p in problems), problems)

    def test_zero_is_allowed(self) -> None:
        """A zero ceiling is degenerate but honest: everything exceeds it."""
        self.assertEqual(bench.budget_schema_problems({"page-turn": {"prestage_warn_ms": 0}}), [])

    def test_a_floor_above_its_ceiling_is_rejected(self) -> None:
        problems = bench.budget_schema_problems(
            {"sleep-sync": {"full_refresh_busy_min_ms": 4300, "full_refresh_busy_max_ms": 3000}}
        )
        self.assertTrue(any("no measurement can satisfy both" in p for p in problems), problems)

    def test_the_page_turn_median_pair_is_checked_too(self) -> None:
        problems = bench.budget_schema_problems(
            {
                "page-turn": {
                    "median_press_to_settled_min_ms": 900,
                    "median_press_to_settled_ms": 550,
                }
            }
        )
        self.assertTrue(any("no measurement can satisfy both" in p for p in problems), problems)

    def test_a_well_ordered_pair_passes(self) -> None:
        self.assertEqual(
            bench.budget_schema_problems(
                {
                    "sleep-sync": {
                        "full_refresh_busy_min_ms": 3000,
                        "full_refresh_busy_max_ms": 4300,
                    }
                }
            ),
            [],
        )

    def test_one_half_of_a_pair_alone_is_fine(self) -> None:
        self.assertEqual(
            bench.budget_schema_problems({"sleep-sync": {"full_refresh_busy_max_ms": 4300}}),
            [],
        )

    def test_every_bound_pair_is_a_real_schema_key(self) -> None:
        for section, pairs in bench.BUDGET_BOUND_PAIRS.items():
            self.assertIn(section, bench.BUDGET_SCHEMA)
            for key in (key for pair in pairs for key in pair):
                self.assertIn(key, bench.BUDGET_SCHEMA[section])


class BoardBudgetTests(unittest.TestCase):
    def test_resolve_budgets_path_defaults(self) -> None:
        self.assertEqual(bench.resolve_budgets_path(None, None), bench.DEFAULT_BUDGETS)
        self.assertEqual(bench.resolve_budgets_path(None, "x4"), bench.DEFAULT_BUDGETS)
        self.assertEqual(bench.resolve_budgets_path(None, "x3"), bench.DEFAULT_BUDGETS_X3)

    def test_capture_board_x3_round_trip_infers_x3_budget_profile(self) -> None:
        """Capture with board='x3' records metadata; flagless report selects X3 budget profile."""
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "log.jsonl"
            capture_args = argparse.Namespace(
                command="page-turn",
                port="/dev/bench-test",
                out=out,
                seconds=1,
                reset_before=False,
                espflash="espflash",
                strict=False,
                board="x3",
                budgets=None,
                note=[],
                book=None,
                turns=1,
            )

            def fake_capture_lines(*_args: Any, **_kwargs: Any) -> Any:
                return iter(
                    [
                        "input: Some(Next) gpio0=1 gpio1=1 gpio2=0 t=1000",
                        "bench: refresh mode=Fast busy_ms=307 t_ms=1307",
                        (
                            "bench: render view=Reading mode=Fast page=1 ch=0 "
                            "layout_ms=15 flush_ms=307 prestage_ms=24 t_ms=1354 req_ms=1000"
                        ),
                        "bench: prestage staged=true elapsed_ms=24",
                    ]
                )

            with (
                patch.object(bench, "capture_lines", fake_capture_lines),
                patch("builtins.print"),
                patch("sys.stdout", io.StringIO()),
            ):
                bench.run_capture(capture_args)

            events = [
                json.loads(line)
                for line in out.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            run_start = next(e for e in events if e.get("event") == "run_start")
            self.assertEqual(run_start.get("board"), "x3")

            report_args = argparse.Namespace(
                paths=[out],
                budgets=None,
                board=None,
                strict=True,
                all=False,
            )
            with (
                patch.object(bench, "load_budgets", wraps=bench.load_budgets) as mock_load,
                patch("builtins.print"),
            ):
                ret = bench.run_report(report_args)
                self.assertEqual(ret, 0)
                mock_load.assert_called_with(bench.DEFAULT_BUDGETS_X3)

    def test_resolve_budgets_path_explicit_overrides_board(self) -> None:
        custom = Path("custom.toml")
        self.assertEqual(bench.resolve_budgets_path(custom, "x3"), custom)
        self.assertEqual(bench.resolve_budgets_path(custom, "x4"), custom)

    def test_resolve_budgets_path_infers_from_events(self) -> None:
        x3_events = [{"event": "run_start", "board": "x3"}]
        x4_events = [{"event": "run_start", "board": "x4"}]
        self.assertEqual(
            bench.resolve_budgets_path(None, None, x3_events), bench.DEFAULT_BUDGETS_X3
        )
        self.assertEqual(bench.resolve_budgets_path(None, None, x4_events), bench.DEFAULT_BUDGETS)

    def test_x4_timings_pass_shared_budgets_but_fail_x3_profile(self) -> None:
        """X4 has ~421 ms Fast BUSY and ~470 ms turn: passes shared, fails X3."""
        x4_events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "refresh", "mode": "Fast", "busy_ms": 421, "t_ms": 1421},
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 1470,
                "layout_ms": 15,
                "req_ms": 1000,
            },
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
        ]
        shared_budgets, _ = bench.load_budgets(bench.DEFAULT_BUDGETS)
        x3_budgets, _ = bench.load_budgets(bench.DEFAULT_BUDGETS_X3)

        shared_warnings = bench.evaluate_budgets(x4_events, shared_budgets)
        self.assertEqual(shared_warnings, [], f"shared budgets failed X4: {shared_warnings}")

        x3_warnings = bench.evaluate_budgets(x4_events, x3_budgets)
        self.assertTrue(
            any("Fast refresh busy" in w and "350ms" in w for w in x3_warnings),
            f"x3 profile did not catch X4 Fast BUSY: {x3_warnings}",
        )
        self.assertTrue(
            any("page-turn median" in w and "400ms" in w for w in x3_warnings),
            f"x3 profile did not catch X4 page turn median: {x3_warnings}",
        )

    def test_x3_a12_timings_pass_x3_profile_while_pre_a12_fails(self) -> None:
        """Post-A12 X3 timings (307 ms Fast, 354 ms turn, 857 ms Full) pass; pre-A12 fails."""
        post_a12_events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "refresh", "mode": "Fast", "busy_ms": 307, "t_ms": 1307},
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 1354,
                "layout_ms": 15,
                "req_ms": 1000,
            },
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
        ]
        pre_a12_events = [
            {"event": "run_start", "suite": "page-turn"},
            {"event": "input", "button": "Next", "t_ms": 1000},
            {"event": "refresh", "mode": "Fast", "busy_ms": 379, "t_ms": 1379},
            {
                "event": "render",
                "view": "Reading",
                "t_ms": 1426,
                "layout_ms": 15,
                "req_ms": 1000,
            },
            {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
        ]
        x3_budgets, _ = bench.load_budgets(bench.DEFAULT_BUDGETS_X3)

        post_warnings = bench.evaluate_budgets(post_a12_events, x3_budgets)
        self.assertEqual(post_warnings, [], f"post-A12 failed X3 profile: {post_warnings}")

        pre_warnings = bench.evaluate_budgets(pre_a12_events, x3_budgets)
        self.assertTrue(
            any("Fast refresh busy" in w and "350ms" in w for w in pre_warnings),
            f"x3 profile did not catch pre-A12 Fast BUSY: {pre_warnings}",
        )
        self.assertTrue(
            any("page-turn median" in w and "400ms" in w for w in pre_warnings),
            f"x3 profile did not catch pre-A12 page turn median: {pre_warnings}",
        )

    def test_x4_sleep_sync_passes_shared_budgets_but_fails_x3_profile(self) -> None:
        """X4 Full refresh is ~3500 ms: passes shared (ceiling 4300 ms), fails X3 (ceiling 900 ms)."""
        x4_sleep_events = [
            {"event": "run_start", "suite": "sleep-sync"},
            {"event": "refresh", "mode": "Full", "busy_ms": 3500, "t_ms": 5000},
            {"event": "sleep", "phase": "refresh", "ok": True, "t_ms": 5005},
        ]
        shared_budgets, _ = bench.load_budgets(bench.DEFAULT_BUDGETS)
        x3_budgets, _ = bench.load_budgets(bench.DEFAULT_BUDGETS_X3)

        shared_warnings = bench.evaluate_budgets(x4_sleep_events, shared_budgets)
        self.assertEqual(
            shared_warnings, [], f"shared budgets failed X4 sleep-sync: {shared_warnings}"
        )

        x3_warnings = bench.evaluate_budgets(x4_sleep_events, x3_budgets)
        self.assertTrue(
            any("Full refresh busy" in w and "900ms" in w for w in x3_warnings),
            f"x3 profile did not catch X4 Full refresh BUSY: {x3_warnings}",
        )

    def test_scoping_infers_x3_budgets_for_latest_when_preceded_by_x4(self) -> None:
        """X4 run followed by X3 run: latest run infers X3 and catches pre-A12 regression."""
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            # Run 1: X4 healthy run
            r1 = [
                {"event": "run_start", "suite": "page-turn", "board": "x4"},
                {"event": "input", "button": "Next", "t_ms": 1000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 421, "t_ms": 1421},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 1470,
                    "layout_ms": 15,
                    "req_ms": 1000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            # Run 2: X3 run with pre-A12 timing (379 ms Fast BUSY: passes X4 500ms, fails X3 350ms)
            r2 = [
                {"event": "run_start", "suite": "page-turn", "board": "x3"},
                {"event": "input", "button": "Next", "t_ms": 2000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 379, "t_ms": 2379},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 2426,
                    "layout_ms": 15,
                    "req_ms": 2000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            with log.open("w", encoding="utf-8") as f:
                for event in r1 + r2:
                    f.write(json.dumps(event) + "\n")

            args = argparse.Namespace(paths=[log], budgets=None, board=None, strict=True, all=False)
            ret = bench.run_report(args)
            self.assertEqual(ret, 1, "expected pre-A12 X3 run to fail under inferred X3 budgets")

    def test_scoping_infers_x4_budgets_for_latest_when_preceded_by_x3(self) -> None:
        """X3 run followed by X4 run: latest run infers X4 and passes healthy X4."""
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            # Run 1: X3 run
            r1 = [
                {"event": "run_start", "suite": "page-turn", "board": "x3"},
                {"event": "input", "button": "Next", "t_ms": 1000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 307, "t_ms": 1307},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 1354,
                    "layout_ms": 15,
                    "req_ms": 1000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            # Run 2: X4 healthy run (421 ms Fast BUSY: fails X3 350ms, passes X4 500ms)
            r2 = [
                {"event": "run_start", "suite": "page-turn", "board": "x4"},
                {"event": "input", "button": "Next", "t_ms": 2000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 421, "t_ms": 2421},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 2470,
                    "layout_ms": 15,
                    "req_ms": 2000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            with log.open("w", encoding="utf-8") as f:
                for event in r1 + r2:
                    f.write(json.dumps(event) + "\n")

            args = argparse.Namespace(paths=[log], budgets=None, board=None, strict=True, all=False)
            ret = bench.run_report(args)
            self.assertEqual(ret, 0, "expected healthy X4 run to pass under inferred X4 budgets")

    def test_pooled_mixed_boards_refused_under_strict(self) -> None:
        """--all with mixed x4 and x3 runs raises SystemExit under --strict unless profile is specified."""
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            r1 = [
                {"event": "run_start", "suite": "page-turn", "board": "x4"},
                {"event": "input", "button": "Next", "t_ms": 1000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 421, "t_ms": 1421},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 1470,
                    "layout_ms": 15,
                    "req_ms": 1000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            r2 = [
                {"event": "run_start", "suite": "page-turn", "board": "x3"},
                {"event": "input", "button": "Next", "t_ms": 2000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 307, "t_ms": 2307},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 2354,
                    "layout_ms": 15,
                    "req_ms": 2000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            with log.open("w", encoding="utf-8") as f:
                for event in r1 + r2:
                    f.write(json.dumps(event) + "\n")

            args = argparse.Namespace(paths=[log], budgets=None, board=None, strict=True, all=True)
            with self.assertRaises(SystemExit) as ctx:
                bench.run_report(args)
            msg = str(ctx.exception)
            self.assertIn("--strict cannot evaluate pooled runs from multiple boards", msg)
            self.assertIn("x3, x4", msg)

    def test_pooled_mixed_boards_allowed_with_explicit_board(self) -> None:
        """--all with mixed x4 and x3 runs is allowed under --strict if explicit --board is passed."""
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            r1 = [
                {"event": "run_start", "suite": "page-turn", "board": "x4"},
                {"event": "input", "button": "Next", "t_ms": 1000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 421, "t_ms": 1421},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 1470,
                    "layout_ms": 15,
                    "req_ms": 1000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            r2 = [
                {"event": "run_start", "suite": "page-turn", "board": "x3"},
                {"event": "input", "button": "Next", "t_ms": 2000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 307, "t_ms": 2307},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 2354,
                    "layout_ms": 15,
                    "req_ms": 2000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            with log.open("w", encoding="utf-8") as f:
                for event in r1 + r2:
                    f.write(json.dumps(event) + "\n")

            # Explicit --board x4: both runs evaluated against shared X4 budgets
            args = argparse.Namespace(paths=[log], budgets=None, board="x4", strict=True, all=True)
            ret = bench.run_report(args)
            self.assertEqual(ret, 0)

    @patch("builtins.print")
    def test_pooled_mixed_boards_warns_in_non_strict(self, mock_print) -> None:
        """--all with mixed x4 and x3 runs in non-strict mode prints warning and does not raise."""
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "log.jsonl"
            r1 = [
                {"event": "run_start", "suite": "page-turn", "board": "x4"},
                {"event": "input", "button": "Next", "t_ms": 1000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 421, "t_ms": 1421},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 1470,
                    "layout_ms": 15,
                    "req_ms": 1000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            r2 = [
                {"event": "run_start", "suite": "page-turn", "board": "x3"},
                {"event": "input", "button": "Next", "t_ms": 2000},
                {"event": "refresh", "mode": "Fast", "busy_ms": 307, "t_ms": 2307},
                {
                    "event": "render",
                    "view": "Reading",
                    "t_ms": 2354,
                    "layout_ms": 15,
                    "req_ms": 2000,
                },
                {"event": "prestage", "staged": True, "elapsed_ms": 24, "suite": "page-turn"},
            ]
            with log.open("w", encoding="utf-8") as f:
                for event in r1 + r2:
                    f.write(json.dumps(event) + "\n")

            args = argparse.Namespace(paths=[log], budgets=None, board=None, strict=False, all=True)
            ret = bench.run_report(args)
            self.assertEqual(ret, 0)
            printed = "\n".join(
                str(call.args[0]) for call in mock_print.call_args_list if call.args
            )
            self.assertIn("pooled runs contain multiple boards", printed)


if __name__ == "__main__":
    unittest.main()
