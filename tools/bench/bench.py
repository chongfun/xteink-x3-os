#!/usr/bin/env python3.14
"""Development bench harness for CalendulaOS hardware runs.

The harness deliberately starts as a serial log collector and parser. The
firmware has no interactive command channel, so hardware suites are guided:
the host tells the operator what workflow to perform, captures structured
`bench:` lines, and writes JSONL for repeatable reporting.
"""

from __future__ import annotations

import argparse
import errno
import json
import os
import re
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time
import tomllib
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TextIO

try:
    import fcntl
except ImportError:  # pragma: no cover - non-POSIX hosts cannot capture serial.
    fcntl = None  # type: ignore[assignment]


DEFAULT_PORT = "/dev/cu.usbmodem101"
DEFAULT_OUT = Path("target/bench/latest.jsonl")
DEFAULT_BUDGETS = Path(__file__).with_name("benches.toml")
DEFAULT_BUDGETS_X3 = Path(__file__).with_name("benches-x3.toml")
BOARD_BUDGETS: dict[str, Path] = {
    "x4": DEFAULT_BUDGETS,
    "x3": DEFAULT_BUDGETS_X3,
}


def resolve_budgets_path(
    budgets: Path | None,
    board: str | None = None,
    events: list[dict[str, Any]] | None = None,
) -> Path:
    if budgets is not None:
        return budgets
    if board and board in BOARD_BUDGETS:
        return BOARD_BUDGETS[board]
    if events:
        for event in events:
            b = event.get("board")
            if b in BOARD_BUDGETS:
                return BOARD_BUDGETS[b]
    return DEFAULT_BUDGETS


LEGACY_RENDER_RE = re.compile(
    r"bench: render (?P<view>\w+) (?P<mode>\w+) page=(?P<page>\d+) "
    r"ch=(?P<ch>\d+) layout=(?P<layout>\d+)ms flush=(?P<flush>\d+)ms "
    r"prestage=(?P<prestage>\d+)ms t=(?P<t>\d+)"
)
LEGACY_INPUT_RE = re.compile(
    r"input: (?P<button>Some\((?P<some>\w+)\)|None) gpio0=(?P<aux>\d+) "
    r"gpio1=(?P<nav>\d+) gpio2=(?P<page>\d+) t=(?P<t>\d+)"
)
REFRESH_BUSY_RE = re.compile(r"display: refresh busy (?P<busy>\d+) ms")
STORAGE_OPEN_RE = re.compile(
    r"storage: open complete status=(?P<status>\w+) pages=(?P<pages>\d+) "
    r"chapters=(?P<chapters>\d+)"
)
# The last line a bench-selftest scenario prints. Only a firmware built with
# that feature can emit it, so recognizing it changes nothing about any
# capture taken before or since from a shipped build. It exists because the
# firmware and the host count different things: folder-nav stops on completed
# round trips, and a selftest that picks twenty rows on a card whose rows
# are books produces none of them, so a count-only capture would wait for a
# target the device stopped being able to reach the moment its loop ended.
SELFTEST_DONE_RE = re.compile(
    r"bench-selftest: scenario=(?P<scenario>[a-z-]+) result=(?P<result>\S+)"
)
# A scenario reporting that it could not perform part of the workflow it
# advertises. Separate from the terminal `result=` record on purpose: the run
# continues, because the rest of a soak's sleep and wake is still evidence,
# and only the terminal record stops a capture. `--strict` fails on this, so a
# soak cannot skip its chapter navigation and still certify, which it could
# before: every other signal that suite checks for is produced by a jumpless
# pass.
SELFTEST_INVALID_RE = re.compile(
    r"bench-selftest: scenario=(?P<scenario>[a-z-]+) invalid=(?P<reason>\S+)"
)
# Which scenario the flashed image is actually about to run, printed once per
# boot before it touches anything. Worth checking against the workflow the
# operator asked for, because a stale image certifies the wrong suite in
# silence: a sleep-sync build captured as `reader-soak --strict` produces
# inputs, renders, completed sleeps and later wakes, which is the whole of
# what reader-soak's gate asks for, and a successful sleeping scenario writes
# no terminal record for the result check to catch.
# A scenario saying how many of its operations are fully decided, written
# after the last postcondition of the nth one has been checked. The count
# stop for a selftest capture keys on this rather than on the operation's own
# telemetry, because a paired render or a `folder_leave` precedes the
# firmware's verdict: a turn still has its quiet check ahead, a leave its
# depth wait and quiet check, and a host that stopped on the telemetry left
# the failing verdict unsent and the capture certified.
SELFTEST_COMPLETED_RE = re.compile(
    r"bench-selftest: scenario=(?P<scenario>[a-z-]+) completed=(?P<kind>[a-z_]+) count=(?P<count>\d+)"
)

# Stop-target events that a selftest certifies with a typed checkpoint. A
# target outside this set, sleep_complete today, keeps its telemetry
# predicate: the sleep is the operation, the reboot is its verdict, and no
# checkpoint could be written after it.
CHECKPOINTED_STOP_EVENTS = {"page_turn", "folder_leave"}
SELFTEST_START_RE = re.compile(r"bench-selftest: scenario=(?P<scenario>[a-z-]+) view=(?P<view>\S+)")

# Printed once per boot, before esp_rtos::start, so it carries no t_ms; it
# marks the start of a boot's time base and says how the chip woke.
DEEP_SLEEP_WAKE_RE = re.compile(
    r"main: deep_sleep_wake=(?P<wake>true|false) "
    r"\(gpio=(?P<gpio>true|false), sleep_image=(?P<image>true|false)\)"
)
# Boot-stage lines that carry a t_ms so boot-to-first-paint can be attributed
# to a stage rather than just totalled. Each must fire at most once per boot,
# or its median describes nothing. `sd: session enter` is deliberately NOT
# here: several SD sessions run before first paint (catalog load, book open,
# cache build), so it contributed multiple samples from one boot and its
# "median" marked no repeatable point. Filtering to stages ahead of the first
# render does not fix that — they are all ahead of it.
BOOT_STAGE_RE = re.compile(
    r"(?P<stage>main: spawn \w+|display: started|display: x3 init done)"
    r" t_ms=(?P<t>\d+)$"
)


@dataclass(frozen=True)
class Suite:
    name: str
    guidance: str
    stop_event: str | None = None
    stop_count_arg: str | None = None
    # What the count flag falls back to when the operator does not name one.
    # Held here rather than as an argparse default so the two cases stay
    # distinguishable: a count that was asked for is a contract, and a count
    # nobody typed is only a stopping rule.
    stop_count_default: int | None = None


SUITES = {
    "page-turn": Suite(
        "page-turn",
        "Open a warmed SD book, then press Next for the requested turn count.",
        # Turns, not Reading renders: the boot paint and any storage-driven
        # repaint is a Reading render that answered no press, and counting
        # those ended the capture one real sample short per repaint.
        stop_event="page_turn",
        stop_count_arg="turns",
        stop_count_default=50,
    ),
    "reader-soak": Suite(
        "reader-soak",
        "Run a normal reading workflow: page turns, chapter jumps, Home/Library returns, and a sleep/wake cycle.",
    ),
    "storage-cache": Suite(
        "storage-cache",
        "Exercise cold/warm catalog, book open, section extend, and progress-write paths.",
    ),
    "sleep-sync": Suite(
        "sleep-sync",
        "After several fast page turns, press Power or wait for idle sleep, then wake and repeat.",
        stop_event="sleep_complete",
        stop_count_arg="cycles",
        stop_count_default=10,
    ),
    "folder-nav": Suite(
        "folder-nav",
        "Enter and leave folders of varying size, and scroll across page boundaries.",
        # Entries, not renders: what this suite exists to time is the walk a
        # folder costs to enter, and a Library repaint that answered no press
        # is not one of those.
        # The completed round trip, not the entry. "Enter and leave folders"
        # is the workload, and stopping on the entry ended a capture with its
        # last leave still in flight, so the operation with its own storage
        # telemetry went unmeasured on the sample the operator asked for.
        stop_event="folder_leave",
        stop_count_arg="entries",
        stop_count_default=20,
    ),
    "thermal-run": Suite(
        "thermal-run",
        "Run the named underlying workflow while recording temperature/ambient notes in the run metadata.",
    ),
}


def require_pinned_python() -> None:
    """Fail with the version, not a traceback, on the wrong interpreter.

    A capture is run straight off the shebang -- `tools/bench/bench.py
    page-turn --port ...` -- so it never passes through `tools/check.sh`.
    Checked in `main` rather than at import so the module stays importable by
    the tests, which `check.sh` has already vetted.

    However many components `.python-version` names is how many are compared,
    so `3.14` accepts any release in that series and a fully-specified pin
    would not, without this having to know which is in force.

    Silent when the pin cannot be read: a copy of this file outside the repo
    has nothing to check itself against.
    """
    pin = Path(__file__).resolve().parents[2] / ".python-version"
    try:
        pinned = pin.read_text(encoding="utf-8").strip()
    except OSError:
        return
    running = ".".join(str(part) for part in sys.version_info[: len(pinned.split("."))])
    if running != pinned:
        series = ".".join(pinned.split(".")[:2])
        raise SystemExit(
            f"bench: this repo needs Python {pinned} (.python-version); this is "
            f"{running}. Install it (`uv python install {pinned}`) and re-run, "
            f"or invoke it explicitly: python{series} {sys.argv[0]}"
        )


def main() -> int:
    require_pinned_python()
    parser = argparse.ArgumentParser(prog="bench")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("list", help="list available suites")

    for name in SUITES:
        add_capture_parser(sub, name)

    stress = sub.add_parser("channel-stress", help="run host concurrency checks")
    stress.add_argument("--host", action="store_true", help="required; no hardware is used")
    stress.set_defaults(func=run_channel_stress)

    report = sub.add_parser("report", help="summarize one or more bench JSONL logs")
    report.add_argument("paths", nargs="+", type=Path)
    report.add_argument(
        "--budgets",
        type=Path,
        default=None,
        help="budgets TOML file to enforce (default: benches.toml)",
    )
    report.add_argument(
        "--board",
        choices=["x4", "x3"],
        default=None,
        help="board profile for budget selection (x3 selects benches-x3.toml)",
    )
    report.add_argument("--strict", action="store_true", help="exit non-zero on budget warnings")
    report.add_argument(
        "--all",
        action="store_true",
        help="pool every run in the log instead of only the latest",
    )
    report.set_defaults(func=run_report)

    args = parser.parse_args()
    if args.command == "list":
        print_suites()
        return 0
    return args.func(args)


def positive_int(text: str) -> int:
    """An argparse type for durations and counts, where zero is not "no limit".

    The capture loop asked `if seconds:`, so `--seconds 0` disabled the
    deadline and ran forever instead of being rejected; `--minutes 0` and
    `--cycles 0` inherited it. The only way to ask for no limit is to leave
    the flag off.
    """
    try:
        value = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r} is not an integer") from None
    if value <= 0:
        raise argparse.ArgumentTypeError(
            f"{value} is not a positive count or duration; omit the flag to "
            "capture without that limit"
        )
    return value


def add_capture_parser(sub: argparse._SubParsersAction[argparse.ArgumentParser], name: str) -> None:
    suite = SUITES[name]
    p = sub.add_parser(name, help=suite.guidance)
    p.add_argument("--port", default=DEFAULT_PORT)
    p.add_argument("--out", type=Path, default=DEFAULT_OUT)
    p.add_argument(
        "--seconds", type=positive_int, default=None, help="stop after this many seconds"
    )
    p.add_argument(
        "--reset-before",
        action="store_true",
        help="hard-reset the ESP32-C3 with espflash before capture",
    )
    p.add_argument("--espflash", default="espflash", help="espflash executable")
    p.add_argument("--strict", action="store_true", help="exit non-zero on budget warnings")
    p.add_argument(
        "--budgets",
        type=Path,
        default=None,
        help="budgets TOML file to enforce (default: benches.toml)",
    )
    p.add_argument(
        "--board",
        choices=["x4", "x3"],
        default=None,
        help="board profile for budget selection (x3 selects benches-x3.toml)",
    )
    p.add_argument("--note", action="append", default=[], help="free-form note stored in metadata")
    p.add_argument("--book", default=None, help="operator label for the book under test")
    if name == "page-turn":
        p.add_argument("--turns", type=positive_int, default=None, help="default 50; see --seconds")
    if name == "folder-nav":
        p.add_argument(
            "--entries", type=positive_int, default=None, help="default 20; see --seconds"
        )
    if name == "reader-soak":
        p.add_argument("--minutes", type=positive_int, default=30)
    if name == "sleep-sync":
        p.add_argument(
            "--cycles", type=positive_int, default=None, help="default 10; see --seconds"
        )
    if name == "storage-cache":
        p.add_argument(
            "--cold",
            action="store_true",
            help="the capture must exercise a cold storage path; --strict fails if it did not",
        )
        p.add_argument(
            "--warm",
            action="store_true",
            help="the capture must exercise a warm storage path; --strict fails if it did not",
        )
    if name == "thermal-run":
        p.add_argument("--suite", choices=["page-turn", "sleep-sync"], default="page-turn")
        p.add_argument("--minutes", type=positive_int, default=45)
    p.set_defaults(func=run_capture)


def print_suites() -> None:
    for name, suite in sorted(SUITES.items()):
        print(f"{name:14} {suite.guidance}")
    print("channel-stress  Host-only interleaving checks for queue/coalescing rules.")
    print("report          Summarize captured JSONL logs.")


MAX_PENDING_PRESTAGE_LINES = 5
DEFAULT_PENDING_PRESTAGE_TIMEOUT_S = 2.0


def process_capture_stream(
    lines: Iterable[str],
    suite_name: str,
    stop_target: tuple[str, int] | None,
    out: TextIO | None = None,
    print_lines: bool = True,
    event_callback: Callable[[dict[str, Any]], None] | None = None,
    pending_prestage_timeout_s: float = DEFAULT_PENDING_PRESTAGE_TIMEOUT_S,
    on_deadline_set: Callable[[float], None] | None = None,
) -> dict[str, int]:
    counts: dict[str, int] = {}
    turns = PageTurnCounter()
    pending_prestage = False
    pending_prestage_deadline: float | None = None
    pending_prestage_lines = 0
    # The last folder leave, waiting for the repaint that completes it. Its
    # own state and not the prestage one: the prestage wait deliberately
    # gives up on a line ceiling or a deadline and still calls the run
    # complete, which for a leave would certify the missing repaint the wait
    # exists to require.
    pending_leave_render = False
    pending_leave_deadline: float | None = None
    pending_leave_t_ms: int | None = None
    # A self-driven capture stops a checkpointed target only on the
    # firmware's typed `completed=` checkpoint. The banner is one line at
    # boot and a finite scenario works for minutes after it, so a capture
    # attached late sees no banner; every selftest record and every injected
    # press, which alone carries `action=`, marks the stream as self-driven,
    # and a press precedes the operation it drives, so the mark lands before
    # the operation's telemetry can reach the count. Checkpoints count from
    # where the capture joined, so a late attach owes N operations from there
    # rather than stopping on a total it did not watch accumulate. The first
    # checkpoint a late attach sees certifies an operation the capture may
    # have joined midway, so it is the baseline and not a sample; only a
    # capture that saw the announce has watched its first operation whole.
    selftest_seen = False
    checkpoints = CheckpointTally()

    for line in lines:
        if line != "":
            if print_lines:
                sys.stdout.write(line)
                sys.stdout.flush()
            parsed_events = parse_line(line, suite_name)
            for event in parsed_events:
                if out is not None:
                    write_event(out, event)
                if event_callback is not None:
                    event_callback(event)
                for counter in event_counters(event):
                    counts[counter] = counts.get(counter, 0) + 1
                turns.observe(event)
                counts["page_turn"] = turns.turns
                kind_name = str(event.get("event", ""))
                injected_press = kind_name == "input" and isinstance(event.get("action"), str)
                if not selftest_seen and (kind_name.startswith("selftest_") or injected_press):
                    selftest_seen = True
                    counts["selftest_seen"] = 1
                checkpoints.observe(event)
                if kind_name == "selftest_completed":
                    kind = str(event.get("kind", ""))
                    counts[f"selftest_completed:{kind}"] = checkpoints.completed(kind)
        else:
            parsed_events = []

        if pending_leave_render:
            # Only a render frozen after the leave completes it. The Back press
            # itself renders at once, showing the folder as Leaving with the
            # depth unchanged, and the SD leave prints its line in about 40 ms,
            # well inside that render's 400 ms flush. So the first render to
            # settle after `folder_leave` is usually the Back render, requested
            # before the leave, and accepting it ended the capture with the
            # parent listing still undrawn. `req_ms` is the freeze boundary
            # the harness already pairs presses on; the same boundary answers
            # here.
            if any(
                e.get("event") == "render"
                and isinstance(e.get("req_ms"), int)
                and pending_leave_t_ms is not None
                and e["req_ms"] > pending_leave_t_ms
                for e in parsed_events
            ):
                # Cleared before the break, or the post-loop check below reads
                # a satisfied wait as an unmet one.
                pending_leave_render = False
                break
            if pending_leave_deadline is not None and time.monotonic() >= pending_leave_deadline:
                # It did not. The SD work finished and nothing drew it, which
                # is a fault rather than a sample, so the run is marked and
                # `observed_stop_reason` refuses to call it complete. Marked
                # here and cleared, so the post-loop check counts a stream
                # that simply ended rather than adding a second mark.
                counts["folder_leave_unrendered"] = counts.get("folder_leave_unrendered", 0) + 1
                pending_leave_render = False
                break
        elif pending_prestage:
            has_prestage = any(e.get("event") == "prestage" for e in parsed_events)
            has_new_turn_or_render = any(
                e.get("event") in {"render", "input"} for e in parsed_events
            )
            if line != "":
                pending_prestage_lines += 1
            expired = (
                pending_prestage_deadline is not None
                and time.monotonic() >= pending_prestage_deadline
            )
            if (
                has_prestage
                or has_new_turn_or_render
                or pending_prestage_lines >= MAX_PENDING_PRESTAGE_LINES
                or expired
            ):
                break
        elif any(e.get("event") == "selftest_done" for e in parsed_events):
            # The device drives itself and has finished. Nothing further is
            # coming, so waiting out a duration or a count target only delays
            # the report.
            break
        elif (
            selftest_seen
            and stop_target
            and stop_target[0] in CHECKPOINTED_STOP_EVENTS
            and counts.get(f"selftest_completed:{stop_target[0]}", 0) >= stop_target[1]
        ):
            # The firmware has checked every postcondition of the nth
            # operation of this kind, so its verdict, an `invalid=` or a
            # terminal, is already in the log if there was one.
            break
        elif selftest_seen and stop_target and stop_target[0] in CHECKPOINTED_STOP_EVENTS:
            # Telemetry may already show the requested count, but a
            # self-driven capture stops a checkpointed target only on its
            # checkpoint. Keep reading. A target of another kind, the sleep
            # cycle, falls through to its telemetry predicate below.
            pass
        elif stop_target and counts.get(stop_target[0], 0) >= stop_target[1]:
            if stop_target[0] == "folder_leave" and not any(
                e.get("event") == "render" for e in parsed_events
            ):
                # `folder_leave` is printed by the storage call as soon as the
                # SD work is done, before its listing has reached the app,
                # been folded into state, or been drawn. Breaking there ends
                # the capture mid-round-trip on the very sample the operator
                # asked for, and hides a listing that arrived and then failed
                # to render. So the target leave waits for the repaint that
                # completes it, and only a repaint satisfies that wait.
                pending_leave_render = True
                pending_leave_t_ms = max(
                    (
                        e["t_ms"]
                        for e in parsed_events
                        if e.get("event") == "folder_leave" and isinstance(e.get("t_ms"), int)
                    ),
                    default=None,
                )
                deadline = time.monotonic() + pending_prestage_timeout_s
                pending_leave_deadline = deadline
                if on_deadline_set is not None:
                    on_deadline_set(deadline)
            elif stop_target[0] == "page_turn":
                already_prestaged = any(
                    e.get("event") == "render" and isinstance(e.get("prestage_ms"), int)
                    for e in parsed_events
                )
                if not already_prestaged:
                    pending_prestage = True
                    deadline = time.monotonic() + pending_prestage_timeout_s
                    pending_prestage_deadline = deadline
                    if on_deadline_set is not None:
                        on_deadline_set(deadline)
                    pending_prestage_lines = 0
                else:
                    break
            else:
                break

    if pending_leave_render:
        # The stream ended while the last leave still owed its repaint. Same
        # verdict as the deadline expiring: the count target is met on paper
        # and the round trip is not finished, so the run must not read as
        # complete. A port pulled mid-flush lands here too, which is the
        # honest answer for it.
        counts["folder_leave_unrendered"] = counts.get("folder_leave_unrendered", 0) + 1

    return counts


def run_capture(args: argparse.Namespace) -> int:
    suite = SUITES[args.command]
    seconds = capture_seconds(args)
    stop_target = stop_target_for(args, suite)
    args.out.parent.mkdir(parents=True, exist_ok=True)

    workflow = capture_workflow(args, suite)
    requested = capture_request(args, seconds, stop_target)
    print(f"bench {suite.name}: {suite.guidance}")
    if workflow != suite.name:
        # What the report will hold this run to, said before it starts.
        print(f"workflow: {workflow} (budgets and signal checks follow it)")
    print(f"port: {args.port}")
    print(f"out:  {args.out}")
    if stop_target and seconds is not None:
        # Both stop the capture, so say both. Printing only the duration —
        # the count message sat behind an `elif` — left the operator to
        # discover the count was still in force when the report faulted the
        # run for missing it.
        print(
            f"stop: after {stop_target[1]} parsed {stop_target[0]}(s) or {seconds}s, "
            "whichever comes first"
        )
        print(
            f"note: {stop_target[1]} {stop_target[0]}(s) is the contract; --seconds is "
            "a ceiling, and a capture it cuts short is reported as short"
        )
    elif stop_target:
        print(f"stop: after {stop_target[1]} parsed {stop_target[0]}(s)")
    elif seconds is not None:
        print(f"stop: after {seconds}s")
    else:
        print("stop: Ctrl-C")
    modes = requested.get("storage_modes")
    if modes:
        print(
            f"modes: {', '.join(modes)} — the capture must show each of these "
            "paths; --strict fails if one was never exercised"
        )
    if args.note:
        print("notes:", "; ".join(args.note))
    if args.reset_before:
        print("reset: hard-reset before capture")

    metadata = {
        "suite": suite.name,
        "workflow": workflow,
        "event": "run_start",
        "host_time": time.time(),
        "port": args.port,
        "notes": args.note,
        "book": getattr(args, "book", None),
        "reset_before": bool(args.reset_before),
        # What the operator asked this capture to collect, so the report can
        # say whether they got it. A capture cut short — by Ctrl-C, by a stop
        # rule that miscounted, or by a `--seconds` window that closed first —
        # otherwise looks exactly like a complete one. Always written, even
        # empty, because its presence is what tells the report this run
        # carries a completion contract at all.
        "requested": requested,
    }
    if getattr(args, "board", None):
        metadata["board"] = args.board
    counts: dict[str, int] = {}
    command_started = time.monotonic()
    # Reassigned once the device is back and the port is readable: a reset and
    # its re-enumeration are setup, not telemetry, and counting them against
    # `--seconds 20` collected materially less than 20 seconds while reporting
    # about 20 elapsed.
    started = command_started
    stop_at: float | None = None
    pending_deadline: float | None = None
    stop_reason = "stream-ended"

    def get_stop_at() -> float | None:
        if stop_at is not None and pending_deadline is not None:
            return min(stop_at, pending_deadline)
        return pending_deadline if pending_deadline is not None else stop_at

    def set_pending_deadline(deadline: float) -> None:
        nonlocal pending_deadline
        pending_deadline = deadline

    with args.out.open("a", encoding="utf-8") as out:
        write_event(out, metadata)
        try:
            if args.reset_before:
                reset_device(args.espflash, args.port)
            started = time.monotonic()
            stop_at = started + seconds if seconds is not None else None
            counts = process_capture_stream(
                capture_lines(args.port, stop_at=stop_at, get_stop_at=get_stop_at),
                suite.name,
                stop_target,
                out=out,
                print_lines=True,
                on_deadline_set=set_pending_deadline,
            )
            stop_reason = observed_stop_reason(counts, stop_target, stop_at)
        except KeyboardInterrupt:
            print("\nbench: capture stopped")
            # Ctrl-C *is* the stop condition when none was requested, so a
            # capture that asked for nothing else ends complete. One that did
            # ask was cut short, and must not read as though it finished.
            stop_reason = "operator" if seconds is None and stop_target is None else "interrupt"
        finally:
            write_event(
                out,
                {
                    "suite": suite.name,
                    "event": "run_end",
                    "host_time": time.time(),
                    # The telemetry window, which is what a requested duration
                    # is checked against; `command_elapsed_s` is the whole
                    # command, reset and re-enumeration included.
                    "elapsed_s": round(time.monotonic() - started, 3),
                    "command_elapsed_s": round(time.monotonic() - command_started, 3),
                    "stop_reason": stop_reason,
                    "completed": stop_reason in COMPLETED_STOP_REASONS,
                    "counts": counts,
                },
            )
    budgets_arg = getattr(args, "budgets", None)
    board_arg = getattr(args, "board", None)
    auto_board = budgets_arg is None and board_arg is None
    budgets_path = resolve_budgets_path(budgets_arg, board_arg) if not auto_board else None
    report_warnings = summarize_paths(
        [args.out],
        budgets_path,
        board=board_arg,
        auto_board=auto_board,
        validate_suites=args.strict,
    )
    return 1 if args.strict and report_warnings else 0


# Stop conditions that mean the capture collected what it was told to. The
# rest — an interrupted capture that had one, a stream that simply ended —
# leave a partial run `--strict` must not certify.
COMPLETED_STOP_REASONS = {"count", "duration", "operator", "selftest"}

# Stop events and the request key that names what the operator asked for.
STOP_EVENT_REQUEST_KEYS = {
    "page_turn": "page_turns",
    "sleep_complete": "sleep_cycles",
    # Still `--entries`, since that is what an operator asks for; what it
    # counts is completed enter-and-leave round trips.
    "folder_leave": "folder_entries",
}


def capture_request(
    args: argparse.Namespace,
    seconds: int | None,
    stop_target: tuple[str, int] | None,
) -> dict[str, Any]:
    """The contract a capture is held to: everything the operator asked for.

    Only the page-turn count was ever recorded, so `sleep-sync --cycles 10`,
    `reader-soak --minutes 30` and `thermal-run --minutes 45` could stop at
    the first valid signal and still satisfy `--strict` — proof that *some*
    expected telemetry occurred, not that the requested capture happened.
    `--cold`/`--warm` were worse: read once by argparse and never checked.

    A count and a duration are not both minimums. Whichever the operator
    typed is the contract (`stop_target_for` drops a defaulted count when a
    duration was named); with both named the count wins and `--seconds` is a
    ceiling. Recording both made `page-turn --seconds 60` unsatisfiable — the
    capture stops at whichever lands first, and the report faulted it for the
    other.
    """
    request: dict[str, Any] = {}
    count_key = STOP_EVENT_REQUEST_KEYS.get(stop_target[0]) if stop_target is not None else None
    if count_key is not None:
        request[count_key] = stop_target[1]
    elif seconds is not None:
        request["seconds"] = seconds
    modes = [mode for mode in STORAGE_MODES if getattr(args, mode, False)]
    if modes:
        request["storage_modes"] = modes
    return request


def observed_stop_reason(
    counts: dict[str, int],
    stop_target: tuple[str, int] | None,
    stop_at: float | None,
) -> str:
    """Why the capture stream ended, from what it actually reached."""
    if counts.get("folder_leave_unrendered", 0) > 0:
        # The requested leaves all happened and the last one was not drawn.
        # The count target is met on paper, so without this the run reads as
        # `count` and certifies the round trip it could not finish.
        return "leave-unrendered"
    if counts.get("selftest_done", 0) > 0:
        # A self-driving scenario ran to its own end. That is a stop
        # condition someone asked for, even when the suite's own count
        # target went unmet: on a flat card folder-nav produces no
        # `folder_leave` at all, and the run is complete regardless.
        return "selftest"
    if (
        stop_target is not None
        and counts.get("selftest_seen", 0) > 0
        and stop_target[0] in CHECKPOINTED_STOP_EVENTS
    ):
        # A self-driven capture's count for a checkpointed target is the
        # typed checkpoint, not the telemetry, which precedes the verdict on
        # the last operation.
        if counts.get(f"selftest_completed:{stop_target[0]}", 0) >= stop_target[1]:
            return "count"
    elif stop_target is not None and counts.get(stop_target[0], 0) >= stop_target[1]:
        return "count"
    if stop_at is not None and time.monotonic() >= stop_at:
        return "duration"
    # The device stopped talking and never came back, or the port was pulled:
    # neither is a stop condition anyone asked for.
    return "stream-ended"


def capture_workflow(args: argparse.Namespace, suite: Suite) -> str:
    """The workflow a capture actually ran, which is not always its suite.

    `thermal-run` is a condition, not a workload: `--suite` picks which of the
    other workflows runs under it. That choice decided what telemetry the run
    would produce and was then thrown away, so every thermal run looked alike
    to the report — a `--suite sleep-sync` capture was gated by the page-turn
    budgets and never asked for a sleep at all, which is the "selected and
    unchecked" shape this harness exists to catch.
    """
    underlying = getattr(args, "suite", None)
    return str(underlying) if isinstance(underlying, str) else suite.name


def reset_device(espflash: str, port: str) -> None:
    command = [
        espflash,
        "reset",
        "--chip",
        "esp32c3",
        "--port",
        port,
        "--non-interactive",
        "--after",
        "hard-reset",
    ]
    subprocess.run(command, check=True)


def press_action(event: dict[str, Any]) -> Any:
    """What a press meant: its logical action when the line carries one, else
    the physical key.

    An injected press logs both. `button=` is the physical key the reducer
    saw, which a swapped front pair or a flipped orientation maps to another
    action, and `action=` is what the scenario asked for. Pairing on the key
    alone made fifty genuine page turns invisible under PagesLeft, where the
    key that turns a page is Confirm. A manual capture has no `action=` and
    is read as it always was.
    """
    action = event.get("action")
    return action if isinstance(action, str) else event.get("button")


def event_counters(event: dict[str, Any]) -> list[str]:
    event_name = str(event.get("event", ""))
    counters = [event_name]
    if event_name == "render" and event.get("view") == "Reading":
        counters.append("reading_render")
    if is_completed_sleep_cycle(event):
        counters.append("sleep_complete")
    if event_name == "input" and press_action(event) in {"Next", "Previous"}:
        counters.append("page_input")
    return counters


def capture_seconds(args: argparse.Namespace) -> int | None:
    if args.seconds is not None:
        return args.seconds
    if args.command in {"reader-soak", "thermal-run"}:
        return int(args.minutes) * 60
    return None


def stop_target_for(args: argparse.Namespace, suite: Suite) -> tuple[str, int] | None:
    """The count this capture stops on, if a count is what bounds it.

    A count the operator typed is theirs, and `--seconds` is a ceiling over
    it. A count they did not type is only this suite's default and must not
    outrank a duration they did: holding `page-turn --seconds 60` to 50 turns
    reported almost every time-boxed capture as short of a sample count nobody
    asked for. So a named duration with no named count drops the count from
    both the stopping rule and the contract.
    """
    if suite.stop_event is None or suite.stop_count_arg is None:
        return None
    requested = getattr(args, suite.stop_count_arg, None)
    if requested is None:
        if getattr(args, "seconds", None) is not None:
            return None
        requested = suite.stop_count_default
    if requested is None:
        return None
    return suite.stop_event, int(requested)


# Errno values a vanishing USB-serial device raises: the ESP32-C3's
# USB-JTAG port drops off the bus when the firmware enters deep sleep
# (idle timeout, the sleep-cycle suites). macOS reports ENXIO ("Device
# not configured"), Linux EIO or ENODEV; ENOENT covers a reopen racing
# re-enumeration.
PORT_LOST_ERRNOS = {errno.ENXIO, errno.EIO, errno.ENODEV, errno.ENOENT}


def capture_lines(
    port: str,
    stop_at: float | None = None,
    get_stop_at: Callable[[], float | None] | None = None,
) -> Iterable[str]:
    """`serial_lines`, surviving the device dropping off the bus.

    Deep sleep mid-capture is expected — an idle timeout or a
    sleep-cycle suite kills the USB-JTAG port — so rather than dying
    with a traceback, announce the loss, wait for the port to
    re-enumerate (waking the device is the operator's job), and resume
    until the capture window closes. A port that never produced a line
    still fails fast, so a mistyped --port errors immediately.
    """
    connected = False
    reconnecting = False
    while True:
        if reconnecting:
            effective_stop_at = get_stop_at() if get_stop_at is not None else stop_at
            if effective_stop_at is not None and time.monotonic() >= effective_stop_at:
                print("port: capture window ended while the device was away", flush=True)
                return
            if not os.path.exists(port):
                time.sleep(0.5)
                continue
            # Let enumeration settle before reopening the fresh device node.
            time.sleep(0.5)

        try:
            for line in serial_lines(port, stop_at=stop_at, get_stop_at=get_stop_at):
                if line == "":
                    if reconnecting:
                        print("port: back; resuming capture", flush=True)
                        reconnecting = False
                    continue
                connected = True
                yield line
        except OSError as err:
            if not connected or err.errno not in PORT_LOST_ERRNOS:
                raise
            if not reconnecting:
                print(
                    f"port: {port} vanished (device asleep?); wake it to resume capture", flush=True
                )
                reconnecting = True
        else:
            return


def serial_lines(
    port: str,
    stop_at: float | None = None,
    get_stop_at: Callable[[], float | None] | None = None,
) -> Iterable[str]:
    if fcntl is None:
        raise RuntimeError("serial capture requires POSIX fcntl support")
    fd = os.open(port, os.O_RDONLY | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        attrs = termios.tcgetattr(fd)
        attrs[0] = 0
        attrs[1] = 0
        attrs[2] = termios.CREAD | termios.CLOCAL | termios.CS8
        attrs[3] = 0
        termios.tcsetattr(fd, termios.TCSANOW, attrs)
        fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack("i", termios.TIOCM_DTR))
        fcntl.ioctl(fd, termios.TIOCMBIC, struct.pack("i", termios.TIOCM_RTS))
        yield ""
        buf = b""
        while True:
            effective_stop_at = get_stop_at() if get_stop_at is not None else stop_at
            timeout = 0.2
            if effective_stop_at is not None:
                remaining = effective_stop_at - time.monotonic()
                if remaining <= 0:
                    return
                timeout = min(timeout, remaining)
            ready, _, _ = select.select([fd], [], [], timeout)
            if not ready:
                continue
            chunk = os.read(fd, 4096)
            if not chunk:
                raise OSError(errno.EIO, "EOF on serial port")
            buf += chunk
            while b"\n" in buf:
                raw, buf = buf.split(b"\n", 1)
                yield raw.decode("utf-8", errors="replace") + "\n"
    finally:
        os.close(fd)


def parse_line(line: str, suite: str = "unknown") -> list[dict[str, Any]]:
    text = line.strip()

    match = LEGACY_RENDER_RE.match(text)
    if match:
        data = match.groupdict()
        return [
            {
                "suite": suite,
                "event": "render",
                "view": data["view"],
                "mode": data["mode"],
                "page": int(data["page"]),
                "chapter": int(data["ch"]),
                "layout_ms": int(data["layout"]),
                "flush_ms": int(data["flush"]),
                "prestage_ms": int(data["prestage"]),
                "t_ms": int(data["t"]),
                "legacy": True,
            }
        ]

    match = SELFTEST_COMPLETED_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "selftest_completed",
                "scenario": match.group("scenario"),
                "kind": match.group("kind"),
                "count": int(match.group("count")),
            }
        ]

    match = SELFTEST_START_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "selftest_start",
                "scenario": match.group("scenario"),
                "view": match.group("view"),
            }
        ]

    match = SELFTEST_INVALID_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "selftest_invalid",
                "scenario": match.group("scenario"),
                "reason": match.group("reason"),
            }
        ]

    match = SELFTEST_DONE_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "selftest_done",
                "scenario": match.group("scenario"),
                "result": match.group("result"),
            }
        ]

    if text.startswith("bench: "):
        return [parse_bench_line(text, suite)]

    match = LEGACY_INPUT_RE.match(text)
    if match:
        button = match.group("some") or "None"
        return [
            {
                "suite": suite,
                "event": "input",
                "button": button,
                "aux": int(match.group("aux")),
                "nav": int(match.group("nav")),
                "page_raw": int(match.group("page")),
                "t_ms": int(match.group("t")),
                "legacy": True,
            }
        ]

    match = REFRESH_BUSY_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "refresh",
                "busy_ms": int(match.group("busy")),
                "legacy": True,
            }
        ]

    if text == "display: sleep deep":
        return [{"suite": suite, "event": "sleep", "phase": "deep", "legacy": True}]
    # `display: sleep framebuffer flush failed` is deliberately NOT parsed.
    # The firmware prints it beside `bench: sleep phase=refresh ok=false`, and
    # has since both were added, so parsing it recorded every failed flush
    # twice and the report said "2 failed sleep phase(s)" for one failure. The
    # println stays in the firmware because it is an error path, and error
    # paths are unconditional there (see fw::log) — it is the only word a
    # `--no-default-features` build says about a failed sleep. Its sibling
    # `display: sleep transition failed` was never parsed either.
    if "queue full" in text or "panicked at" in text or "watchdog" in text.lower():
        return [{"suite": suite, "event": "warning", "line": text}]

    match = STORAGE_OPEN_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "storage_open",
                "status": match.group("status"),
                "pages": int(match.group("pages")),
                "chapters": int(match.group("chapters")),
                "legacy": True,
            }
        ]

    match = DEEP_SLEEP_WAKE_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "boot",
                "deep_sleep_wake": match.group("wake") == "true",
                "gpio": match.group("gpio") == "true",
                "sleep_image": match.group("image") == "true",
            }
        ]

    match = BOOT_STAGE_RE.match(text)
    if match:
        return [
            {
                "suite": suite,
                "event": "boot_stage",
                "stage": match.group("stage"),
                "t_ms": int(match.group("t")),
            }
        ]
    return []


def parse_bench_line(text: str, suite: str) -> dict[str, Any]:
    body = text.removeprefix("bench: ").strip()
    if not body:
        return {"suite": suite, "event": "unknown", "line": text}
    parts = body.split()
    event = parts[0].replace("-", "_")
    result: dict[str, Any] = {"suite": suite, "event": event}
    for part in parts[1:]:
        if "=" not in part:
            result.setdefault("tokens", []).append(part)
            continue
        key, raw = part.split("=", 1)
        result[key] = parse_value(raw)
    return result


def parse_value(raw: str) -> Any:
    value = raw.strip().rstrip(",")
    if value.startswith("Some(") and value.endswith(")"):
        return value[5:-1]
    if value.endswith("ms") and value[:-2].isdigit():
        return int(value[:-2])
    if value in {"true", "false"}:
        return value == "true"
    if value in {"ok", "fail"}:
        return value == "ok"
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    try:
        return float(value)
    except ValueError:
        return value


def write_event(out: Any, event: dict[str, Any]) -> None:
    out.write(json.dumps(event, sort_keys=True, separators=(",", ":")) + "\n")
    out.flush()


def run_report(args: argparse.Namespace) -> int:
    budgets_arg = getattr(args, "budgets", None)
    board_arg = getattr(args, "board", None)
    auto_board = budgets_arg is None and board_arg is None
    budgets_path = resolve_budgets_path(budgets_arg, board_arg) if not auto_board else None
    report_warnings = summarize_paths(
        args.paths,
        budgets_path,
        board=board_arg,
        auto_board=auto_board,
        validate_suites=args.strict,
        latest_only=not getattr(args, "all", False),
    )
    return 1 if args.strict and report_warnings else 0


def split_runs(events: list[dict[str, Any]]) -> list[list[dict[str, Any]]]:
    """Splits a pooled event stream at its run_start markers.

    Captures append to the same log, so a file usually holds several
    runs; events before the first marker (hand-assembled logs) form
    their own leading segment.
    """
    runs: list[list[dict[str, Any]]] = [[]]
    for event in events:
        if event.get("event") == "run_start" and runs[-1]:
            runs.append([])
        runs[-1].append(event)
    return [run for run in runs if run]


def summarize_paths(
    paths: list[Path],
    budgets_path: Path | None = None,
    *,
    board: str | None = None,
    auto_board: bool = False,
    validate_suites: bool = False,
    latest_only: bool = True,
) -> list[str]:
    events: list[dict[str, Any]] = []
    for path in paths:
        file_events = read_events(path)
        if events and file_events and file_events[0].get("event") != "run_start":
            # Two files are two captures, but the event stream was simply
            # concatenated, so a log that opens without a `run_start` — every
            # capture predating the marker, and every hand-built one — had no
            # boundary and was swallowed by whichever run preceded it. Its
            # samples then inherited that run's suite and its device time base,
            # both wrong, and both decided by the order of the paths on the
            # command line. The marker is synthesized in memory only; it
            # carries no suite, so the run stays unlabelled and says so.
            events.append({"event": "run_start", "file_boundary": str(path)})
        events.extend(file_events)

    if not events:
        print("bench report: no events")
        return ["no events parsed"] if validate_suites else []

    runs = split_runs(events)
    if latest_only and len(runs) > 1:
        events = runs[-1]
        start = next((e for e in events if e.get("event") == "run_start"), {})
        print(
            f"bench report: latest run only ({start.get('suite', 'unknown')}; "
            f"{len(runs) - 1} earlier run(s) in the log — pass --all to pool)"
        )
    # Boot detection and t_ms monotonicity are per run: pooled runs restart
    # the device time base at every run_start, which is not a reset.
    scoped_runs = runs[-1:] if latest_only else runs
    boot_paints, boot_stages, time_warnings = boot_report(scoped_runs)

    if budgets_path is None and board is not None:
        budgets_path = BOARD_BUDGETS.get(board, DEFAULT_BUDGETS)
    elif budgets_path is None and auto_board:
        if latest_only:
            budgets_path = resolve_budgets_path(None, None, events)
        else:
            run_boards = set()
            for run in scoped_runs:
                b = next(
                    (e.get("board") for e in run if e.get("board") in BOARD_BUDGETS),
                    "unspecified",
                )
                run_boards.add(b)
            if len(run_boards) == 1:
                b = next(iter(run_boards))
                budgets_path = BOARD_BUDGETS.get(b, DEFAULT_BUDGETS)
            else:
                board_names = ", ".join(sorted(run_boards))
                if validate_suites:
                    raise SystemExit(
                        f"bench report: --strict cannot evaluate pooled runs from multiple boards ({board_names}); specify --board or --budgets"
                    )
                print(
                    f"bench report: warning: pooled runs contain multiple boards ({board_names}); specify --board or --budgets to enforce budgets"
                )
                budgets_path = None

    # `validate_suites` is the --strict flag. A strict gate that cannot load
    # its budgets must fail loudly: exiting 0 with the checks silently absent
    # is how a 16.7x overrun once passed clean.
    budgets, budgets_problem = load_budgets(budgets_path)
    if budgets_problem is not None:
        if validate_suites:
            raise SystemExit(f"bench report: --strict cannot enforce budgets: {budgets_problem}")
        print(f"bench report: warning: budgets not checked: {budgets_problem}")

    renders = [event for event in events if event.get("event") == "render"]
    reading_renders = [event for event in renders if event.get("view") == "Reading"]
    refreshes = [event for event in events if event.get("event") == "refresh"]
    sleeps = [event for event in events if event.get("event") == "sleep"]
    warnings = [event for event in events if event.get("event") == "warning"]
    storage = [event for event in events if str(event.get("event", "")).startswith("storage")]

    print("\nbench report")
    # Synthesized file boundaries are structure, not telemetry; counting them
    # would report more events than the logs hold.
    print(f"events:        {sum(1 for e in events if 'file_boundary' not in e)}")
    print(f"renders:       {len(renders)}")
    print(f"storage:       {len(storage)}")
    print(f"sleeps:        {len(sleeps)}")
    print(f"warnings:      {len(warnings)}")
    print_duration("reading layout", values(reading_renders, "layout_ms"))
    pool = page_turn_pool(labelled_runs(events))
    turn_stats = pool.every
    for label, stats in pool.untrusted:
        print(
            f"page turn      EXCLUDED {label}: {page_turn_exclusion(stats)} — "
            "left out of the page turn figure below"
        )
    if pool.trusted.durations:
        print_duration("page turn", pool.trusted.durations)
    elif turn_stats.durations:
        print("page turn      no run paired at a trustworthy cadence; median suppressed")
    if turn_stats.presses:
        print(
            f"page inputs:   presses={turn_stats.presses} "
            f"page_turns={len(turn_stats.durations)} "
            f"nav={turn_stats.nav_answered} "
            f"coalesced={turn_stats.coalesced_presses} "
            f"unmatched={turn_stats.unmatched_presses} "
            f"reading_renders={turn_stats.reading_renders}"
        )
    print_duration("render flush", values(renders, "flush_ms"))
    print_duration("prestage", prestage_values(events))
    print_duration("refresh busy", values(refreshes, "busy_ms"))
    # One line per path, never a pooled median — the same rule the boot report
    # follows, for the same reason: a cache build and a RAM hit are different
    # work by three orders of magnitude, and their mixture is a number
    # matching no open that ever happened.
    opens = storage_open_kinds(events)
    for kind in STORAGE_OPEN_KINDS:
        print_duration(f"storage open ({kind})", values(opens[kind], "elapsed_ms"))
    # The same population the budgets measure, so the printed figure and the
    # gated one cannot disagree about what was sampled. What is left out is
    # named below rather than hidden.
    print_duration("catalog load", values(catalog_samples(events, "load"), "elapsed_ms"))
    print_duration("catalog scan", values(catalog_samples(events, "scan"), "elapsed_ms"))
    for action in ("load", "scan"):
        excluded = [
            event
            for event in catalog_events(events, action)
            if not catalog_succeeded(event) and not unverifiable_catalog_op(event)
        ]
        if not excluded:
            continue
        if action == "scan":
            reasons = [f"{len(excluded)} failed"]
        else:
            faults: dict[str, int] = {}
            for event in excluded:
                faults[str(event.get("result", "not loaded"))] = (
                    faults.get(str(event.get("result", "not loaded")), 0) + 1
                )
            reasons = [
                f"{count} "
                + CATALOG_LOAD_REASONS.get(result, f"reported an unrecognised result ({result})")
                for result, count in sorted(faults.items())
            ]
        print(f"catalog {action}:  " + ", ".join(reasons) + ", left out of the figure above")
    print_duration(
        "progress write",
        values(
            [
                event
                for event in events
                if event.get("event") == "storage_progress" and event.get("elapsed_ms") is not None
            ],
            "elapsed_ms",
        ),
    )
    builds = [event for event in events if event.get("event") == "storage_build"]
    print_duration("storage build", values(builds, "elapsed_ms"))
    print_duration("build spine", values(builds, "spine_ms"))
    print_duration("build write", values(builds, "write_ms"))
    if builds:
        totals = {
            key: sum(int(event[key]) for event in builds if isinstance(event.get(key), int))
            for key in ("rd_calls", "rd_blocks", "wr_calls", "wr_blocks")
        }
        print(
            f"build io:      builds={len(builds)} "
            + " ".join(f"{key}={value}" for key, value in totals.items())
        )
    print_duration(
        "first page",
        values(
            [event for event in events if event.get("event") == "storage_first_page"],
            "elapsed_ms",
        ),
    )
    print_duration(
        "bg build",
        values(
            [event for event in events if event.get("event") == "storage_background_build"],
            "elapsed_ms",
        ),
    )
    # One line per boot kind, never a pooled median: a cold boot pays the full
    # waveform and a wake does not, so their mixture is a number matching no
    # boot that ever happened — and the mixing ratio is decided by the suite
    # (sleep-sync is ~1 cold to 20 wakes, --reset-before is all cold).
    for kind in sorted(boot_paints):
        print_duration(f"boot to paint ({kind})", boot_paints[kind])
    if boot_paints:
        print(
            "boots:         "
            + " ".join(f"{kind}={len(boot_paints[kind])}" for kind in sorted(boot_paints))
            + " (first render t_ms per witnessed boot; reset = unexplained reboot)"
        )
    if boot_stages:
        print("boot stages:   (median t_ms across witnessed boots)")
        for stage, samples in sorted(boot_stages.items(), key=lambda kv: statistics.median(kv[1])):
            print(f"  {stage:24} {statistics.median(samples):.0f}ms (n={len(samples)})")

    modes: dict[str, int] = {}
    for event in renders:
        mode = str(event.get("mode", "unknown"))
        modes[mode] = modes.get(mode, 0) + 1
    if modes:
        print("refresh modes: " + ", ".join(f"{k}={v}" for k, v in sorted(modes.items())))
    if warnings:
        print("warning lines:")
        for event in warnings[:10]:
            print(f"  {event.get('line', event)}")
    budget_warnings = evaluate_budgets(events, budgets)
    suite_warnings = evaluate_suite_signals(events) if validate_suites else []
    if time_warnings:
        print("time warnings:")
        for warning in time_warnings:
            print(f"  {warning}")
    if budget_warnings:
        print("budget warnings:")
        for warning in budget_warnings:
            print(f"  {warning}")
    if suite_warnings:
        print("suite warnings:")
        for warning in suite_warnings:
            print(f"  {warning}")
    return time_warnings + budget_warnings + suite_warnings


def read_events(path: Path) -> list[dict[str, Any]]:
    result = []
    with path.open(encoding="utf-8") as handle:
        for line_no, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                result.append(json.loads(line))
            except json.JSONDecodeError as err:
                raise SystemExit(f"{path}:{line_no}: invalid JSONL: {err}") from err
    return result


def values(events: list[dict[str, Any]], key: str) -> list[int]:
    return [int(event[key]) for event in events if isinstance(event.get(key), int)]


def refresh_busy_values(events: list[dict[str, Any]], mode: str) -> list[int]:
    return values(
        [
            event
            for event in events
            if event.get("event") == "refresh" and event.get("mode") == mode
        ],
        "busy_ms",
    )


# The three populations a `storage_open` can belong to. They are three
# different pieces of work, and on this repo's own captures they do not
# overlap at all: RAM hits run 0-15 ms, warm opens 57-95 ms, cold opens
# 14-64 seconds. Pooling them produced a "storage open p95" that described
# none of them, and let a deliberately cold open decide the *warm* budget.
#
#   ram   the requested page was inside the section window already loaded, so
#         nothing touched the card (`ram_hit=true`).
#   warm  the card was read but the book's cache was already built, so the
#         open loaded it. This is the population `warm_book_open_warn_ms`
#         describes.
#   cold  the cache had to be built first, which the firmware announces as a
#         `storage_build` inside the same transaction. Book-size dependent by
#         construction, so it is reported and never gated.
STORAGE_OPEN_KINDS = ("ram", "warm", "cold")


def storage_open_kinds(events: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    """Split `storage_open` events into the three paths they can take.

    Cold is decided positionally, because `storage_build` carries no request
    id or `t_ms` to join on: the firmware prints it from inside the open's
    `LoadSection` step, so a build seen since the previous open belongs to it.
    Only events carrying a boolean `ram_hit` close an open — the legacy
    `storage: open complete` line parses to a `storage_open` too and would
    otherwise consume the build belonging to the structured event right after
    it. Those legacy lines land in no kind; they carry no `elapsed_ms` either,
    so no budget can be satisfied by an event that measured nothing.

    Three things clear a pending build. `storage_background_build`, because a
    background walk's last step publishes through the same `report_publish`
    with no open in flight — reliably paired, since `report_publish` fires
    only on the `Ready` outcome that makes the step `Finished`. And
    `run_start`/`boot`, because `--all` concatenates captures and a build at
    the end of one run would be charged to the first open of the next. The
    cost is the same either way: a real warm sample filed as a 14-64 second
    cold one, out of the budget and out of `--warm` evidence.
    """
    kinds: dict[str, list[dict[str, Any]]] = {kind: [] for kind in STORAGE_OPEN_KINDS}
    built = False
    for event in events:
        name = event.get("event")
        if name in {"run_start", "boot", "storage_background_build"}:
            built = False
        elif name == "storage_build":
            built = True
        elif name == "storage_open" and isinstance(event.get("ram_hit"), bool):
            if event["ram_hit"]:
                kinds["ram"].append(event)
            else:
                kinds["cold" if built else "warm"].append(event)
            built = False
    return kinds


def storage_open_values(events: list[dict[str, Any]], kind: str) -> list[int]:
    return values(storage_open_kinds(events)[kind], "elapsed_ms")


# What `storage-cache --cold` / `--warm` are asking the operator to exercise,
# and the telemetry that shows they did. bench.py only listens — the operator
# drives the device — so the flags cannot steer the capture; they declare an
# intent the report then holds the capture to. Before this they were read once
# by argparse and never written down or checked, so `--cold --warm --strict`
# passed without proving either path.
STORAGE_MODES = ("cold", "warm")

STORAGE_MODE_EVIDENCE = {
    "cold": "a book open that had to build its cache, or a catalog scan that succeeded",
    "warm": (
        "a book open served from an already-built cache or from the loaded "
        "RAM window, or a catalog loaded from its snapshot"
    ),
}


def catalog_events(events: list[dict[str, Any]], action: str) -> list[dict[str, Any]]:
    return [
        event
        for event in events
        if event.get("event") == "storage_catalog" and event.get("action") == action
    ]


# A catalog load reports three outcomes its original `ok` could not tell
# apart, and the difference decides whether a capture is looking at a fault or
# at the ordinary cold path.
#
#   hit      the snapshot loaded.
#   miss     there was nothing to load — no catalog directory, or no file in
#            it. This is *normal*: it is what makes the firmware queue
#            `RefreshCatalog`, so a card whose catalog has not been built yet
#            prints one immediately before the scan that builds it. Reading it
#            as a failure faulted a healthy cold boot, which `--reset-before`
#            makes the common case.
#   stale    the file was written by firmware of another catalog version.
#            Bumping `CATALOG_VERSION` is how that format migrates — the old
#            snapshot stops loading and the scan rebuilds it — so this is the
#            designed first boot after an upgrade, not a fault.
#   invalid  the file was there and did not check out: wrong magic, the
#            version-0 placeholder an interrupted scan leaves behind, a length
#            disagreeing with its header, or a record that ended early. The
#            card answered and what it held was unusable, which is a finding.
#   error    the card refused an open, a seek, or a read.
#   reclaimed  the load was abandoned on purpose: recovery removed an
#            interrupted upload, so any catalog written before it may name a
#            file that is now gone. The rescan that follows is the repair.
#   unreconciled  recovery could not finish, so nothing established what the
#            shelf holds. Where a miss, a stale snapshot and a reclaimed one
#            are all answered by the scan that follows, this one is not: that
#            scan refuses to rebuild for the same reason, so the reader has no
#            library until a mount where recovery settles. A finding.
#
# `miss` is deliberately the narrowest of them: the firmware used to
# reduce the whole read to a bool inside the SD session, so a refused read, a
# failed seek and a torn file all surfaced as that benign one.
CATALOG_LOAD_RESULTS = {
    "hit",
    "miss",
    "stale",
    "invalid",
    "error",
    "reclaimed",
    "unreconciled",
}

# The results that mean something went wrong. `miss`, `stale` and `reclaimed`
# do not: a catalog not built yet, one the firmware has outgrown, and one
# retired because recovery deleted a file it might name, each answered by the
# same scan.
CATALOG_LOAD_FAULTS = {"invalid", "error", "unreconciled"}

# How the report names each one, so a miss does not read as a fault.
CATALOG_LOAD_REASONS = {
    "miss": "found no snapshot (normal cold path)",
    "stale": "found an older catalog version (rebuilt by the scan)",
    "invalid": "found an unusable snapshot",
    "error": "card error",
    "reclaimed": "retired after an interrupted upload was reclaimed (rebuilt by the scan)",
    "unreconciled": "refused because storage recovery did not finish (the scan refuses too)",
    "not loaded": "did not load (older firmware: reason not recorded)",
}


def catalog_load_hit(event: dict[str, Any]) -> bool:
    """The snapshot loaded — the only load that evidences the warm path.

    Keyed on the field being *present*, not on its value being a string:
    falling back to `ok` for any non-string made `{"ok": true, "result": null}`
    read as a confirmed hit. Only a line with no `result` at all is old enough
    for `ok` to be the whole story.
    """
    if "result" in event:
        return event["result"] == "hit"
    return event.get("ok") is True


def catalog_load_error(event: dict[str, Any]) -> bool:
    """The read went wrong, as opposed to there being nothing to hand over.

    Asserted only from `result`: a pre-`result` capture's `ok=false` covers
    every outcome at once, and the miss is by far the common one.
    """
    return event.get("result") in CATALOG_LOAD_FAULTS


def unknown_catalog_result(event: dict[str, Any]) -> bool:
    """A `result=` this bench.py has no meaning for.

    A typo, a token some future firmware adds, or a value that did not parse
    as a string. Not a hit, not a recognised fault, and not result-less, so
    every other predicate answered "no" about it — which is how it slipped
    through `--strict` in silence beside a valid `hit` in the same run.
    Reported rather than guessed at, as an unrecognised workflow is.
    """
    if event.get("event") != "storage_catalog" or "result" not in event:
        return False
    result = event["result"]
    return not isinstance(result, str) or result not in CATALOG_LOAD_RESULTS


def unverifiable_catalog_op(event: dict[str, Any]) -> bool:
    """A catalog line that does not say how the operation went.

    In practice a scan from firmware older than the scan line's `ok`, which is
    every capture predating it: `status` could not answer the question,
    because the firmware replaces a failed scan's `Error` with `Ready` while
    an older in-memory catalog is still listed.
    """
    if event.get("event") != "storage_catalog":
        return False
    # Either field *present* means the line said something. Whether it is
    # readable is `unknown_catalog_result`'s business, not grounds to pool the
    # line here as though it were merely old.
    return "result" not in event and not isinstance(event.get("ok"), bool)


def catalog_succeeded(event: dict[str, Any]) -> bool:
    """Confirmed success, which is what strict evidence is allowed to rest on."""
    if event.get("action") == "load":
        return catalog_load_hit(event)
    return event.get("ok") is True


def catalog_samples(events: list[dict[str, Any]], action: str) -> list[dict[str, Any]]:
    """The operations whose duration describes the path a budget is about.

    Confirmed successes, plus lines too old to say — the one place that
    compatibility assumption is applied. A budget asks how long the working
    path took, so a legacy line is read as it always was; strict *evidence*
    demands proof instead (`storage_mode_evidence`), because proving a path
    ran is a claim rather than a measurement. Confirmed non-successes are out
    of both: a miss and a refused session return in a fraction of the time,
    so pooling them measures how fast the card said no.
    """
    return [
        event
        for event in catalog_events(events, action)
        if catalog_succeeded(event) or unverifiable_catalog_op(event)
    ]


def failed_storage_ops(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Storage operations the firmware reported as genuinely failed.

    A catalog load is judged on `result` alone: its `ok=false` is the normal
    cold path — no snapshot yet — far more often than it is a fault.
    Everything else storage-side means what `ok=false` says.
    """
    failed = []
    for event in events:
        if not str(event.get("event", "")).startswith("storage"):
            continue
        if event.get("event") == "storage_catalog" and event.get("action") == "load":
            if catalog_load_error(event):
                failed.append(event)
        elif event.get("ok") is False:
            failed.append(event)
    return failed


# What `storage_mode_evidence` found. A mode never taken and one witnessed
# only by telemetry too old to say both fail `--strict`, but the operator
# fixes them differently: capture it, or reflash.
STORAGE_MODE_ABSENT = "absent"
STORAGE_MODE_UNVERIFIED = "unverified"
STORAGE_MODE_CONFIRMED = "confirmed"


def storage_mode_evidence(events: list[dict[str, Any]], mode: str) -> str:
    """How well this capture proves it took the storage path it was asked to.

    Strict evidence rests only on confirmed success. Nothing regresses by
    demanding it — a capture whose firmware predates the result fields never
    had its mode verified anyway — and it is the case that matters, since the
    *host* records `requested.storage_modes` whatever firmware is on the
    device.
    """
    opens = storage_open_kinds(events)
    if mode == "cold":
        confirming = bool(opens["cold"]) or any(
            catalog_succeeded(event) for event in catalog_events(events, "scan")
        )
        unverifiable = any(
            unverifiable_catalog_op(event) for event in catalog_events(events, "scan")
        )
    elif mode == "warm":
        confirming = bool(opens["warm"] or opens["ram"]) or any(
            catalog_load_hit(event) for event in catalog_events(events, "load")
        )
        unverifiable = any(
            unverifiable_catalog_op(event) for event in catalog_events(events, "load")
        )
    else:
        return STORAGE_MODE_ABSENT
    if confirming:
        return STORAGE_MODE_CONFIRMED
    return STORAGE_MODE_UNVERIFIED if unverifiable else STORAGE_MODE_ABSENT


def print_duration(label: str, data: list[int]) -> None:
    if not data:
        return
    median = statistics.median(data)
    p95 = percentile(data, 95)
    print(f"{label:14} median={median:.0f}ms p95={p95:.0f}ms min={min(data)}ms max={max(data)}ms")


def prestage_values(events: list[dict[str, Any]]) -> list[int]:
    """Prestage durations, from either or both firmware event shapes.

    Firmware from 2026-07-27 emits prestage as its own event, after the
    render event that ends press-to-settled. Older firmware folded
    `prestage_ms` into the render line, which is also why its `page turn`
    figures run ~24 ms long — the render timestamp was printed after the
    prestage rather than at the settle. Captures from before that change
    still summarize, but do not compare their `page turn` against a newer
    run's.
    """
    result: list[int] = []
    for event in events:
        if event.get("event") == "prestage" and isinstance(event.get("elapsed_ms"), int):
            result.append(int(event["elapsed_ms"]))
        elif event.get("event") == "render" and isinstance(event.get("prestage_ms"), int):
            result.append(int(event["prestage_ms"]))
    return result


# Fraction of page presses that may fail to produce a page-turn sample before
# the median is untrustworthy. Two ways a press produces no sample, and both
# mean the same thing — the capture is recording tapping rhythm rather than
# firmware latency:
#
#   unmatched  no render ever answered it (the firmware logs every debounced
#              press whether or not the reader acts on it, and the input
#              channel drops the oldest event on overflow).
#   coalesced  a newer press superseded it before any render began. The app
#              coalesces input while a render is in flight, so N presses
#              during one refresh produce one frame showing the latest state.
#              The superseded presses never had a frame of their own and
#              their "latency" is just how fast the operator tapped.
#
# A deliberate-cadence capture (one press per settled page) leaves at most a
# press or two in either bucket — a final press after the last captured
# render, say — so ~2-4% of a 50-turn run. 10% keeps headroom over that
# boundary noise while still catching a burst or a dropped-input episode.
PAGE_TURN_UNTRUSTED_MAX_FRACTION = 0.10


@dataclass(frozen=True)
class PageTurnStats:
    """Press-to-settled pairing, with the bookkeeping needed to trust it."""

    durations: list[int]
    presses: int
    reading_renders: int
    # Presses whose answering render was not a Reading render: Library/Home
    # navigation, accounted for but not page turns.
    nav_answered: int
    # Presses superseded by a newer press before any render began. See the
    # constant above — these are the burst signal, and the reason the median
    # cannot be trusted on unmatched count alone.
    coalesced_presses: int

    @property
    def unmatched_presses(self) -> int:
        return self.presses - len(self.durations) - self.nav_answered - self.coalesced_presses

    @property
    def untrusted_presses(self) -> int:
        """Presses that produced no page-turn sample, for either reason."""
        return self.unmatched_presses + self.coalesced_presses

    @property
    def untrusted_fraction(self) -> float:
        if self.presses == 0:
            return 0.0
        return self.untrusted_presses / self.presses

    @property
    def median_trusted(self) -> bool:
        return bool(self.durations) and (
            self.untrusted_fraction <= PAGE_TURN_UNTRUSTED_MAX_FRACTION
        )


def render_begin_ms(event: dict[str, Any]) -> int:
    """When the reader state this render displays stopped being able to change.

    A press later than this cannot be reflected in the frame, so it must not
    be credited to it. `req_ms` is that boundary, stamped by the app as it
    freezes the request. `deq_ms`, when present, is the later instant the
    display task dequeued that request; it is reported for diagnosis and
    never used for pairing.

    Older captures have no `req_ms` and fall back to `t_ms - layout_ms -
    flush_ms`. That estimate is deliberately *not* trusted where the real
    stamp exists, because it omits everything outside those two intervals —
    the catalog and TOC window reads ahead of layout, and after the flush the
    planner record, a 52 KB framebuffer copy, and a chapter-tracking read that
    touches the card whenever the chapter changes. Each omission pushes the
    estimate later than the truth, which is the direction that mispairs: a
    press inside one of those windows looks earlier than the render's start
    and gets charged to it, worst at chapter boundaries. Treat page-turn
    minima from pre-`req_ms` logs as suspect for that reason.
    """
    # `req_ms` is stamped by the app as it freezes the request, so it is the
    # instant the frame's state stopped being able to change. It is deliberately
    # not the display task's dequeue time: a render can wait in the channel
    # behind a flush, a prestage, a storage command or a background build step,
    # and a press arriving during that wait belongs to the next frame, not this
    # one. (`deq_ms`, when present, is the dequeue instant; `deq_ms - req_ms` is
    # the queue wait, reported for diagnosis and never used for pairing.)
    # 0 means unstamped — see `RenderRequest::requested_at_ms`.
    req_ms = event.get("req_ms")
    if isinstance(req_ms, int) and req_ms > 0:
        return req_ms
    t_ms = int(event["t_ms"])
    layout_ms = event.get("layout_ms")
    flush_ms = event.get("flush_ms")
    if isinstance(layout_ms, int) and isinstance(flush_ms, int):
        return t_ms - layout_ms - flush_ms
    return t_ms


def page_turn_stats(events: list[dict[str, Any]]) -> PageTurnStats:
    """Press-to-settled durations, robust to renders the press did not cause.

    Each press is credited with the first render that *began* after it
    (the display task coalesces queued input into the latest reader state,
    so every press older than a render's begin time is reflected in it).
    A press that lands while a render is already in flight is NOT credited
    with the remainder of that render — it waits for the next one. This is
    what makes the statistic a function of firmware latency rather than
    tapping rhythm: a repaint or a storage-driven re-render begins after the
    press it follows was already answered and therefore credits nothing,
    where the old render-driven FIFO charged it to the *next* press and
    produced impossible 2 ms samples next to multi-second stale-press ones.

    A render yields at most one duration, measured from the newest press it
    answers. Older presses it also clears were superseded before it began —
    the app coalesced them into this one frame — so they are counted as
    `coalesced_presses` rather than given a duration of their own. Charging
    them would reintroduce the defect from the other side: a burst would
    produce a long sample per superseded press and still report every press
    as "matched". Presses answered by a non-Reading render are navigation,
    counted but excluded from durations.
    """
    pending_inputs: list[int] = []
    durations: list[int] = []
    presses = 0
    reading_renders = 0
    nav_answered = 0
    coalesced_presses = 0
    for event in sorted(events, key=event_sort_key):
        name = event.get("event")
        if name == "input" and press_action(event) in {"Next", "Previous"}:
            t_ms = event.get("t_ms")
            if isinstance(t_ms, int):
                presses += 1
                pending_inputs.append(t_ms)
        elif name == "render":
            t_ms = event.get("t_ms")
            if not isinstance(t_ms, int):
                continue
            is_reading = event.get("view") == "Reading"
            if is_reading:
                reading_renders += 1
            begin = render_begin_ms(event)
            # Events are sorted by t_ms, so pending_inputs is nondecreasing
            # and this pops exactly the presses this render answers.
            newest_answered: int | None = None
            answered = 0
            while pending_inputs and pending_inputs[0] <= begin:
                newest_answered = pending_inputs.pop(0)
                answered += 1
            if newest_answered is None:
                continue
            coalesced_presses += answered - 1
            if is_reading:
                durations.append(t_ms - newest_answered)
            else:
                nav_answered += 1
    return PageTurnStats(durations, presses, reading_renders, nav_answered, coalesced_presses)


class PageTurnCounter:
    """Live count of presses answered by a Reading render.

    The capture loop stops on this rather than on Reading renders, because a
    render that answered no press is not a sample: the boot paint and any
    storage-driven repaint are Reading renders, and each one used to end the
    capture a real turn short of what the operator asked for. Nothing
    downstream could notice — the report pairs properly, so it simply
    reported 49 turns for `--turns 50` and no check said otherwise.

    Applies the same rule as `page_turn_stats`, incrementally: a render
    answers every pending press older than its request-freeze boundary and
    yields at most one turn, and a press answered by a non-Reading render is
    navigation. A test pairs the two implementations against the same stream
    so they cannot drift. It works in arrival order rather than sorting by
    `t_ms`, which is what a live stream can do; the harness already documents
    that the two differ only by the odd millisecond of print inversion, which
    cannot change whether a press was answered.
    """

    def __init__(self) -> None:
        self.pending: list[int] = []
        self.turns = 0
        self.last_t: int | None = None

    def _new_epoch(self) -> None:
        """Drop presses from the boot that just ended.

        `t_ms` restarts at every reboot, and `pending` is a FIFO ordered by it,
        so a press left unanswered before a sleep or a reset sits at the head
        with a timestamp no later render can reach: every render after the
        wake compares against it, pops nothing, and counts no turn. One stale
        press stopped the counter permanently, which meant a `--turns N`
        capture that slept partway through never reached N and ran to its
        deadline instead — while the report, which splits by boot segment,
        reported the turns that did happen. This is the drift the two
        implementations are paired to prevent.
        """
        self.pending.clear()
        self.last_t = None

    def observe(self, event: dict[str, Any]) -> None:
        name = event.get("event")
        t_ms = event.get("t_ms")
        if name == "boot":
            # The first print of a new boot, emitted before the timer starts,
            # so it carries no `t_ms` to compare against.
            self._new_epoch()
        elif isinstance(t_ms, int):
            # The same regression test `boot_segments` splits on, so the live
            # counter and the report agree on where one boot ends.
            if self.last_t is not None and self.last_t - t_ms > _BOOT_REGRESSION_SKEW_MS:
                self._new_epoch()
            self.last_t = t_ms
        if name == "input" and press_action(event) in {"Next", "Previous"}:
            if isinstance(t_ms, int):
                self.pending.append(t_ms)
        elif name == "render" and isinstance(t_ms, int):
            begin = render_begin_ms(event)
            answered = 0
            while self.pending and self.pending[0] <= begin:
                self.pending.pop(0)
                answered += 1
            if answered and event.get("view") == "Reading":
                self.turns += 1


def page_turn_stats_over_epochs(events: list[dict[str, Any]]) -> PageTurnStats:
    """Pair presses to renders within each device time epoch, then merge.

    `t_ms` is device uptime. It restarts at every reboot, and separate
    captures in one log each carry their own. `page_turn_stats` sorts by it,
    so handing it events that span an epoch boundary interleaves two clocks:
    two healthy runs that each recorded `input @1000, render @1500` sort as
    two inputs then two renders, and the first render answers both — one
    invented coalesced press, one render credited with nothing, and with
    other overlaps, durations measured from one run's press to another run's
    render.

    `boot_report` documents this hazard and guards against it; page-turn
    pairing did not, which left `--all` and any single capture containing a
    sleep or a reset (so `sleep-sync` and `reader-soak` by construction)
    reporting cross-epoch samples. Splitting by run *and* by boot segment is
    the same decomposition the boot report already computes.
    """
    return merge_page_turn_stats(
        [
            page_turn_stats(segment)
            for run in split_runs(events)
            for segment, _kind in boot_segments(run)[0]
        ]
    )


@dataclass(frozen=True)
class PageTurnPool:
    """One page-turn population per run, and the two ways of adding them up.

    Trust is a property of how a capture was *performed*, so it has to be
    decided before the runs are added together. Merging first hides the bad
    one in the good ones: a run of 1 turn and 1 unanswered press is 50%
    untrusted on its own, and pooled with a clean 20-turn run it becomes 1 in
    22 and passes the 10% threshold. The report then prints, and `--strict`
    gates on, a median partly drawn from a capture this same tool would
    refuse to report on its own.

    `every` accounts for every press and is what the `page inputs:` line
    reports. `trusted` is the population the median comes from: runs that
    paired at a cadence worth believing. `untrusted` is what was left out,
    named, so the operator knows which capture to redo.
    """

    per_run: list[tuple[str, PageTurnStats]]

    @property
    def every(self) -> PageTurnStats:
        return merge_page_turn_stats([stats for _label, stats in self.per_run])

    @property
    def trusted(self) -> PageTurnStats:
        return merge_page_turn_stats(
            [stats for _label, stats in self.per_run if stats.median_trusted]
        )

    @property
    def untrusted(self) -> list[tuple[str, PageTurnStats]]:
        """Runs whose presses gave the median nothing it could use.

        A run that produced no pairing at all belongs here as much as a
        cadence-corrupted one: `trusted` drops both, so reporting only the
        second meant a pooled report printed the healthy run's median with no
        sign that a whole capture had contributed nothing. Runs with no
        presses are silent because nothing was attempted in them.
        """
        return [
            (label, stats)
            for label, stats in self.per_run
            if stats.presses and not stats.median_trusted
        ]


def page_turn_exclusion(stats: PageTurnStats) -> str:
    """Why a run's presses are not in the pooled median.

    One wording for both the report and the budget warning, so the two
    cannot drift into disagreeing about what was left out.
    """
    if not stats.durations:
        return f"{stats.presses} presses and no input-to-Reading-render sample"
    return (
        f"{stats.untrusted_presses}/{stats.presses} presses produced no page "
        f"turn ({stats.coalesced_presses} coalesced, "
        f"{stats.unmatched_presses} unanswered; over "
        f"{PAGE_TURN_UNTRUSTED_MAX_FRACTION:.0%}), so its samples are operator "
        "cadence, not firmware latency"
    )


def page_turn_pool(runs: list[LabelledRun]) -> PageTurnPool:
    """Pair presses to renders inside each run, keeping the runs apart."""
    return PageTurnPool([(run.label, page_turn_stats_over_epochs(run.events)) for run in runs])


def merge_page_turn_stats(parts: list[PageTurnStats]) -> PageTurnStats:
    """Sum independently paired populations into one.

    Used both for the boot segments inside a run and for the runs inside a
    pooled report: the counters add and the durations concatenate, so a
    pooled figure is exactly its parts and coverage can be judged per part
    without measuring anything twice.
    """
    merged = PageTurnStats([], 0, 0, 0, 0)
    for stats in parts:
        merged = PageTurnStats(
            merged.durations + stats.durations,
            merged.presses + stats.presses,
            merged.reading_renders + stats.reading_renders,
            merged.nav_answered + stats.nav_answered,
            merged.coalesced_presses + stats.coalesced_presses,
        )
    return merged


def page_turn_durations(events: list[dict[str, Any]]) -> list[int]:
    return page_turn_stats_over_epochs(events).durations


def event_sort_key(event: dict[str, Any]) -> tuple[int, float]:
    t_ms = event.get("t_ms")
    if isinstance(t_ms, int):
        return (0, float(t_ms))
    host_time = event.get("host_time")
    if isinstance(host_time, (int, float)):
        return (1, float(host_time) * 1000)
    return (2, 0)


# `t_ms` is device uptime, so within one boot it only grows; a decrease in
# capture (file) order marks a reboot. A reboot right after a completed deep
# sleep is the normal wake path. Any other one is a watchdog/brownout/manual
# reset, and it matters because `event_sort_key` orders events by t_ms while
# `split_runs` splits only on host-side run_start markers: the two boots'
# time bases interleave and press/render pairings straddle the reset.
_SLEEP_TERMINAL_PHASES = {"deep_sleep", "complete"}


def is_terminal_sleep(event: dict[str, Any]) -> bool:
    """A sleep line that says the device actually went to sleep.

    Most `sleep` lines say no such thing: `phase=requested` opens the
    transition, and `refresh`, `power_down_start`, `power_down_done` and
    `power_off` are steps inside it that a failed handshake reaches too.
    Counting any of them as a sleep let a `sleep-sync --strict` capture pass
    having only *asked* for one.

    The terminal markers are `complete`, printed by the display task on both
    devices once the panel acknowledged, and `deep_sleep`, printed by the X3
    panel driver *after* its deep-sleep command returned — the only terminal
    marker in captures predating the `complete ok=true` line, whose absence
    on the success path was itself a defect. `phase=complete` prints on both
    outcomes and carries the result in `ok`; absent (older captures) is
    treated as success.

    The X4's `display: sleep deep` (parsed as `phase=deep`) is deliberately
    not one of them, despite the name: ssd1677 prints it after the power-down
    handshake and *before* sending its deep-sleep command, which can still
    fail. It also carries no `t_ms`, so a boot segment latched by it could
    never be un-latched by the later `complete ok=false` and a reset was
    filed as a wake. An X4 capture with no `complete ok=true` cannot prove a
    sleep finished, and now fails closed instead of claiming one.
    """
    return (
        event.get("event") == "sleep"
        and event.get("phase") in _SLEEP_TERMINAL_PHASES
        and event.get("ok") is not False
    )


def is_completed_sleep_cycle(event: dict[str, Any]) -> bool:
    """One sleep cycle the panel finished, counted exactly once.

    `sleep-sync --cycles N` stops on this and the report checks against the
    same predicate, so the two cannot disagree. Narrower than
    `is_terminal_sleep` on both sides, deliberately:

    - `ok` must not be false. `phase=complete` prints on both outcomes, and
      counting a failed one ended the capture as though a cycle had landed.
    - `phase=deep_sleep` is not counted: the X3 panel driver prints it
      *beside* the display task's `phase=complete`, so admitting it counted
      every X3 cycle twice and `--cycles 10` would have stopped at five.
      `is_terminal_sleep` still accepts it — "did this device ever sleep?" of
      a pre-`complete` capture is a different question.
    """
    return (
        event.get("event") == "sleep"
        and event.get("phase") == "complete"
        and event.get("ok") is not False
    )


def completed_sleep_cycles(events: list[dict[str, Any]]) -> int:
    return sum(1 for event in events if is_completed_sleep_cycle(event))


def has_failed_sleep(events: list[dict[str, Any]]) -> bool:
    """A sleep phase the firmware reported as failed.

    Any phase counts, not just the terminal ones: `refresh`, `power_down_*`
    and `power_off` each fail on their own, and the power task retries, so a
    later completed cycle in the same capture does not make the failure go
    away. The structured event is the only record of it — the parser drops
    the duplicate unstructured "framebuffer flush failed" line on purpose.
    """
    return any(event.get("event") == "sleep" and event.get("ok") is False for event in events)


# A t_ms decrease smaller than this is treated as clock skew, not a reboot.
# `bench: input` is stamped and printed from the interrupt-priority executor
# and can preempt the display task between its `Instant::now()` and its
# blocking UART write, so two lines can land out of order by a hair. A real
# reboot restarts the uptime clock, so its regression is the whole elapsed
# session — orders of magnitude above this. Without the guard a 1 ms
# inversion fabricates a boot, a boot-to-first-paint sample, and a --strict
# failure; no inversion of any size appears in the reference capture, so this
# guards a hazard rather than an observed defect.
_BOOT_REGRESSION_SKEW_MS = 250


def boot_segments(
    run: list[dict[str, Any]],
) -> tuple[list[tuple[list[dict[str, Any]], str]], list[str]]:
    """Split one run's events (kept in capture order) at device reboots.

    Returns ``([(segment, kind)], warnings)``. The leading segment's kind is
    "attach" (capture joined an already-running device; its t_ms values do
    not date from a witnessed boot). Later segments start at a reboot:
    "wake" when the boot marker reports a real deep-sleep wake (or, with no
    marker at all, a completed deep sleep preceded it), "cold" when the
    marker says otherwise, and "reset" for a bare t_ms regression - an
    unexplained reboot, which is also returned as a warning.
    """
    segments: list[list[Any]] = [[[], "attach"]]
    warnings: list[str] = []
    last_t: int | None = None
    slept = False
    slept_at: int | None = None
    for event in run:
        name = str(event.get("event", ""))
        if name == "boot":
            # Explicit marker line: the first print of a new boot. The
            # firmware already decided this — `deep_sleep_wake` is
            # `gpio && sleep_image`, because a wake pin that fired is only
            # half the story: a wake whose sleep image was not retained pays
            # the full waveform, so its first paint belongs with the cold
            # cluster. Take that verdict as given. A preceding sleep must not
            # overrule it: a device that browns out or resets after a
            # completed sleep reports false here, and reading it as a wake
            # files a full cold boot in the fast cluster. The heuristic is
            # kept only for markers too old to carry the fields.
            wake = event.get("deep_sleep_wake")
            gpio = event.get("gpio")
            if not isinstance(wake, bool) and isinstance(gpio, bool):
                # A marker printed before the combined field existed: the
                # same rule, spelled out from its two halves.
                wake = gpio and event.get("sleep_image") is not False
            if isinstance(wake, bool):
                boot_kind = "wake" if wake else "cold"
            else:
                # No marker fields at all — only then is a preceding sleep
                # the best evidence available.
                boot_kind = "wake" if slept else "cold"
            if segments[-1][0]:
                segments.append([[], boot_kind])
            else:
                segments[-1][1] = boot_kind
            slept = False
            slept_at = None
            last_t = None
        t_ms = event.get("t_ms")
        if isinstance(t_ms, int):
            if last_t is not None and last_t - t_ms > _BOOT_REGRESSION_SKEW_MS:
                kind = "wake" if slept else "reset"
                if kind == "reset":
                    warnings.append(
                        f"t_ms went backwards ({last_t} -> {t_ms}) with no deep sleep "
                        "recorded: unexpected reset (watchdog/brownout?); timings "
                        "straddle two boots"
                    )
                segments.append([[], kind])
                slept = False
                slept_at = None
            elif slept and slept_at is not None and t_ms > slept_at:
                # The device went on running past a sleep it reported as
                # terminal, so it did not deep-sleep: the panel handshake
                # failed and the power task stayed awake to retry. Anything
                # that follows is not a wake, and a reset after it must still
                # be reported as one.
                slept = False
                slept_at = None
            last_t = t_ms
        if is_terminal_sleep(event):
            slept = True
            slept_at = t_ms if isinstance(t_ms, int) else None
        segments[-1][0].append(event)
    return [(segment, kind) for segment, kind in segments if segment], warnings


def woke_after_sleep(run: list[dict[str, Any]]) -> bool:
    """True when a wake in this run followed a sleep this run captured.

    Asking the two halves separately - does a completed sleep appear, does
    any segment read as a wake - passes a capture that was started while the
    device was already asleep: the operator wakes it to begin, the leading
    boot marker says `deep_sleep_wake=true`, and the sleep at the far end of
    the run is never woken from. That is the ordinary way a soak is started,
    and it left the wake path unexercised while reading as a full cycle. So
    walk the segments in capture order and require the wake to come after.
    """
    slept = False
    for segment, kind in boot_segments(run)[0]:
        if slept and kind == "wake":
            return True
        if any(is_terminal_sleep(event) for event in segment):
            slept = True
    return False


def boot_report(
    scoped_runs: list[list[dict[str, Any]]],
) -> tuple[dict[str, list[int]], dict[str, list[int]], list[str]]:
    """Boot-to-first-paint per witnessed boot, plus boot-stage samples.

    The t_ms epoch is the esp-hal systimer (embassy_time resolves to
    esp_rtos::now() -> Instant::now().duration_since_epoch()), which starts
    a little before esp_rtos::start. Neither epoch includes the ROM and
    second-stage bootloader, so "boot to paint" is firmware-visible boot
    time and not wall-clock time from the button press. The t_ms of a boot's
    first render is that figure - but only when the capture actually witnessed
    the boot (a reset_before run start, a boot marker line, a wake, or a
    t_ms regression). An "attach" segment joined mid-session, and its first
    render says nothing about boot. Note a wake whose first-paint line was
    lost while the serial port re-enumerated can leave an inflated sample;
    it shows up as an outlier against the boot-time cluster.
    """
    paints: dict[str, list[int]] = {}
    stages: dict[str, list[int]] = {}
    warnings: list[str] = []
    for run in scoped_runs:
        run_start = next((e for e in run if e.get("event") == "run_start"), {})
        segments, segment_warnings = boot_segments(run)
        warnings.extend(segment_warnings)
        for index, (segment, kind) in enumerate(segments):
            if index == 0 and kind == "attach" and run_start.get("reset_before"):
                # `--reset-before` hard-resets after the capture opens, so
                # the run's leading segment is a witnessed cold boot.
                kind = "cold"
            if kind == "attach":
                continue
            first_render_t: int | None = None
            for event in segment:
                if event.get("event") == "render" and isinstance(event.get("t_ms"), int):
                    first_render_t = int(event["t_ms"])
                    break
            if first_render_t is None:
                continue
            paints.setdefault(kind, []).append(first_render_t)
            for event in segment:
                if event.get("event") == "boot_stage" and isinstance(event.get("t_ms"), int):
                    t_ms = int(event["t_ms"])
                    if t_ms <= first_render_t:
                        stages.setdefault(str(event.get("stage")), []).append(t_ms)
    return paints, stages, warnings


def load_budgets(path: Path | None) -> tuple[dict[str, Any], str | None]:
    """Returns (budgets, problem).

    ``problem`` is a human-readable reason the budgets could not be loaded,
    and ``budgets`` is empty whenever it is set. A ``None`` path means the
    caller intentionally disabled budgets, which is not a problem. Budgets
    silently absent is how ``--strict`` once verified nothing for months, so
    every involuntary empty result carries its reason. The parser is no longer
    one of them: `tomllib` is imported directly.
    """
    if path is None:
        return {}, None
    if not path.exists():
        return {}, f"budgets file {path} does not exist"
    # Unreadable and unparseable both owe the same answer as a missing parser:
    # every involuntary empty result carries its reason, so `--strict` exits
    # on it and a plain report warns. Letting either raise broke that with a
    # traceback — `path.exists()` is satisfied by a directory, and a decode
    # error propagated straight out. `TOMLDecodeError` is resolved off
    # whichever parser is bound, so a test double without one still works.
    decode_error = getattr(tomllib, "TOMLDecodeError", ValueError)
    try:
        with path.open("rb") as handle:
            budgets = tomllib.load(handle)
    except OSError as err:
        return {}, f"cannot read {path}: {err}"
    except decode_error as err:
        return {}, f"cannot parse {path}: {err}"
    problems = budget_schema_problems(budgets)
    if problems:
        return {}, f"{path} is not a valid budget file: " + "; ".join(problems)
    return budgets, None


# Every budget this harness enforces, and the section that owns it. This is
# the registry, not documentation: a key absent from here is rejected rather
# than ignored, so a misspelling cannot leave a section with no operative
# threshold while `--strict` still reports success.
BUDGET_SCHEMA: dict[str, set[str]] = {
    "page-turn": {
        "median_press_to_settled_ms",
        "median_press_to_settled_min_ms",
        "fast_refresh_busy_warn_ms",
        "reading_layout_warn_ms",
        "prestage_warn_ms",
    },
    "sleep-sync": {
        "full_refresh_busy_min_ms",
        "full_refresh_busy_max_ms",
    },
    "storage-cache": {
        "warm_book_open_warn_ms",
        "catalog_load_warn_ms",
    },
    "folder-nav": {
        "folder_enter_warn_ms",
        "folder_leave_warn_ms",
    },
}

# Budget keys that bound the same measurement from both sides, as
# (floor, ceiling). A floor above its ceiling admits no sample at all, so it
# is a budget guaranteed to fire or guaranteed never to — either way it is not
# measuring anything, which is what this validation exists to catch.
BUDGET_BOUND_PAIRS: dict[str, list[tuple[str, str]]] = {
    "page-turn": [("median_press_to_settled_min_ms", "median_press_to_settled_ms")],
    "sleep-sync": [("full_refresh_busy_min_ms", "full_refresh_busy_max_ms")],
}


def budget_schema_problems(budgets: dict[str, Any]) -> list[str]:
    """Everything wrong with a budget document, before any capture is read.

    Every consumer fails open: `warn_if_above`/`warn_if_below` return silently
    unless the threshold is an `int`, and an unknown key is never looked up.
    So `median_press_to_settledd_ms = 550` left page-turn with no operative
    threshold while `--strict` still exited 0 — a file that reads as enforced
    and enforces nothing. A string did the same, and so did `true`, since
    `isinstance(True, int)` holds and a bool arrives at the comparison as 1.

    Rejected for the same reason, each being a gate that cannot fire: an empty
    section, a negative threshold, a floor above its own ceiling, and a
    document with no sections at all — that last separately, because the loop
    below has nothing to iterate and `budget_sections_in_play` cannot tell
    `{}` from budgets deliberately disabled.

    Reported as a load failure, so `--strict` refuses to run and a plain
    report says budgets were not checked.
    """
    problems: list[str] = []
    if not budgets:
        return ["the document configures no budget sections"]
    for section, entries in sorted(budgets.items()):
        known = BUDGET_SCHEMA.get(section)
        if known is None:
            problems.append(
                f"unknown section [{section}] (known sections are "
                f"{', '.join(sorted(BUDGET_SCHEMA))})"
            )
            continue
        if not isinstance(entries, dict):
            problems.append(f"[{section}] is not a table")
            continue
        if not entries:
            problems.append(f"[{section}] configures no budget")
            continue
        for key, value in sorted(entries.items()):
            if key not in known:
                problems.append(
                    f"[{section}] has unknown key {key} (known keys are {', '.join(sorted(known))})"
                )
            elif isinstance(value, bool) or not isinstance(value, int):
                problems.append(
                    f"[{section}] {key} must be an integer number of milliseconds, not {value!r}"
                )
            elif value < 0:
                problems.append(
                    f"[{section}] {key} is {value}; a millisecond budget "
                    "cannot be negative, and as a bound it would gate nothing"
                )
        problems.extend(budget_bound_problems(section, entries))
    return problems


def budget_bound_problems(section: str, entries: dict[str, Any]) -> list[str]:
    """Floors that sit above their own ceilings."""
    problems = []
    for floor_key, ceiling_key in BUDGET_BOUND_PAIRS.get(section, []):
        floor = entries.get(floor_key)
        ceiling = entries.get(ceiling_key)
        if (
            isinstance(floor, int)
            and isinstance(ceiling, int)
            and not isinstance(floor, bool)
            and not isinstance(ceiling, bool)
            and floor > ceiling
        ):
            problems.append(
                f"[{section}] {floor_key} ({floor}ms) is above {ceiling_key} "
                f"({ceiling}ms), so no measurement can satisfy both"
            )
    return problems


def evaluate_budgets(events: list[dict[str, Any]], budgets: dict[str, Any]) -> list[str]:
    in_play, warnings = budget_sections_in_play(events, budgets)
    page_turn = budgets.get("page-turn", {})
    if page_turn and "page-turn" in in_play:
        runs = section_runs(events, "page-turn")
        pool = page_turn_pool(runs)
        # A gate over a cadence artifact is worse than no gate, and trust is
        # per capture: an untrusted run is named and dropped rather than
        # averaged into the pool it would otherwise corrupt.
        for label, stats in pool.untrusted:
            warnings.append(f"page-turn median excludes {label}: {page_turn_exclusion(stats)}")
        if pool.trusted.durations:
            turn_median = statistics.median(pool.trusted.durations)
            warn_if_above(
                warnings,
                "page-turn median",
                turn_median,
                page_turn.get("median_press_to_settled_ms"),
            )
            warn_if_below(
                warnings,
                "page-turn median",
                turn_median,
                page_turn.get("median_press_to_settled_min_ms"),
            )
        elif pool.every.durations:
            warnings.append(
                "page-turn median: no run paired at a trustworthy cadence; budget not checked"
            )
        for key in ("median_press_to_settled_ms", "median_press_to_settled_min_ms"):
            warn_if_unobserved(
                warnings,
                "page-turn",
                page_turn,
                key,
                runs,
                [stats.durations for _label, stats in pool.per_run],
                "press-to-settled pairings",
            )
        per_run_layout, reading_layout = per_run_samples(
            runs,
            lambda run_events: values(
                [
                    event
                    for event in run_events
                    if event.get("event") == "render" and event.get("view") == "Reading"
                ],
                "layout_ms",
            ),
        )
        warn_if_above(
            warnings,
            "Reading layout p95",
            percentile(reading_layout, 95) if reading_layout else None,
            page_turn.get("reading_layout_warn_ms"),
        )
        warn_if_unobserved(
            warnings,
            "page-turn",
            page_turn,
            "reading_layout_warn_ms",
            runs,
            per_run_layout,
            "Reading renders",
        )
        per_run_prestage, prestage = per_run_samples(runs, prestage_values)
        warn_if_above(
            warnings,
            "prestage p95",
            percentile(prestage, 95) if prestage else None,
            page_turn.get("prestage_warn_ms"),
        )
        warn_if_unobserved(
            warnings,
            "page-turn",
            page_turn,
            "prestage_warn_ms",
            runs,
            per_run_prestage,
            "prestage samples",
        )
        per_run_fast, fast_busy = per_run_samples(
            runs, lambda run_events: refresh_busy_values(run_events, "Fast")
        )
        warn_if_above(
            warnings,
            "Fast refresh busy p95",
            percentile(fast_busy, 95) if fast_busy else None,
            page_turn.get("fast_refresh_busy_warn_ms"),
        )
        warn_if_unobserved(
            warnings,
            "page-turn",
            page_turn,
            "fast_refresh_busy_warn_ms",
            runs,
            per_run_fast,
            "Fast refresh events",
        )

    sleep_sync = budgets.get("sleep-sync", {})
    if sleep_sync and "sleep-sync" in in_play:
        runs = section_runs(events, "sleep-sync")
        per_run_full, full_busy = per_run_samples(
            runs, lambda run_events: refresh_busy_values(run_events, "Full")
        )
        min_ms = sleep_sync.get("full_refresh_busy_min_ms")
        max_ms = sleep_sync.get("full_refresh_busy_max_ms")
        for busy in full_busy:
            if isinstance(min_ms, int) and busy < min_ms:
                warnings.append(f"Full refresh busy {busy}ms below budget floor {min_ms}ms")
            if isinstance(max_ms, int) and busy > max_ms:
                warnings.append(f"Full refresh busy {busy}ms above budget ceiling {max_ms}ms")
        failed_sleeps = [
            event
            for run in runs
            for event in run.events
            if event.get("event") == "sleep" and event.get("ok") is False
        ]
        if failed_sleeps:
            warnings.append(f"{len(failed_sleeps)} failed sleep phase(s)")
        for key in ("full_refresh_busy_min_ms", "full_refresh_busy_max_ms"):
            warn_if_unobserved(
                warnings,
                "sleep-sync",
                sleep_sync,
                key,
                runs,
                per_run_full,
                "Full refresh events",
            )
    storage_cache = budgets.get("storage-cache", {})
    if storage_cache and "storage-cache" in in_play:
        runs = section_runs(events, "storage-cache")
        # Warm opens only. The budget is named for the warm path and sized for
        # it (150 ms against a 57-95 ms population), but it was computed over
        # every `storage_open`, so a cold build — 14 to 64 seconds on this
        # repo's captures — failed the *warm* ceiling, and a RAM hit at 0 ms
        # dragged the percentile back under it. Neither number described the
        # path the key is about.
        per_run_open, warm_open = per_run_samples(
            runs, lambda run_events: storage_open_values(run_events, "warm")
        )
        warn_if_above(
            warnings,
            "warm book open p95",
            percentile(warm_open, 95) if warm_open else None,
            storage_cache.get("warm_book_open_warn_ms"),
        )
        warn_if_unobserved(
            warnings,
            "storage-cache",
            storage_cache,
            "warm_book_open_warn_ms",
            runs,
            per_run_open,
            "warm storage_open events",
        )
        # Loads that actually loaded: a miss (no snapshot on the card yet) and
        # a refused session both return in a fraction of the time a real load
        # takes, so pooling them measured how fast the card said no and pulled
        # the percentile away from the path the budget is about.
        per_run_catalog, catalog_load = per_run_samples(
            runs,
            lambda run_events: values(catalog_samples(run_events, "load"), "elapsed_ms"),
        )
        warn_if_above(
            warnings,
            "catalog load p95",
            percentile(catalog_load, 95) if catalog_load else None,
            storage_cache.get("catalog_load_warn_ms"),
        )
        warn_if_unobserved(
            warnings,
            "storage-cache",
            storage_cache,
            "catalog_load_warn_ms",
            runs,
            per_run_catalog,
            "catalog load events",
        )
    folder_nav = budgets.get("folder-nav", {})
    if folder_nav and "folder-nav" in in_play:
        runs = section_runs(events, "folder-nav")
        # Successful operations only. A refused leave returns in a fraction
        # of the time a real one takes and is a card fault, which the suite
        # check names on its own; pooling it here would pull the percentile
        # toward how fast the card said no.
        per_run_enter, enter = per_run_samples(
            runs,
            lambda run_events: values(folder_samples(run_events, "folder_enter"), "ms"),
        )
        warn_if_above(
            warnings,
            "folder enter p95",
            percentile(enter, 95) if enter else None,
            folder_nav.get("folder_enter_warn_ms"),
        )
        warn_if_unobserved(
            warnings,
            "folder-nav",
            folder_nav,
            "folder_enter_warn_ms",
            runs,
            per_run_enter,
            "folder_enter events",
        )
        per_run_leave, leave = per_run_samples(
            runs,
            lambda run_events: values(folder_samples(run_events, "folder_leave"), "ms"),
        )
        warn_if_above(
            warnings,
            "folder leave p95",
            percentile(leave, 95) if leave else None,
            folder_nav.get("folder_leave_warn_ms"),
        )
        warn_if_unobserved(
            warnings,
            "folder-nav",
            folder_nav,
            "folder_leave_warn_ms",
            runs,
            per_run_leave,
            "folder_leave events",
        )
    return warnings


def folder_samples(events: list[dict[str, Any]], kind: str) -> list[dict[str, Any]]:
    """Folder operations of one kind that succeeded, so a budget times the
    operation and not the card refusing it."""
    return [e for e in events if e.get("event") == kind and e.get("ok") is not False]


def measured_folder_round_trips(events: list[dict[str, Any]]) -> int:
    """Round trips with both halves in the capture: a successful enter and
    the successful leave that follows it before any other enter.

    Entering and leaving are separate measured operations with separate
    budgets, so a round trip whose enter line is missing has a leave sample
    and no enter sample, and counting leaves alone would report it measured.
    Pairing in order also refuses a duplicated or misordered record.
    """
    pairs = 0
    open_enter = False
    for event in events:
        kind = event.get("event")
        if kind == "folder_enter":
            open_enter = event.get("ok") is not False
        elif kind == "folder_leave":
            if open_enter and event.get("ok") is not False:
                pairs += 1
            open_enter = False
    return pairs


def evaluate_suite_signals(events: list[dict[str, Any]]) -> list[str]:
    """The telemetry each capture owes, checked against what it produced.

    Resolved by workflow rather than by suite: a `thermal-run --suite
    sleep-sync` capture owes sleep telemetry, and asking it only for the
    refresh events every thermal run has is how a deliberately selected
    workflow passed `--strict` without its own signal ever being looked for.

    Checked per run, not per workflow. Runs of one workflow used to be
    concatenated before the check, so two sleep-sync captures — one holding
    only a Full refresh, one only a completed sleep — answered for each other
    and both passed while neither was a complete capture. Runs are named
    `suite/workflow`, and by position when the log holds more than one, so a
    warning says which capture it came from.
    """
    warnings: list[str] = []
    for run in labelled_runs(events):
        workflow = run.workflow
        label = run.label
        warnings.extend(capture_completion_warnings(run))
        signal_events = [
            event for event in run.events if event.get("event") not in {"run_start", "run_end"}
        ]
        if not signal_events:
            warnings.append(f"{label}: no parsed bench telemetry")
            continue
        event_names = {str(event.get("event")) for event in signal_events}
        if "warning" in event_names:
            warnings.append(f"{label}: warning events present")
        invalid_reasons = sorted(
            {
                str(event.get("reason", "unspecified"))
                for event in signal_events
                if event.get("event") == "selftest_invalid"
            }
        )
        for reason in invalid_reasons:
            warnings.append(
                f"{label}: the selftest scenario reported it could not run "
                f"{reason}, so this capture does not cover the whole workflow"
            )
        # A self-driving scenario writes exactly one terminal record, and only
        # `done` means it finished what it set out to do. Stopping the capture
        # on any of them is right, since the device has stopped talking, but
        # certifying any of them was not: a storage-cache run that failed to
        # open on its second cycle ended the capture on `nav-failed` while its
        # first cycle had already produced the telemetry and budget samples
        # this gate asks for.
        # Every selftest record names its scenario, so every one of them can
        # answer whether the firmware on the device is what the command asked
        # for. Checked across all three and not only the announcement: that
        # line prints once, four seconds after boot, while a finite scenario
        # goes on working for minutes, so a capture attached late sees a
        # terminal record and no announcement. A page-turn image captured as
        # `storage-cache --strict` supplies storage telemetry from opening
        # its book and then reports `scenario=page-turn result=done`, which
        # the result check has no opinion about.
        #
        # Only checked where a record exists, so a capture from a shipped
        # build is unaffected: it emits none of these. `workflow` and not
        # `suite`, so a thermal-run comparison uses the workflow it selected.
        announced = {
            str(event.get("scenario"))
            for event in signal_events
            if str(event.get("event", "")).startswith("selftest_")
        }
        for scenario in sorted(announced):
            if workflow is not None and scenario != workflow:
                warnings.append(
                    f"{label}: the device is running the {scenario} selftest "
                    f"scenario, but this capture was taken as {workflow}"
                )
        terminals = [e for e in signal_events if e.get("event") == "selftest_done"]
        for event in terminals:
            result = str(event.get("result", "unspecified"))
            if result != "done":
                warnings.append(
                    f"{label}: the selftest scenario ended on result={result} "
                    "rather than done, so it did not finish the workflow"
                )
        if len(terminals) > 1:
            # One scenario, one ending. Two means the firmware printed a
            # terminal record and carried on, which is how the earlier
            # storage-cache arm produced `nav-failed` and then `done`.
            reported = ", ".join(str(e.get("result")) for e in terminals)
            warnings.append(
                f"{label}: {len(terminals)} selftest terminal records ({reported}); "
                "a scenario owes exactly one"
            )
        if run.suite == "thermal-run" and "refresh" not in event_names:
            # Ambient investigations live on refresh timing whatever workflow
            # they ran under.
            warnings.append(f"{label}: no refresh timing telemetry captured")
        if workflow == "page-turn":
            turn_stats = page_turn_stats_over_epochs(run.events)
            if not turn_stats.durations:
                warnings.append(f"{label}: no input-to-Reading-render duration captured")
            elif not turn_stats.median_trusted:
                warnings.append(
                    f"{label}: {turn_stats.untrusted_presses}/{turn_stats.presses} "
                    "presses produced no page turn; the press-to-settled figure "
                    "is cadence, not firmware — recapture at one press per "
                    "settled page"
                )
        elif workflow == "storage-cache":
            if not any(name.startswith("storage") for name in event_names):
                warnings.append(f"{label}: no storage telemetry captured")
            # The same rule as a failed sleep phase in a sleep suite: the
            # workflow that owns this telemetry is the one that has to answer
            # for it, and a later success does not undo the failure. The
            # unstructured `sd: session failed` line is deliberately not
            # parsed, so the structured `ok=false` is the only record.
            failed = failed_storage_ops(signal_events)
            if failed:
                actions = sorted(
                    {
                        # Not every storage event carries an action, and
                        # interpolating a missing one printed the literal
                        # "storage_x None" into the warning.
                        " ".join(
                            str(part)
                            for part in (event.get("event"), event.get("action"))
                            if part is not None
                        )
                        for event in failed
                    }
                )
                warnings.append(
                    f"{label}: {len(failed)} failed storage operation(s) "
                    f"captured ({', '.join(actions)})"
                )
            # A result nothing here recognises is neither a success nor a
            # known fault, so it would otherwise be the one storage outcome
            # `--strict` says nothing at all about.
            unknown = [event for event in signal_events if unknown_catalog_result(event)]
            if unknown:
                seen = sorted({repr(event.get("result")) for event in unknown})
                warnings.append(
                    f"{label}: {len(unknown)} catalog operation(s) reported a "
                    f"result this bench.py does not know ({', '.join(seen)}); "
                    f"known results are {', '.join(sorted(CATALOG_LOAD_RESULTS))}"
                )
        elif workflow == "folder-nav":
            # Entering and leaving are both the walk this suite times, and
            # they are separate storage operations with separate telemetry. A
            # capture holding only entries measured half of what it was run
            # for, which is what a scenario that descended instead of coming
            # back out produced.
            if "folder_enter" not in event_names:
                warnings.append(f"{label}: no folder entry telemetry captured")
            elif "folder_leave" not in event_names:
                warnings.append(
                    f"{label}: folder entries captured but no leaves; the walk "
                    "went down and did not come back"
                )
            failed = sum(
                1
                for event in signal_events
                if event.get("event") == "folder_leave" and event.get("ok") is False
            )
            if failed:
                warnings.append(
                    f"{label}: {failed} folder leave(s) failed; a refused leave "
                    "is a card fault, not a sample"
                )
        elif workflow == "sleep-sync":
            if not any(is_terminal_sleep(event) for event in signal_events):
                warnings.append(
                    f"{label}: no completed sleep captured; a requested or "
                    "part-way sleep does not show the panel slept"
                )
            if has_failed_sleep(signal_events):
                warnings.append(f"{label}: failed sleep phase captured")
        elif workflow == "reader-soak":
            if not {"render", "input"}.issubset(event_names):
                warnings.append(f"{label}: expected both input and render telemetry")
            # The suite's guidance promises a sleep/wake cycle, and the wake
            # path is the part of the workflow nothing else exercises: a soak
            # that only turned pages was a page-turn run wearing the soak's
            # name, and passed strict as one.
            if not any(is_terminal_sleep(event) for event in signal_events):
                warnings.append(
                    f"{label}: no completed sleep captured; the soak workflow "
                    "includes a sleep/wake cycle"
                )
            elif not woke_after_sleep(run.events):
                warnings.append(
                    f"{label}: a sleep completed but no wake followed it; the "
                    "soak workflow includes a sleep/wake cycle"
                )
            if has_failed_sleep(signal_events):
                warnings.append(f"{label}: failed sleep phase captured")
        elif workflow == "thermal-run":
            # No workflow recorded, so there is nothing to check beyond the
            # refresh timing above; say so rather than imply it was gated.
            warnings.append(
                f"{label}: no workflow recorded, so no workflow signal was "
                "checked; recapture with a current bench.py"
            )
        elif workflow is None:
            warnings.append(
                f"{label}: no suite label, so nothing workflow-specific was "
                "checked; report this log on its own"
            )
        else:
            # A misspelling, or a workflow newer than this bench.py. Either
            # way nothing here knows what the run owed, and silence would
            # read as a pass — the fail-closed direction is to say so.
            warnings.append(
                f"{label}: unrecognised workflow, so nothing workflow-specific "
                f"was checked; known workflows are {', '.join(sorted(SUITES))}"
            )
    return warnings


# How far a capture's telemetry window may fall short of the duration asked
# for before it counts as cut short. The window is closed by a select() with a
# 0.2 s tick and the run_end timestamp is taken after it, so a healthy capture
# lands a few tens of milliseconds over, never under; a second of slack covers
# rounding without admitting a run that stopped early.
CAPTURE_DURATION_TOLERANCE_S = 1.0


def capture_completion_warnings(run: LabelledRun) -> list[str]:
    """Did this capture finish, and did it collect what it was asked for?

    Two questions, gated differently on purpose.

    *Did it finish* can only be asked of a run bench.py captured, which
    `host_time` on the `run_start` is what says: hand-built logs and fixtures
    carry a `run_start` but never that stamp, and owe no `run_end` either. A
    real capture predating the contract is reported as unverified rather than
    assumed complete.

    *Did it collect what was asked for* is asked of any run that recorded a
    request, whatever wrote it. That is what `--strict` was missing — it
    checked that expected telemetry appeared, not that the requested capture
    happened, so a `--cycles 10` run cut short after one cycle passed.
    """
    start = next((event for event in run.events if event.get("event") == "run_start"), {})
    warnings: list[str] = []
    if "host_time" in start:
        warnings.extend(capture_stop_warnings(run))
    warnings.extend(request_shortfall_warnings(run, start))
    return warnings


def capture_stop_warnings(run: LabelledRun) -> list[str]:
    """Whether the capture reached a stop condition anyone asked for."""
    end = next((event for event in run.events if event.get("event") == "run_end"), None)
    if end is None:
        return [
            (
                f"{run.label}: the capture recorded no run_end, so it was "
                "killed or the log is truncated; its telemetry is a fragment"
            )
        ]
    completed = end.get("completed")
    if not isinstance(completed, bool):
        return [
            (
                f"{run.label}: the capture recorded no completion status, so "
                "nothing says it ran to a stop condition; recapture with a "
                "current bench.py"
            )
        ]
    if not completed:
        return [
            (
                f"{run.label}: the capture did not complete (stopped by "
                f"{end.get('stop_reason', 'an unrecorded cause')})"
            )
        ]
    return []


def requested_counts(start: dict[str, Any]) -> dict[str, Any]:
    """What a run recorded as its request, across both metadata shapes.

    `requested_page_turns` was the first and only such field; captures
    carrying it still report against it.
    """
    requested = start.get("requested")
    result = dict(requested) if isinstance(requested, dict) else {}
    legacy = start.get("requested_page_turns")
    if isinstance(legacy, int) and "page_turns" not in result:
        result["page_turns"] = legacy
    return result


def is_self_driven(events: list[dict[str, Any]]) -> bool:
    """Whether a selftest scenario, not a hand, drove this capture.

    Every selftest record says so, and so does an injected press, which
    alone carries `action=`. A capture attached late sees no announcement
    but sees the press before the operation it drives.
    """
    return any(
        str(event.get("event", "")).startswith("selftest_")
        or (event.get("event") == "input" and isinstance(event.get("action"), str))
        for event in events
    )


class CheckpointTally:
    """Typed `completed=` checkpoints, counted from where the capture joined.

    The capture loop feeds this one event at a time to decide when to stop,
    and the report feeds it the whole run to decide what was collected, so
    both answer from the same population. The first checkpoint of a kind is
    a sample only if the capture saw the announcement: without it, the
    operation that checkpoint certifies may have begun before the port was
    open, and it is the baseline instead.
    """

    def __init__(self) -> None:
        self.announce_seen = False
        self._base: dict[str, int] = {}
        self._completed: dict[str, int] = {}
        # How many events preceded the baseline checkpoint, per kind: the
        # telemetry of the operations that count starts after it.
        self._window_start: dict[str, int] = {}
        self._seen = 0

    def observe(self, event: dict[str, Any]) -> None:
        self._seen += 1
        kind_name = str(event.get("event", ""))
        if kind_name == "selftest_start":
            self.announce_seen = True
        elif kind_name == "selftest_completed":
            kind = str(event.get("kind", ""))
            n = int(event.get("count", 0))
            if kind not in self._base:
                if self.announce_seen:
                    self._base[kind] = n - 1
                    self._window_start[kind] = 0
                else:
                    self._base[kind] = n
                    self._window_start[kind] = self._seen
            self._completed[kind] = n - self._base[kind]

    def completed(self, kind: str) -> int:
        return self._completed.get(kind, 0)

    def window_start(self, kind: str) -> int:
        return self._window_start.get(kind, 0)


def checkpoint_completions(events: list[dict[str, Any]], kind: str) -> int:
    tally = CheckpointTally()
    for event in events:
        tally.observe(event)
    return tally.completed(kind)


def counted_window(events: list[dict[str, Any]], kind: str) -> list[dict[str, Any]]:
    """The events from the first counted operation of `kind` onward.

    A late attach's first checkpoint is a baseline, and the telemetry of the
    operation it certifies must not be measured against the request either:
    that operation's `folder_leave` is in the stream while its enter is not.
    The run's own `run_start` and `run_end` are kept so the window still
    reads as a run.
    """
    tally = CheckpointTally()
    for event in events:
        tally.observe(event)
    start = tally.window_start(kind)
    return [
        event
        for index, event in enumerate(events)
        if index >= start or event.get("event") in ("run_start", "run_end")
    ]


def request_shortfall_warnings(run: LabelledRun, start: dict[str, Any]) -> list[str]:
    """Each thing the capture was asked for, against what it came home with."""
    requested = requested_counts(start)
    if not requested:
        return []
    warnings: list[str] = []
    end = next((event for event in run.events if event.get("event") == "run_end"), None)

    seconds = requested.get("seconds")
    # A selftest scenario that finished everything it set out to do ends the
    # capture itself, and the operator's --seconds was the ceiling the docs
    # tell them to pass, not a window owed. Holding such a run to the window
    # failed every storage-cache selftest on "185s of the 500s requested",
    # with the scenario's own `result=done` sitting in the same log. Only
    # `done` earns the exemption: any other result is already a strict
    # failure, and a `duration` or `stream-ended` stop keeps the contract.
    finished_itself = (
        end is not None
        and end.get("stop_reason") == "selftest"
        and any(
            event.get("event") == "selftest_done" and event.get("result") == "done"
            for event in run.events
        )
    )
    # A run with no `run_end` at all is already reported as truncated, so the
    # duration check stays quiet rather than saying the same thing twice.
    if isinstance(seconds, int) and end is not None and not finished_itself:
        elapsed = end.get("elapsed_s")
        if not isinstance(elapsed, (int, float)) or isinstance(elapsed, bool):
            warnings.append(
                f"{run.label}: {seconds}s were requested but the capture "
                "recorded no length to check that against"
            )
        elif elapsed + CAPTURE_DURATION_TOLERANCE_S < seconds:
            warnings.append(
                f"{run.label}: {elapsed:.0f}s of the {seconds}s requested were "
                "captured; the run is short of the window it was asked for"
            )

    # A request for N samples is met by two counts, and a self-driven
    # capture owes both. The firmware's typed checkpoints, counted from
    # where the capture joined, say the device completed N operations with
    # their postconditions checked; without that count a late attach one
    # checkpoint short of its target that ran on to `result=done` passed on
    # the partial operation the stop rule had excluded. The telemetry the
    # statistics are drawn from says the host captured N measurements; a
    # checkpoint survives a dropped render line, and a report of fifty turns
    # over forty-nine samples would be certified against the wrong
    # population. A manual capture carries no checkpoint and owes only the
    # telemetry.
    self_driven = is_self_driven(run.events)

    def short(
        label: str,
        requested_n: int,
        measured: int,
        completed: int | None,
        detail: str = "",
    ) -> None:
        if completed is not None and completed < requested_n:
            warnings.append(
                f"{run.label}: {completed} of {requested_n} requested {label} "
                "captured; the run is short of the sample count it was asked "
                "for"
            )
        elif measured < requested_n:
            if completed is None:
                warnings.append(
                    f"{run.label}: {measured} of {requested_n} requested {label} "
                    f"captured{detail}; the run is short of the sample count it "
                    "was asked for"
                )
            else:
                warnings.append(
                    f"{run.label}: the device completed {completed} {label} but "
                    f"only {measured} of the {requested_n} requested were "
                    f"measured{detail}; the telemetry for the rest is missing "
                    "from the capture"
                )

    turns = requested.get("page_turns")
    if isinstance(turns, int):
        window = counted_window(run.events, "page_turn") if self_driven else run.events
        short(
            "page turns",
            turns,
            len(page_turn_stats_over_epochs(window).durations),
            checkpoint_completions(run.events, "page_turn") if self_driven else None,
        )

    entries = requested.get("folder_entries")
    if isinstance(entries, int):
        window = counted_window(run.events, "folder_leave") if self_driven else run.events
        # A round trip is measured only with both halves present: the enter
        # and leave populations feed separate budgets, and five leaves over
        # four enters is four round trips, not five.
        enters = len(folder_samples(window, "folder_enter"))
        leaves = len(folder_samples(window, "folder_leave"))
        short(
            "folder round trips",
            entries,
            measured_folder_round_trips(window),
            checkpoint_completions(run.events, "folder_leave") if self_driven else None,
            detail=f" ({enters} entries and {leaves} leaves)",
        )

    cycles = requested.get("sleep_cycles")
    if isinstance(cycles, int):
        completed = completed_sleep_cycles(run.events)
        if completed < cycles:
            warnings.append(
                f"{run.label}: {completed} of {cycles} requested sleep cycles "
                "completed; the run is short of the cycle count it was asked "
                "for"
            )

    modes = requested.get("storage_modes")
    if isinstance(modes, list):
        for mode in modes:
            name = str(mode)
            if name not in STORAGE_MODE_EVIDENCE:
                warnings.append(
                    f"{run.label}: unrecognised storage mode {name}, so "
                    "nothing checked that it was exercised"
                )
                continue
            verdict = storage_mode_evidence(run.events, name)
            if verdict == STORAGE_MODE_ABSENT:
                warnings.append(
                    f"{run.label}: --{name} was requested but the capture "
                    f"shows no {name} storage path ("
                    f"{STORAGE_MODE_EVIDENCE[name]})"
                )
            elif verdict == STORAGE_MODE_UNVERIFIED:
                warnings.append(
                    f"{run.label}: --{name} cannot be verified from this "
                    "capture: its catalog telemetry predates the result field "
                    "that says whether the operation succeeded, so nothing "
                    "here proves the path ran — reflash and recapture"
                )
    return warnings


def warn_if_unobserved(
    warnings: list[str],
    section: str,
    budgets: dict[str, Any],
    key: str,
    runs: list[LabelledRun],
    per_run: list[list[int]],
    what: str,
) -> None:
    """Report a budget that is configured but had nothing to measure.

    `warn_if_above` and `warn_if_below` return silently when the value is
    `None`, which is right for an exploratory report and wrong for a gate:
    a run missing the telemetry a budget covers passed `--strict` while
    enforcing nothing, which is the same "configured but not protecting
    anything" shape this harness exists to stop. A budget with zero samples
    is now a warning, so a non-strict report says so and `--strict` fails.

    Checked per capture, not per pool. `--all` reports several runs together,
    and a run that never produced the telemetry a budget covers is not
    excused by a sibling that did: two sleep-sync captures, one holding only
    a Full refresh and the other only a completed sleep, between them
    satisfied every check while neither was a complete capture. Each run is
    named so the incomplete one can be found.

    Callers only reach this for a section whose workflow the log actually
    contains, so a page-turn capture is never faulted for holding no storage
    telemetry.
    """
    if budgets.get(key) is None:
        return
    # `per_run` is built from `runs` one-for-one by `per_run_samples`, so a
    # length mismatch would mean a caller paired the wrong two lists and
    # silently checked coverage against the wrong capture.
    for run, samples in zip(runs, per_run, strict=True):
        if samples:
            continue
        warnings.append(
            f"[{section}] {key} is configured but nothing was measured against "
            f"it: no {what} in {run.label}"
        )


# Which workflows produce the measurements each budget section gates. A
# section is named after the workflow that owns it, but `reader-soak` is page
# turns plus navigation, so it answers to the page-turn budgets too. Nothing
# else is folded in: reader-soak sleeps once, so pointing it at `sleep-sync`
# would fault a healthy capture for a Full refresh it never owed.
#
# Keyed on workflow rather than suite so `thermal-run --suite sleep-sync`
# resolves to the sleep-sync budgets, which is the whole point of the flag.
# The map is also the isolation boundary — a section measures only the
# workflows listed for it, so pooling with `--all` cannot let one capture's
# samples decide another's verdict.
BUDGET_SECTION_WORKFLOWS: dict[str, set[str]] = {
    "page-turn": {"page-turn", "reader-soak"},
    "sleep-sync": {"sleep-sync"},
    "storage-cache": {"storage-cache"},
    "folder-nav": {"folder-nav"},
}


@dataclass(frozen=True)
class LabelledRun:
    events: list[dict[str, Any]]
    suite: str | None
    workflow: str | None
    # How a warning names this run. Carries its position in the log whenever
    # there is more than one, because "no Fast refresh events" is only
    # actionable if the operator can tell which capture is missing them.
    label: str


def labelled_runs(events: list[dict[str, Any]]) -> list[LabelledRun]:
    """Each run with the suite and workflow it was captured under.

    Both are properties of the run, not of the event: `run_start` is the only
    line that carries `workflow`, and hand-built fixtures and older captures
    put even `suite` only there. Every event therefore takes its run's labels
    rather than being dropped from every section for want of its own.

    A run with no recorded `workflow` falls back to its suite name. For
    `thermal-run` that resolves to nothing a budget section claims, and is
    reported — the alternative is assuming the argparse default and gating a
    `--suite sleep-sync` capture as if it were a page-turn run, which is the
    guess that made this worth fixing.
    """
    split = split_runs(events)
    runs = []
    for index, run in enumerate(split):
        suite = next(
            (str(event["suite"]) for event in run if isinstance(event.get("suite"), str)),
            None,
        )
        start = next((event for event in run if event.get("event") == "run_start"), {})
        workflow = start.get("workflow")
        workflow = str(workflow) if isinstance(workflow, str) else suite
        name = workflow or "unlabelled"
        if suite is not None and suite != workflow:
            name = f"{suite}/{name}"
        label = name if len(split) == 1 else f"{name} run {index + 1} of {len(split)}"
        runs.append(LabelledRun(run, suite, workflow, label))
    return runs


def section_runs(events: list[dict[str, Any]], section: str) -> list[LabelledRun]:
    """The runs a budget section is allowed to measure.

    Choosing *which* sections to evaluate was never enough: the measurements
    themselves were taken from the whole pooled log, so a file holding a
    `page-turn` run next to a `storage-cache` or `sleep-sync` one had its
    strict verdict decided partly by samples from a workflow that budget does
    not describe — a false pass or a false failure, either way from the wrong
    population.

    Returned as runs rather than a flat event list because coverage is a
    property of each capture too: pool first and ask "were there any
    samples?" afterwards, and one healthy run answers for a sibling that
    produced nothing. Every caller pools for its statistic and checks
    coverage per run — see `per_run_samples` and `warn_if_unobserved`.

    A log with no label anywhere measures everything: there is nothing better
    to go on. Once *any* run is labelled the unlabelled ones are left out
    rather than folded in, and `budget_sections_in_play` reports the mixture.
    """
    workflows = BUDGET_SECTION_WORKFLOWS.get(section, {section})
    runs = labelled_runs(events)
    if all(run.workflow is None for run in runs):
        return runs
    return [run for run in runs if run.workflow in workflows]


def per_run_samples(
    runs: list[LabelledRun],
    extract: Callable[[list[dict[str, Any]]], list[int]],
) -> tuple[list[list[int]], list[int]]:
    """Each run's samples, and the pooled list built out of them.

    Coverage reads the first, the percentile or median reads the second.
    Deriving the pool from the parts rather than measuring the concatenated
    stream keeps the two from ever disagreeing about what was sampled.
    """
    per_run = [extract(run.events) for run in runs]
    return per_run, [sample for samples in per_run for sample in samples]


def budget_sections_in_play(
    events: list[dict[str, Any]], budgets: dict[str, Any]
) -> tuple[set[str], list[str]]:
    """Budget sections whose workflow this log actually contains, plus warnings.

    Every section used to be evaluated against every log, which was harmless
    while an absent metric silently skipped its check and is not once that
    absence is a warning. Logs whose runs carry no label at all — hand-built
    fixtures, and any capture predating suite tagging — keep the old
    behaviour of evaluating everything, since there is nothing better to go
    on.

    Two things are reported rather than passed over, because both mean a
    capture ran and nothing gated it, which is the silently-unenforced shape
    this file exists to prevent: a known workflow no configured section
    claims, and an unlabelled run pooled with labelled ones — the latter is
    excluded from every section, and reading it into whichever run happened
    to precede it in the file list would be worse than saying so.
    """
    if not budgets:
        # Budgets deliberately disabled, or unloadable — reported elsewhere.
        return set(), []
    per_run = [run.workflow for run in labelled_runs(events)]
    labelled = {label for label in per_run if label is not None}
    if not labelled:
        return set(budgets), []
    sections = {name for name in budgets if labelled & BUDGET_SECTION_WORKFLOWS.get(name, {name})}
    warnings = []
    for workflow in sorted(labelled):
        if any(workflow in BUDGET_SECTION_WORKFLOWS.get(section, {section}) for section in budgets):
            continue
        # Every unclaimed workflow, not only the ones this bench.py knows: a
        # misspelled or newer name matches no section either, and skipping
        # the warning for it let malformed metadata buy silence.
        unknown = "" if workflow in SUITES else " (not a workflow this bench.py knows)"
        warnings.append(f"workflow {workflow} has no budget section to check it against{unknown}")
    unlabelled = sum(1 for label in per_run if label is None)
    if unlabelled:
        warnings.append(
            f"{unlabelled} of {len(per_run)} runs carry no suite label and are "
            "outside every budget section; report those logs on their own"
        )
    return sections, warnings


def warn_if_above(
    warnings: list[str],
    label: str,
    actual: float | None,
    threshold: Any,
) -> None:
    if actual is None or not isinstance(threshold, int):
        return
    if actual > threshold:
        warnings.append(f"{label} {actual:.0f}ms above warning budget {threshold}ms")


def warn_if_below(
    warnings: list[str],
    label: str,
    actual: float | None,
    threshold: Any,
) -> None:
    if actual is None or not isinstance(threshold, int):
        return
    if actual < threshold:
        warnings.append(
            f"{label} {actual:.0f}ms below plausibility floor {threshold}ms "
            "(burst cadence or dropped presses, not a faster device)"
        )


def percentile(data: list[int], pct: int) -> float:
    if len(data) == 1:
        return float(data[0])
    ordered = sorted(data)
    index = (len(ordered) - 1) * pct / 100
    lower = int(index)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = index - lower
    return ordered[lower] * (1 - fraction) + ordered[upper] * fraction


def run_channel_stress(args: argparse.Namespace) -> int:
    if not args.host:
        raise SystemExit("channel-stress currently requires --host")
    model = ChannelStressModel()
    model.run()
    print("channel-stress: ok")
    for check in model.checks:
        print(f"  {check}")
    return 0


class ChannelStressModel:
    """Tiny host model for the firmware's coalescing contract."""

    def __init__(self) -> None:
        self.rendering = False
        self.render_pending = False
        self.state_page = 0
        self.rendered_pages: list[int] = []
        self.pending_storage: int | None = None
        self.latest_request_id = 0
        self.reader_section_loaded = False
        self.loading_plate_painted = False
        self.checks: list[str] = []

    def run(self) -> None:
        self.input_page_turn()
        self.input_page_turn()
        self.input_page_turn()
        assert self.rendering
        assert self.render_pending
        self.display_settled()
        assert self.rendering
        assert not self.render_pending
        self.display_settled()
        assert self.rendered_pages == [1, 3]
        self.checks.append("input during render coalesces to latest reader state")

        stale = self.open_request()
        fresh = self.open_request()
        assert stale < fresh == self.latest_request_id
        assert not self.storage_request_is_current(stale)
        assert self.storage_request_is_current(fresh)
        self.checks.append("stale open/extend requests are rejected by request id")

        self.storage_wins_first_open()
        assert self.loading_plate_painted
        self.checks.append("storage-first cold book open paints a loading plate")

        self.pending_storage = 1
        self.sleep()
        assert not self.rendering
        assert not self.render_pending
        self.checks.append("sleep clears render in-flight and pending-render state")

        refused = {"OpenBook", "ExtendSection", "LoadChapters", "JumpChapter", "LoadCatalogCache"}
        admitted = {"StoreProgress", "StoreWifiCredentials", "ReceiveUpload"}
        assert all(not sync_loaned_admits(command) for command in refused)
        assert all(sync_loaned_admits(command) for command in admitted)
        self.checks.append("sync session admits only progress, credentials, and upload after loan")

    def input_page_turn(self) -> None:
        self.state_page += 1
        if self.rendering:
            self.render_pending = True
        else:
            self.rendering = True
            self.rendered_pages.append(self.state_page)

    def display_settled(self) -> None:
        self.rendering = False
        if self.render_pending:
            self.render_pending = False
            self.rendering = True
            self.rendered_pages.append(self.state_page)

    def open_request(self) -> int:
        self.latest_request_id += 1
        return self.latest_request_id

    def storage_request_is_current(self, request_id: int) -> bool:
        return request_id == self.latest_request_id

    def storage_wins_first_open(self) -> None:
        self.loading_plate_painted = False
        self.reader_section_loaded = False
        if not self.reader_section_loaded:
            self.loading_plate_painted = True
        self.reader_section_loaded = True

    def sleep(self) -> None:
        self.rendering = False
        self.render_pending = False


def sync_loaned_admits(command: str) -> bool:
    return command in {"StoreProgress", "StoreWifiCredentials", "ReceiveUpload"}


if __name__ == "__main__":
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    raise SystemExit(main())
