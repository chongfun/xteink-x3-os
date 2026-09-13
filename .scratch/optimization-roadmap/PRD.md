# PRD: CalendulaOS optimization roadmap

Status: **WS-A reopened, and it has now paid out.** **A13 landed as #89 on
2026-09-04**, the first item shipped since WS-A was reopened and the first
in this document verified by a before-and-after device capture rather than
by a single measurement: predicted −200 ms on every FastClean, measured
−199, A12 followed as #92 on 2026-09-11 at −73, and A14 as #98 on 2026-09-12. C2 is now the top of Tier 1. A new WS-G owns app-state render
invalidation; the bench harness has an owner for the first time. **Tier 0 is
implemented** (`opt/tier0-measurement-integrity`) and **the double repaint is
merged** (#56), so the two largest items in the round are resolved within it.
Updated 2026-07-30 after a seven-region survey, a branch audit, and a
code-reviewed implementation of Tier 0. Started 2026-07-09 from six parallel
code-survey agents, one per workstream, scoped to mostly-disjoint code regions
so work can proceed in parallel.

**Reconciled against `main` 2026-08-13, after #75 (long-name uploads,
journalled install) landed and v0.6.0 shipped.** #75 is not a performance
change, but it moved the `embedded-sdmmc` pin to our own fork and rebased it
onto upstream v0.10, and that touches this roadmap in two places worth reading
before picking anything up: **D6 lost its cost side** (it was rated for the
expense of *creating* a fork we now maintain anyway, so it re-rates L → M), and
**Tier 3's "none needs a rebase" is no longer true** (`opt/b7-per-config-section-caches`
conflicted, and the fix was a driver API migration rather than a rebase; that
branch is now retired, see Tier 3). WS-D's upload baseline predates #75's
write path and needs re-taking before it is quoted again.

**Reconciled again 2026-09-03, against `main` at #88 and against the four
library PRDs** (`library-identity`, `source-identity`, `physical-folder-library`,
`reading-position-and-layout-durability`), which were written while this
document sat still. Ten merges landed in between, #79 through #88. What that
sweep changed:

- **B7 is retired**, superseded by Reading Position and Layout Durability
  Milestones 3 and 4. Tier 3 says why; issue 02 keeps the eleven findings the
  branch paid for, because its successor inherits every one of them.
- **Tier 1 was re-verified line by line and stands unchanged.** A13's
  `SETTLE_MS`, A12's unvaried `CDI_INTERVAL`, the absent frame-identity guard
  behind A14, and C2's untouched sleep path are all still there. WS-A and WS-C
  saw no substantive commits in the interval, so the top of the queue is the
  part of this document that has aged best. **A13 has since landed as #89**,
  measured on device and struck from Tier 1; the other three still stand.
- **Tier 3 lost its second branch and its clean-merge claim.**
  `opt/upload-session-token` no longer merges.
- **F11 landed** inside #58 and is struck from item 11. F10 stands.
- **Two workstreams no longer own their regions alone.** The library work is
  implemented in WS-B's `proto/` and `reader-cache/` and in WS-D's
  `upload-store/` and `sd_session.rs`, from a different document. See
  Workstreams and Coordination hazards.
- **WS-B's pipeline baselines predate folder browsing** and are marked as
  such.

This document is kept to three things: **what is left**, **what has been
done**, and **what not to do**. Round-by-round history has been dropped —
it lives in the git log and the PR descriptions, which are the honest
record. What survives from it is only what still changes a decision.

## Goal

Ship the highest-ROI performance, battery, and size improvements across the
firmware and web emulator. Every item cites a measured baseline, not a guess.

## The queue

**Re-ranked 2026-07-30** after a seven-region survey and an audit of the six
in-flight branches. Three things moved the ranking more than any single new
item:

- **The measurement layer is partly broken and largely unread**, and every
  ranking in this document is downstream of it. Tier 0 exists for that reason
  and comes first.
- **"Stop optimizing the render path" was wrong**, and wrong for a specific
  reason worth remembering: the reasoning behind it was an **X4** statement
  applied to the **X3**. See WS-A's reopened status.
- **Six branches are already written and none needs a rebase** — all six sit
  on the current tip of `main` and all 15 pairs merge cleanly, so landing
  order is unconstrained by conflicts. Rank on correctness and residual work,
  not on cost-to-build. Three are much closer to done than their old queue
  positions implied; two have defects that must be fixed first.
  **Expired 2026-08-13.** Of the six, four have merged,
  `opt/b7-per-config-section-caches` is retired as of 2026-09-03, and
  `opt/upload-session-token` is the only one still live. The *ranking
  principle* still holds. Rank on correctness and residual work. But
  "landing order is unconstrained by conflicts" no longer describes the
  tree. See Tier 3.

### Tier 0 — measurement integrity (do first; hours, not days)

Nothing below this line can be honestly ranked until these land. Three rounds
of misranking — A10, A7, the retired 354 ms baseline — came from this layer.

**MERGED as #58**. What each item turned out to be:

| # | Item | Outcome |
|---|---|---|
| 0a | `--strict` silently checked nothing on Python < 3.11 | Fixed. It now refuses to run without a TOML parser, naming the version and `pip install tomli`; non-strict warns visibly. **Re-check anything previously signed off "with `--strict`"** — on this machine it verified nothing. |
| 0b | Telemetry already captured, consumed by nothing | Fixed. `storage_build` / `storage_first_page` / `storage_background_build` are summarized, and boot-to-first-paint is reported **per boot kind**. Dead `section_extend_warn_ms` deleted, with a test that fails on any future dead key. |
| 0c | `page turn` was a function of tapping rhythm | Fixed, and it was worse than diagnosed — see below. |
| 0d | Serial logging blocks untethered | Seam built: default-on `serial-log`, inert by default. Scope is honest in the Cargo.toml comment — it does *not* yet cover the SD session's per-call lines or most of `book_build`'s chatter. The confirming device measurement is still untaken. |

**0c's root cause was the double repaint, which makes it the most instructive
result of the round.** Pairing was render-driven — each render popped a press
off a FIFO — so the second repaint of turn N consumed press N+1 and credited
it with a near-zero duration. **The #1 optimization item was corrupting the
primary metric.** Pairing is now begin-time (`t_ms − layout_ms − flush_ms`),
one duration per render from the newest press it answers, with superseded
presses counted as `coalesced`; trust gates on `(coalesced + unmatched)`,
because an unmatched-only gate is structurally blind whenever renders ≥
presses — the normal shape of these captures.

On the reference capture: **median 476 → 477 ms, p95 33,681 → 2,991 ms, min
2 → 462 ms.** The median was always right and the tails were fiction. **So the
~424 ms press-to-settled baseline stands** — it does not need retiring, which
is the opposite of what was expected when this started.

**Three things fell out of doing it:**

1. **The first real split of a build.** Of a ~63 s full build: `build spine`
   62,942 ms, `build write` 11,324 ms — section writes are ~18%, and the
   spine walk is essentially the whole build. The fixture holds no *replay*
   events, so this does not yet size the 24–27 s replay directly; but a replay
   does the same writes while skipping zip/inflate/XML, which would put writes
   near 40% of it and raise WS-B's write-alignment item.
2. **`wr_calls == wr_blocks` (18,626) and `rd_calls == rd_blocks`** on fresh
   data — D6's precondition, still holding.
3. **A live gate is miscalibrated.** `[sleep-sync] full_refresh_busy_min_ms =
   3000` against a measured X3 Full busy of **928–930 ms** — the same stale
   "~3.5 s Full" figure this round retired, sitting in an enforced budget.
   Now that `--strict` actually works, **every strict X3 sleep-sync run will
   fail on it.** Left untouched deliberately: it is a real finding about the
   budget, not about the harness. **Re-sized in PR #91 (2026-09-10)** to a
   300 to 1100 ms bracket, once the unattended baseline showed the X3's Full
   is two BUSY intervals, 928 ms of waveform and a 379 ms clean pass, both
   logged as `mode=Full`. The warm-open budget was re-sized in the same PR,
   150 to 200 ms, from a 65 to 137 ms population on the foldered card.

### Tier 1 — large, cheap, high confidence

| # | Item | WS | Why it ranks here | Effort |
|---|---|---|---|---|
| ~~1~~ | ~~**A13, FastClean's 200 ms trailing settle**~~ | A | **LANDED as #89, 2026-09-04.** Predicted −200 ms; measured −199 on an X3 A/B. See Landed. | done |
| ~~2~~ | ~~**A12, the 136 ms that is not waveform drive**~~ | A | **LANDED as #92, 2026-09-11.** The CDI nibble was the suspect and the fix: `0x9` to `0xF`. Predicted 77 to 100 ms off every refresh; measured **73 ms** on an unattended X3 A/B, Fast BUSY 379 to 306 and a page turn 426 to 353. See Landed. | done |
| ~~3~~ | ~~**A14, frame-identity guard at the flush seam**~~ | A | **LANDED as #98, 2026-09-12.** Measured before it was built: 3.5% of renders byte-identical and 29 of 29 loading plates, against a 0.17% break-even. On the X3, one soak skipped 8 renders and 29 plates, about 12.3 s of panel time, with a 50-turn page turn unchanged at 355 ms. The compare early-exits, so a miss costs 14 to 16 µs and only a hit pays 4.6 ms. See Landed and issue 01. | done |
| 4 | **C2** — measure sleep current with the fuel gauge, then hold GPIOs if it indicts them | C | **Unblocked 2026-07-30: the first-line experiment needs no meter and no disassembly.** The X3's BQ27220 sits on the battery and keeps integrating while the SoC is in deep sleep, so a charge-register read, a 24–72 h sleep, and a second read give average standby draw. Over 48 h, 15 µA is 0.72 mAh against 300 µA's 14.4 mAh — decisive even at 1 mAh resolution, and a null result *is* the answer. Cost is one register and one `println!`. The series meter drops to a follow-up for if it comes back high. **The "which GPIOs" half now has two named suspects from the 2026-08-06 upstream sweep, so a high reading has somewhere to go: (a) the X3 SD rail on GPIO13 — we drive that pin nowhere and so never cut the card for sleep, and freeink confirmed the pin by factory-firmware RE (`x3-sd-rail-sleep-power`); and (b) the panel RST line floating in deep sleep, closed unmerged as PR #70, which upstream reports as ~36 h-to-dead on a UC8179 while calling the SSD1677 tolerant. Neither is confirmed on our hardware and the gauge cannot separate them, but the card can be removed outright for a control run, making (a) the cheaper one to isolate.** **Sharpened 2026-08-13, then corrected the same day.** Upstream's 12.8 µA X3 figure is measured **with GPIO13 driven low and latched** (`HalPowerManager.cpp:89-90`), so it bounds what the hardware can reach rather than describing a board left alone — which makes the SD rail a *stronger* suspect, not a settled one. Two further findings moved this item: **GPIO13 is the C3's flash SPIWP pad** (so the card powers up because the pin is muxed to flash at boot, not because anything holds it high — a third case neither document had), and **our deep sleep never runs esp-hal's digital-pad isolation pass at all**, because it is gated on a hold bit esp-hal never writes. The latter affects every unheld pad, is the more likely explanation for a high reading, and is investigable today with no hardware. Do that before the 72-hour gauge run. Also: crosspoint `9b1fb712` guards GPIO13 to Xteink C3 boards, so a fix must be board-gated. | S |

### Tier 2 — gated on a Tier 0 measurement

| # | Item | WS | Why it ranks here | Effort |
|---|---|---|---|---|
| 5 | **Upload instrumentation, then D6** | D | The D2 post-mortem has been misread: it tested write *stalls*, never write *bandwidth*. Arithmetic says **~72% of upload wall time is single-block SD writes** and the ceiling is 1.4× above observed. D6's own precondition was met in July and it stayed deferred. ~25 lines of instrumentation resolve it in one capture. **Re-rated 2026-08-13: #75 removed D6's cost side.** It was sized as "L, fork maintenance" and carried a do-not-fork escape hatch; we now own and maintain that fork, so the payoff is unchanged and the cost is sunk. Effort drops L → M. The instrumentation still goes first — the 72% is arithmetic, not a measurement — but its job is now sizing, not deciding whether to fork. Re-take the baseline before quoting it: #75 changed the upload write path. **And re-take it again after #87 (2026-09-03):** `StagedUpload::write` now feeds every byte through a streaming SHA-256 (`upload-store/src/install.rs:1329`), so upload wall time carries a hashing cost that was not in the arithmetic behind the 72% figure. The instrumentation has to separate hash from write, or the split it reports credits the SD path with CPU time and overstates D6. | S–M then M |
| 6 | **Cache write alignment** | B | Nothing arranges block-aligned writes, so CONT.BIN pays **2 writes + 1 read per 512 B**. Modelled at ~1.75× write amplification, cross-checked against the 2026-07-09 537-block measurement. No format change, no version bump. | S–M |
| 7 | **E5 + E6 — halve the peak stack chain** | E | `CssRules` ~6.9 KB and `parse_opf`'s duplicated manifest+spine 5,896 B, both on the peak reader chain (26,768 B of 42,136). Two mechanical changes take it to ~14.8 KB. | S–M, M |

### Tier 3 — in-flight branches, ranked by residual work

~~All sit on current `main`; none needs a rebase.~~ **False as of #75
(2026-08-13).** The driver pin moved to our fork and rebased onto upstream
v0.10, which is a breaking API change for anything touching SD:
`delete_file_in_dir` → `delete_entry_in_dir`, `CardType` moved into
`embedded-sdmmc-types` (`SDHC` → `SdhcSdxc`), and `iterate_dir` callbacks now
return `ControlFlow`. **Any branch predating #75 that touches `reader-cache/`
or `sd_session.rs` needs that migration, not a rebase.** Verified by
`git merge-tree` against `origin/main`, not assumed, and re-run 2026-09-03:
`opt/upload-session-token` has since acquired eight conflicts of its own from
#79 and #80, which are renames rather than driver API changes.

| Branch | State | Residual |
|---|---|---|
| ~~`opt/tier0-measurement-integrity`~~ | **MERGED as #58**. | — |
| ~~`opt/font-mono-raster`~~ | **MERGED as #61, pixels superseded 2026-08-05.** The device verdict was mixed — per-glyph grid-fitting plus the two-render re-seat made some glyphs unbalanced. What survives it: the H2 justification fix, the specimen/fingerprint machinery, and the diagnosis its successor is built on (issue 08, H1/H5). | — |
| ~~`opt/font-aa-low-threshold`~~ | **MERGED as #72**. | — |
| ~~`opt/prune-orphan-sections`~~ | **MERGED as #59**. | — |
| ~~`opt/a11-landscape-glyph-batching`~~ | **MERGED as #57**. | — |
| `opt/upload-session-token` | **No longer merges, as of 2026-09-03.** It conflicts with `main` in eight files: `app-core/src/lib.rs`, `ui/src/lib.rs`, `ui/src/app_render.rs`, `ui/src/join_qr.rs`, `fw/src/tasks/wifi.rs`, `tools/emulator/src/scenario.rs`, and both `fixtures/golden/sync-portal-qr*.png`. The cause is #79 and #80, which renamed the card layout and the onboarding hotspot per device and re-took those goldens, so the conflict is a real rename to follow rather than a textual one, and the two golden conflicts resolve by regenerating rather than by picking a side. The original caveat also stands: it adds 68 lines to `fw/src/tasks/wifi.rs`, and #75 introduced refusal paths the token gate predates, since uploads and deletes are refused while an install journal stands. Confirm the gate covers those before the device check rather than after. | Resolve eight conflicts, regenerate two goldens, re-verify the gate, then device check |
| ~~`opt/inflate-caller-owned-window`~~ | **MERGED as #63**. | — |
| ~~`opt/d4-directed-wifi-join`~~ | **MERGED as #73**. | — |
| ~~`opt/single-repaint-per-page-turn`~~ | **MERGED as #56**, reworked first. The audit found it suppressed the `Loaded` *event* rather than the render, freezing the app's page count during a background build and stranding the reader at the frontier — rule 4 through a door rule 4 does not name. #56 moved the decision into `app_core::loaded_repaints` and kept the event unconditional, and picked up an open-gate latch and a failed-refresh retry on the way. | — |
| ~~`opt/b7-per-config-section-caches`~~ | **RETIRED 2026-09-03.** Superseded by the Reading Position and Layout Durability PRD, whose Milestones 3 and 4 build the same cache from a different key: `(LocalCacheIdentity, LayoutId)` over a ContentAnchor, rather than the layout config over a page index. Two things settled it, neither of them the three known defects. **#88 replaced the cache key the branch names its files from.** Identity now derives from root, full locator and byte size, not from the display label the branch's `S<cfg><nnn>.BIN` scheme was built over, so the port is no longer "three fixes plus a v0.10 migration". And **its position model is the page index Milestone 2 removes**, so landing it would mean migrating reading positions twice. The branch is not rebased and not merged. It stays readable for its tests and its findings, which are carried into that PRD and enumerated in issue 02. | None. Do not rebase it. |

### Tier 4 — worthwhile, unblocked, smaller

| # | Item | WS | Why | Effort |
|---|---|---|---|---|
| 8 | **First open on a deep resume** | B | B4 gives nothing near the end of a book; only a progress indicator helps. | M |
| 9 | **C8 — sleep entry's discarded second pass** | C | ~657 ms of the ~4.1 s sleep entry, thrown away by the next boot's `init_panel`. Refuting check is free (count `bench: refresh` per sleep in an existing capture). Risk gates it. | M + hw |
| 10 | **C9 — recovery combo on every wake** | C | 28 ms + 48 ADC conversions that cannot succeed on a button wake. One-line gate. | S |
| 11 | **F10, close the last CI hole** | F | `tools/web-emulator` is built by **no PR gate**, so a broken wasm merges green: `pages.yml` has the right path filters but fires only on `push` to `main`, and `ci.yml` does not build it at all. ~0 CI wall clock. **F11 is done.** #58 added a "Test bench harness" step running `tools/check.sh test-bench`, so the harness's own tests are gated on every PR. | S |
| 12 | **D5** — portal → station handoff | D | ~40–60 s and 3 steps off first-time onboarding. | M |
| 13 | **E7** — `sort_unstable_by_key` in the wifi scan | E | One stable sort of 20 elements costs a 4,128 B frame and 3.8 KB of flash; stability is irrelevant there. Cheapest item in the roadmap. | S |
| 13a | **C11 — scale the CPU clock down when nothing needs 160 MHz** | C | **New 2026-08-13 from crosspoint `70faa29d`.** We set 160 MHz once at `fw/src/main.rs:367` and never vary it. Upstream measured a 3 s post-turn 160 MHz tail at 21.2 mA on an X3. Unlike C10 this touches no timer domain and is not blocked. Impact deliberately unestimated — but note our BUSY wait already sits in WFI on a GPIO edge rather than spinning, so the lever is clock-tree only and **upstream's 3.2× is not this item's number**. **Run it on C2's gauge rig, not separately** — same instrument, same untethered requirement. | S–M |
| 14 | **F5 re-scoped** | F | Merriweather is **42.9% of the wasm** and is not the default face: −41% on *every* first visit, not just a board switch. The old deferral reasoning was wrong. | L |

### Unresolved — measure before ranking

**A15 — `fill_plane`'s 528 single-row transfers.** Two surveys costed it and
disagree by an order of magnitude (~87–139 ms vs ~10 ms), and neither
reconciles a third data point. The per-transaction model is not identified.
~20 minutes on device settles it; nobody should write code against it first.

Unscheduled, deliberately: **A4, A5** (hardware-risk display experiments, and
A5 is X4-only — the owner has no X4), **C6** and **C10** (both gated on C2's
meter reading), **E4** (flash is 57% used, 2.69 MB spare — less urgent than
recorded), **B5** (bundle-only; and its cost is stack, not time — see issue
02), **B1's remaining levers** (only if custom-font cold builds still measure
slow), **A9** (see below).

**A9 is effectively dead as a bandwidth item.** A plane write is ~26 ms of
which ~20.9 ms is wire time at the datasheet-ceiling 20 MHz, so the entire
addressable overhead is ~5 ms and A9 has at most ~3 ms to win — while
re-spending the 8 KB `TX_BAND` that A3 freed, on the tight board. It survives
only as the honest home for prestage overlap.

## Landed

| Item | PR | Measured result |
|---|---|---|
| A1 — `Settled` before prestage | #12 | Prestage off the page-turn path |
| A2 / A2-P — byte-run rasterizer fast paths | #24, #46 | Landscape layout 16–18 ms; portrait 33 ms (later 13 via #50); menus 82–90 → 3–8 ms |
| A3 — panel-native framebuffer byte order | #46 | Zero-copy `fb.band()` SPI; 8 KB `TX_BAND` freed |
| A6 — O(1) ASCII glyph advance lookup | #47 | Bypasses binary search for printable ASCII and the kerning search on unkerned fonts. No separate measurement — it landed inside the same rasterizer push |
| A10-as-shipped — portrait glyph transpose | #50 | **Portrait reading layout 33 → 13 ms median on X3 (2.5×)**. Not what A10 specified — see "Do not re-propose" |
| B2+B3 — catalog scan + incremental pagination | #10 | O(C+N) sweep; ~100–300 ms per build |
| B6 — settings-independent content cache (CONT.BIN) | #23 | Settings-change rebuild 2.4–2.6× faster than a full build (~37 s saved) |
| B4 — progressive first open | #53 | **Time-to-prologue 45.3 → 32.6 s; page-turn median during a build 1270 → 231 ms.** Reader now runs 15.9 s ahead of the builder |
| Reader-cache crate extraction | #55 | `reader-cache` host-testable; 7 fault-injection tests on an in-memory FAT16 card |
| **Single repaint per page turn** | #56 | Storage emits `Loaded` unconditionally; the repaint decision moved into `app_core::loaded_repaints` with a `text_replaced` flag. **~405 ms of panel time off every page turn in every book** — half the panel duty cycle and half the refresh count. Also fixed an open gate that latched forever when an open was served from RAM, and added a retry when a refresh fails |
| C1+C3+C4+C5 — wake refresh, gauge decimation, idle tiers, boot init | #11, #36 | Wake seeded from the deep-sleep cause; idle tiered 10 min Reading / 3 min menus |
| D1 — SD SPI tier | #14 | Cold build −5.4%, write_ms −9.5%, progress write −35%. **Not** the hoped 2× |
| D3 — portal PSK | #19 | Shipped as a per-session runtime PSK, not the build-time one this PRD proposed |
| E1+E2+E3 — flash and stack budget | — | ~246 KB flash freed, ~7 KB stack headroom both boards, `.data` 52 → 5 KB |
| F1–F4+F6 — web emulator and CI | #13 | Initial transfer −49% gz; reading goldens now run in CI |
| Tier 0 — measurement integrity | #58 | `--strict` works, dead telemetry deleted |
| Font AA low threshold | #72 | Cut shipped faces from one AA render |
| Prune orphan sections | #59 | Deletes section files stranded by shrinking rebuild |
| A11 — landscape glyph batching | #57 | Blit a whole glyph at a time |
| Inflate caller-owned window | #63 | Decode zip entries against caller-owned inflate window |
| D4 — directed wifi join | #73 | Join access point this device last associated through |
| Drop MarigoldOS lineage | #78 | Firmware identity OTA rename |
| Board identity guard | #77 | Refuse to boot on wrong board |
| Panel controller detection | #76 | Probe panel controller before driving |
| **Long-name uploads, journalled install** | #75 | Uploads stage outside `/BOOKS` and install as a same-volume move — two directory writes rather than a copy of the book's length. Not a performance item, but it moved the driver pin to our fork and rewrote the surface three WS-D items are written against. Released as v0.6.0 |
| **Source identity** | #87 | Not a performance item, listed because it changes two of them. `StagedUpload` hashes every byte it writes (`install.rs:1329`), which puts a streaming SHA-256 on the upload path item 5 measures; and it gives content-derived caches an identity, the one the retired B7's successor keys pagination under |
| **Physical folder paths** | #88 | Not a performance item, listed because it retired one. A book is addressed by a root-relative locator and its cache key derives from root, locator and byte size rather than from a display label, which is what made B7 unportable. Also catalogs at depth, so WS-B's scan baselines predate it |

| **A13, FastClean's trailing settle** | #89 | **FastClean `flush_ms` 681 to 482 median on an X3, `busy_ms` unchanged at 456, prestage unchanged at 24.** The 200 ms `DelayMs` moved off the reader's wait and onto the caller: it rides on `FlushPlan::settle_after_ms`, comes back through the flush seam as `PanelSettle`, and the display task holds it after `Settled` and before the prestage it guards. The decisive figure is the software tail (`flush` minus `busy`): Fast was and is 26 ms, FastClean was 226 and is now 26, so the tail was the timer and nothing else. Also −200 on the power-on clean path a wake takes (808 to 608) |

Everything above is on `main`. B7 was committed to a branch and retired
without merging, so it is not listed here; see Tier 3.

## Current measured baselines

**X3, unattended, main `0fdde34` (2026-09-12).** Quote these, not anything
older. All five suites taken by the `bench-selftest` build with nobody at
the keys, `--strict --board x3` clean on four; the reader soak is the
exception and for a fixture reason, below. Captures are `a12-*.jsonl` in
the session scratchpad; the recipe is `docs/agents/bench.md`.

| Metric | Value |
|---|---|
| **Page turn, press-to-settled** | **353 ms** median, 357 p95, 339 min, 361 max, queue wait 0 |
| Render flush, Fast | 332 ms median (306 ms of it panel BUSY) |
| FastClean BUSY | 383 ms |
| Full refresh | two BUSY intervals per Full render: 856 ms waveform + 307 ms clean pass |
| Reading layout, portrait | 15 ms median / 17 p95 |
| Prestage | 24 ms |
| Progress write | 74 ms (card-bound, see below) |
| Warm book open (cache built, RAM miss) | 38 to 89 ms, median 55 (n=42) |
| Catalog load | 27 ms |
| Folder enter | 27 ms median |
| Folder leave | 15 ms |
| Boot to first paint, cold | 2,863 to 3,170 ms |
| Boot to first paint, timer wake | 1,933 ms median |

Three changes separate cleanly across the captures on disk, which is worth
recording because a single before-and-after would have credited all of it
to whichever landed last:

| Metric | 8 GB card, 72b24f3 | 64 GB card, 72b24f3 | + #94 | main 0fdde34 |
|---|---|---|---|---|
| Fast BUSY | 379 ms | 379 ms | 379 ms | **306 ms** |
| Folder enter | 85 ms | 29 ms | **26 ms** | 27 ms |
| Folder leave | 40 ms | 18 ms | **15 ms** | 15 ms |
| Progress write | 88 ms | **73 ms** | 73 ms | 74 ms |
| Catalog load | 47 ms | **26 ms** | 26 ms | 27 ms |

So A12 moved the display and nothing else, #94 moved navigation and nothing
else, and the card swap moved every storage figure. A12's 73 ms matches the
72 ms its own pull request measured by hand, taken a different way.

**The reader soak is the one suite that did not certify, and the harness was
right to say so.** Run last in a five-suite sequence, it opened a book the
earlier suites had already turned 104 pages into, met the end at page 302,
wrote `invalid=end-of-book` twice and `result=short-turns`, and refused to
certify. A full sequence consumes roughly 150 pages, so it needs a book at
least that far from its end, or the soak run first. Recorded in
`docs/agents/bench.md` rather than treated as a defect.

**Display, X3, deliberate cadence, 50 turns (2026-07-27, main `e9163b3`).**
Superseded twice over; kept for the comparison.

| Metric | Value |
|---|---|
| Reading layout, portrait | **13 ms** median / 14 p95 |
| Reading layout, landscape | 16–18 ms |
| Menu / Settings layout | 3–8 ms |
| Render flush, Fast | **405 ms** median (379 ms of it panel BUSY) |
| Render flush, FastClean | **482 ms** median (456 ms of it panel BUSY) |
| Prestage (after the settle; reader never waits) | 24 ms |
| Progress write | 42 ms |
| **Page turn, press-to-settled** | **~424 ms** |

**FastClean re-measured 2026-09-04 after #89 (A13), X3, main `b3f11cd`.**
`flush_ms` 482 median / 482 p95 over 50 samples, `busy_ms` 456 with min equal
to max, prestage 24 with min equal to max over 57 renders. The pre-#89 figure
was 681 median / 726 p95, so **quote 482 and not 681 or the roadmap's older
686.** The Fast row is unchanged and was re-confirmed in the same session at
405 and 379, which pins the FastClean move to the settle rather than to
anything else that moved.

Two things that session settled, both worth reading before sizing an A-item:

- **The software tail is 26 ms per flush, in both modes.** `flush` minus
  `busy` is now 26 ms for Fast and 26 ms for FastClean, where FastClean was
  226. There is no longer a mode-dependent software overhead on the render
  path, so a future A-item cannot be sized against one.
- **`bench.py`'s `render flush` line pools every refresh mode**, which is
  exactly where A13 lived: a pooled median moved 681 to 482 only because the
  capture was mostly FastCleans. Split by mode before believing any flush
  figure. The report gives `refresh modes:` counts to tell you the mix.

**Book pipeline, X3, 11.7 MB baseline book (2026-07-25 / 07-28).**
**These predate folder browsing.** They were taken against a flat `/BOOKS`
card, and #88 now catalogs every book at whatever depth it sits, so the scan
and catalog-write costs behind the build figures are not the costs these
numbers describe. The per-book build figures themselves should survive, since
the build path did not change, but do not quote the scan side of them without
re-taking it. What *is* recorded for the folder path is in
`docs/ARCHITECTURE.md` (~`:639`): on an X3 at 1,129 books, entering a folder
costs 41 ms plus 0.356 ms per row and paging inside one is flat at about
35 ms, so no derived directory index is warranted. A catalog rebuild
figure at that book count was taken in an August bench session but is not
recorded anywhere in the tree, so it needs re-taking before it is quoted.

| Metric | Value |
|---|---|
| Full cold build | 64.0 s portrait (1240 pp / 100 sections), 62.2 s landscape |
| Settings-change replay via CONT.BIN | 24.7 s (736 pp) — 27.1 s (1240 pp), ~280–300 ms/section |
| Orientation flip | same replay path. Turning the flip *back* into a hit is Reading Position and Layout Durability M4, no longer B7 |
| Progressive first open → prologue | 32.6 s, interactive throughout |
| Warm reopen (RAM hit) | 13–15 ms |

**Two figures retired 2026-07-30, both of which items were sized against:**

- **Fast BUSY is 379 ms, not 421 ms.** The 421 figure is a June 10 2026
  capture on an unnamed board and is superseded.
- **Full BUSY on the X3 is 928 ms, not "~3.5 s".** Measured, n=16, in the
  bench log on disk. C1's "~2 s off every wake" was sized against the 3.5 s
  figure; the real saving is Full 928 → FastClean 455, about 474 ms. Anything
  still ranked against 3.5 s needs re-sizing.

**Every latency figure in this document is a *tethered* baseline.** All of
them were captured with a USB monitor attached, and with a monitor attached
`esp-println` writes into a USB FIFO. Untethered there is no SOF and the same
prints take a blocking UART path — see Tier 0d. The numbers are not wrong;
they describe a device plugged into a laptop, which is not the shipped one.

**Never measured:** deep-sleep current or any other power figure at any
operating point (C2); the upload throughput ceiling's cause. Boot- and
wake-to-first-paint came off this list on 2026-09-10 (table above).
**Still true of *this* firmware after the 2026-08-13 sweep** — but upstream now
has X3 figures from a PPK2 at the battery terminals (deep sleep **12.8 µA**,
static-page idle **9.68 mA** before their light-sleep work, **2.78 mA** after;
crosspoint `70faa29d`). Different firmware and a different GPIO configuration,
so these are reference points, not our numbers. Their value is that they bound
what the hardware can do, which is what C2 has been unable to say. See C10.
**Permanently unavailable: the owner has no X4.** Every "verify on both
boards" step is X4 compile/clippy/goldens only; on-device X4 validation
happens if a contributor with hardware appears. C2's wake-reliability step
is X3-only for this reason.

## Method rules, learned expensively

1. **Measure the split before ranking a "cache the layout" item.** A10 sat
   at #1 for three revisions on a premise nobody checked: layout is 2.8 µs
   of a 197 µs portrait page render. The other 98.5% is rasterization.
2. **Check that a measurement brackets what the user experiences.** One
   misplaced `println!` put the prestage inside the reported page turn and
   promoted a pessimization to #2 in the queue. Likewise `storage_first_page`
   at 455 ms described nothing a reader feels — the metric is
   time-to-first-*content*, compared against a plain build, not against zero.
3. **`page-turn` is operator-driven and not cadence-robust.** Its `page turn`
   statistic is input→next-render, so a press landing mid-render is credited
   with only the remainder; burst captures show 2 ms turns. Quote it only
   from deliberate cadence. `layout_ms`, `flush_ms`, `busy_ms` are per-render
   and safe from either.
4. **A partial cache must say on disk whether something is coming back for
   the rest.** The reducer clamps the reader to the advertised page count, so
   a truncated index is a one-way trap the reader cannot provoke a rebuild
   out of — and it looks exactly like a short book.
5. **The compiler names what compiles, not what should be public.** Driving a
   crate extraction's visibility by "promote whatever rustc names" widened
   eighteen fields that only mean anything as a set.
6. **Host gates cannot see a stack overflow.** `tools/check.sh stack-frames`
   now disassembles the release build and fails over 24 KB per frame. It
   bounds one frame, not call depth.
7. **Check whether the answer is already being printed.** Added 2026-07-30,
   and it is the round's most expensive lesson. The replay split WS-B guessed
   at for three rounds, boot-to-first-paint, and B4's own headline numbers are
   all emitted by the firmware on every run and consumed by nothing. Before
   designing an experiment, `grep` the firmware for what it already prints and
   `grep` `bench.py` for what it actually reads — they are not the same set.
8. **A verification gate that can be silently absent is worse than none.**
   `report --strict` checks nothing on Python < 3.11 and exits 0 without a
   diagnostic, which is the interpreter `#!/usr/bin/env python3` selects on
   macOS. Any gate this project relies on should fail loudly when its
   preconditions are missing.
9. **Do not carry a conclusion across boards.** "The BUSY floor is OTP and
   cannot be touched" is true of the X4's SSD1677 and false of the X3's
   UC8253, which uploads its own LUTs and sets its own frame timing. That one
   conflation hid 136 ms of every X3 refresh for four rounds, and it is why
   WS-A is reopened. When a number and a mechanism come from different boards,
   at least one of them is wrong.
10. **Suppress the render, never the event.** An event that carries state the
   reducer needs must always be applied; only the repaint may be skipped.
   Suppressing the event instead is how the top-ranked branch reintroduced
   rule 4's one-way trap. A guard placed *below* the reducer — at the flush
   seam — is structurally immune to this and is the safer default.
11. **A pin protects bytes, not version numbers.** Before regenerating any
   frozen artifact, run the generator unmodified and require a clean diff;
   only then does the pinned toolchain mean anything. #66 pinned a
   Pillow/FreeType pair that could not rebuild the shipped font tables — the
   fingerprints came from #61's toolchain and the pin was never validated by
   regeneration — and the mismatch sat under a "reproduction verified" commit
   message for a week. The prove-first habit is what caught it (issue 08,
   H5).
12. **A pooled statistic hides a mode-specific win, and every flush figure in
   this project is pooled.** `bench.py`'s `render flush` line covers Fast,
   FastClean and Full together, so its median tracks the *mix* of a capture
   as much as any change. On #89's captures the pooled median read 681 to
   482, which looks like the real result and is not: it moved that far only
   because the walk was mostly view changes. Split by mode before believing a
   flush number, and quote the `refresh modes:` counts alongside it so the
   mix is visible. The same trap is waiting for A12, which moves `busy_ms`
   across every mode at once, and for A14, which removes whole renders and so
   changes the mix itself.
13. **Take the before-capture on today's `main`, not from this document.**
   #89 was specified against a 686 ms figure recorded before #79 through #88.
   Re-taken on current `main` in the same session, on the same card and book,
   it was 681. Close enough that the item was safe, which is luck rather than
   method: a 5 ms drift over ten merges could as easily have been 50, and it
   would have been credited to the change. An A/B in one session also makes
   the controls worth something, since `busy_ms` and prestage holding to the
   millisecond only means anything when both sides ran on the same hardware
   minutes apart.

## Workstreams

One issue file each, owning a distinct set of files.

- **WS-A — Display render path** (`issues/01-display-render-path.md`).
  `display/`, `fw/src/display_flush/`, flush/prestage region of
  `fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`. **Reopened
  2026-07-30** — the panel's own timing is software-set on the X3.
- **WS-B — Book pipeline** (`issues/02-book-pipeline.md`). `reader-cache/`,
  `fw/src/book_build.rs`, `fw/src/custom_font.rs`, `fw/src/library_sd.rs`,
  `ui/src/reading.rs`, `proto/`. **Shared since 2026-09-03.** The blanket
  claim on `proto/` and `reader-cache/` is no longer exclusive: #87 and #88
  added `proto/src/source.rs`, `proto/src/library_path.rs` and
  `reader-cache/src/browse.rs`, and `feature/library-identity-m1` adds
  `proto/src/identity.rs` and changes `reader-cache/src/store.rs`,
  `proto/src/catalog.rs` and `fw/src/library_sd.rs`. Those files answer to the
  library PRDs, not to this document. Treat the directory claim as "the book
  pipeline inside these crates" and check `git log` on a file before assuming
  WS-B owns it.
- **WS-C — Power & boot** (`issues/03-power-boot.md`). `fw/src/tasks/power.rs`,
  `fw/src/tasks/input.rs`, `hal-ext/src/rtc.rs`, `hal-ext/src/bq27220.rs`,
  planner seed in `app-core/src/lib.rs`, boot region of the display task.
- **WS-D — Storage & Wi-Fi** (`issues/04-storage-wifi-throughput.md`).
  `fw/src/sd_session.rs`, `fw/src/tasks/wifi.rs`, `fw/src/upload.rs`,
  `fw/src/sync_mem.rs`, `upload-store/`, `proto/src/upload.rs`, and the
  `embedded-sdmmc` fork — which since #75 is **ours to change**, not a pin to
  work around. Its rev and rationale live in `[workspace.dependencies]` in the
  root `Cargo.toml`; its regression tests live in the fork, so re-run them
  there when bumping. **Shared since 2026-09-03**, and more heavily than WS-B:
  `upload-store/` is where the library work lives. #82 and #87 changed the
  install and staging paths, and `feature/library-identity-m1` adds
  `upload-store/src/ledger.rs` and `upload-store/src/replace.rs` and touches
  `install.rs` and `fw/src/sd_session.rs`. A WS-D item that assumes it is the
  only writer in this region will conflict.
- **WS-E — Flash & RAM budget** (`issues/05-flash-ram-budget.md`).
  `.cargo/config.toml`, `display/src/font.rs` (struct only), generated font
  tables.
- **WS-F — Web emulator, CI & the bench harness**
  (`issues/06-web-emulator-ci.md`). `web/`, `tools/web-emulator/`,
  `tools/build-web.sh`, **`tools/bench/`**, `.github/workflows/`. Disjoint
  from firmware work. **Scope widened 2026-07-30** to give the measurement
  harness an owner — it produces every device number in this roadmap and had
  none.
- **WS-H — Typography & text rendering quality**
  (`issues/08-typography.md`). `tools/fontgen_common.py` and the font
  generators, the generated `display/src/*_generated.rs` tables, and the text
  layout in `ui/src/reading.rs`. **New 2026-07-30.** Text quality had no owner
  because it spans three regions. Note the economics invert here: glyph
  rasterization runs on the *host*, so quality costs the device nothing and
  the binding constraint is cache invalidation rather than speed.
- **WS-G — App state & render invalidation**
  (`issues/07-app-render-invalidation.md`). `app-core/`, `ui/`, and
  `fw/src/tasks/app.rs` as the seam. **New 2026-07-30.** The top-ranked item
  was previously filed under no workstream because this region had no owner.
  One avoided panel refresh is worth ~405 ms against a 13 ms total layout
  budget, so the only thing worth ranking here is a render that should not
  happen — never a CPU micro-optimization.

### Coordination hazards

1. **`fw/src/tasks/display.rs`** is touched by WS-A (flush/prestage), WS-C
   (boot init, OTA probe skip), and WS-B (the background-build branch of the
   select). Disjoint regions; rebase carefully.
2. **Stack and RAM are shared currency.** Every `.bss` change re-checks the
   link-time stack ASSERT and `tools/check.sh stack-frames` on **both** X4
   and X3 — X3 is the tight one, 42 KB region against a 27 KB floor.
3. **Golden frames** gate anything touching a render path:
   `fixtures/golden` + `tools/emulator/tests/reading_golden.rs`, per
   `docs/agents/visual-verification.md`.
4. **Hardware sign-off** is required for C2, C6, A4, A5. An agent can prepare
   the code; the verdict is a measurement. C2's first-line experiment now
   needs only a device and patience (the fuel gauge integrates over a long
   sleep), not an instrument; C6 and A4/A5 still need a meter or a careful
   visual soak, and A5 needs an X4 nobody has.
5. **`fw/src/sd_session.rs` is no longer WS-D's alone.** The old rule was
   that WS-B benefits from its changes but must not modify it. That still
   holds for WS-B, but the library work modifies it from outside this
   document (`feature/library-identity-m1`), so WS-D is a second writer rather
   than the owner. Check the file's `git log` before planning a change to it,
   and expect the same of `upload-store/`.

6. **The four library PRDs are the other half of the tree.** They live beside
   this one in `.scratch/` and are where reader-cache identity, pagination
   keying, folder browsing and upload staging are being designed. Any item in
   this roadmap that touches cache keys, catalog records, upload staging or
   book identity should be read against them first, because the roadmap's
   region ranking predates all four.

## Verification

- Firmware timing: `tools/bench/bench.py` suites (`page-turn`,
  `storage-cache`, `sleep-sync`, `channel-stress --host`, `reader-soak`) per
  `docs/agents/bench.md`. Budgets in `tools/bench/benches.toml`.
  **Interpreter: resolved.** Tier 0a landed in #58, so `--strict` refuses to
  run without a TOML parser rather than passing silently, and the repository
  now pins Python to the 3.14 series in `.python-version` with `tools/check.sh`
  preferring `python3.14`. The old "run it under 3.11 or better" caveat is
  retired; use the pinned interpreter.
- **Folding the protocol rules into `docs/agents/bench.md`: done.** That file
  now carries deliberate cadence, the `coalesced`/`unmatched` accounting, the
  10% suppression rule, and the 354 ms history that produced them. `AGENTS.md`
  names `tools/check.sh stack-frames` among its Python-requiring targets.
  Nothing is left owed here.
- Visual: emulator goldens on both boards per
  `docs/agents/visual-verification.md`.
- Size/stack: `llvm-size -A`, `llvm-nm` on `_stack_start`/`_stack_end`, the
  link-time ASSERT, and `tools/check.sh stack-frames`.
- Upload: timed `curl --data-binary @book.epub` A/B plus `sd_stats` counters
  (`write_calls` vs `write_blocks` proves batching).
- Power: **first line is the X3's BQ27220 as an integrating coulomb counter**
  over a long deep sleep — no meter, no disassembly, and it stays powered
  while the SoC does not (issue 03, C2). Do **not** meter the 4-pin pogo
  cable: it is USB, so it sits on the wrong side of the charger and plugging
  it in changes the state under test. A series meter on the *battery lead* is
  the follow-up if the gauge indicts something, and it wants a PPK2-class
  instrument rather than a DMM — burden voltage on a µA range can brown the
  device out on a wake spike, and the span needed is ~15 µA to ~100 mA.
  `bench.py` has no power channel either way.

## Do not re-propose

Each of these was tried, measured, and rejected. The reason is the part that
matters — without it the idea comes back.

**Assessed and declined from the 2026-08-13 upstream sweep** (crosspoint
`255bab31..`, freeink `8b8337b..`). Recorded because each one *looks* like a
port until you check one fact:

- **freeink `fdf246d`, `__builtin_popcount` → inlined SWAR.** Xtensa-specific:
  the builtin lowers to a windowed `callx8` into ROM's `__popcountsi2`. Today's
  firmware targets `riscv32imc` (`rust-toolchain.toml`) and has no `count_ones`
  on any path, so there is nothing to fix **now**. **Do not read this as
  "Xtensa findings are irrelevant" — that stops being true the moment `fw-s3`
  exists.** The Sticky is an ESP32-S3, which is Xtensa, and the Sticky PRD
  already requires Xtensa binary-analysis tooling; this is a concrete first job
  for it. Re-read this entry, not just its verdict, when `fw-s3` lands.
  *(The architecture-neutral half is the shape of the bug — tens of thousands
  of calls per page turn existing only to feed a serial-log statistic. That is
  C7's argument, restated by someone else's profiler.)*
- **crosspoint `6af9a049`, cache cumulative spine sizes in RAM.** Their progress
  bar computes percent-of-book from cumulative spine *byte* sizes, so every
  render paid two seeks and a heap-allocating read. **We do not compute progress
  that way** — the reducer carries a page count from the paginated cache and
  the reading view renders from RAM. There is no per-render SD read here to
  remove.
- **freeink `b17beee`, "one forced full sync after begin(), not two".** Their
  `begin()` set a `_initialFullSyncsRemaining = 2` countdown, so a splash screen
  consumed the first forced clean and the first real screen still paid the
  second (press-to-home 7634 → 4569 ms on X3). **We have no countdown**:
  `fw/src/tasks/display.rs:426` is the sole panel-init site, gated on
  `!screen_on() && last_request().is_none()`, and the mode comes from the
  refresh planner. Worth knowing the failure mode exists; there is nothing to
  fix. *(C8 remains the sleep-side version of this question and is unaffected.)*
- **The grayscale run in freeink** (`72529a0`, `f50b1ab`, `477ac31`, `a7bb60b`).
  Assessed and rejected on 2026-07-25 for reasons that have not changed: no
  2-bpp consumer, the RAM budget, and the UC8279 cannot use our LUTs. `41f2a7f`
  is UC8279-for-**X4 Pro** and does not advance the open UC8279 **X3** port —
  watch that PRD, not this commit.
- **Not declined, re-scoped: the Paper Mono / Murphy / X4 Pro wave is ESP32-S3
  bring-up** (`3c20447`, `f9aa77e`, `d123567`, `ba4f1d6`, `f9c60ae`, `24fbab7`).
  `platformio.sample.ini` puts X4 Pro, Paper Mono, Murphy and PaperS3 all on
  `board_build.mcu = esp32s3`; only the Xteink X3/X4 line is C3. **That is the
  Sticky's silicon**, so this wave is reference material for
  `reterminal-sticky-support`, not "other hardware". It is out of scope *for
  this roadmap* because the roadmap owns the shipping C3 firmware — which is a
  different statement from irrelevant, and the distinction is the whole reason
  this bullet exists. Findings routed to that PRD rather than recorded here.

- **A10 as specified (pre-computed line-wrap caching).** There is no wrapping
  in the render path to cache. The EPUB sink wraps at cache-build time,
  `push_line_block` stores one physical line per `BlockRecord` with
  `line_count: 1`, and the page record is already an O(1) index. Reading-view
  render time moves on rasterizer work only.
- **A7 / skipping or deferring the RED prestage** (#48, reverted #52). The
  prestage is what keeps the previous-frame write *off* the turn; skipping it
  defers the same write into the next Fast flush ahead of `DisplayRefresh`,
  where the reader does wait — and each skip leaves the next turn unstaged,
  so a held button pays it every time. It also never fired: one `yield_now()`
  cannot cover the Settled → app → next-`RenderRequest` round trip. Genuine
  overlap with panel BUSY is a different design and belongs to A9.
- **A8 (4-way strided unrolling of the portrait `blit_row` loop)** — #49
  closed. #50 removed that loop from the glyph path and took 2.5× instead.
  Only `fill_span` is left for it, which is menu furniture at 3–8 ms.
- **D2 (radio RX buffers + AMPDU-RX + paced SD writes).** Timed X3 A/B on a
  3.2 MB EPUB: main 19.3 s, D1+D2 21.1 s, D1+buffers-only ~20.2 s. Pacing
  cost ~1 s/upload; the buffers bought nothing and spent ~6.6 KB of loaned
  heap. Throughput sits near 160 KB/s under every configuration — the
  bottleneck is neither radio RX nor SD write stalls. Start with the
  upload-ceiling investigation, not by re-trying these.
- **Build-time portal PSK.** A committed PSK is public in a public repo, and
  a CI-minted one is extractable from released firmware. Shipped as a
  per-session runtime PSK instead (#19).
- **Partial-window panel refresh** — deliberately shelved twice.
- **SPI above 40 MHz** (rated ceiling); **`MIRROR_Y=true`** (tested, wrong).
- **80 MHz CPU clock** — 160 MHz race-to-idle is an explicit decision.
- **ilp32e ABI, frame-pointer removal** — rejected in the stack brainstorm.
- **Re-donating dram2 to the radio heap** — removed on purpose to restore
  stack. Do not win D2's heap back this way.
- **kosync progress sync** — implemented, shipped unused, removed.
- **Software optimization of the Full-refresh waveform** — noise.
- **Dependency-dedup and panic/fmt shrinking** — <25 KB combined, poor ROI.
- **`.eh_frame`** — non-alloc, not in the flash image. Nothing to reclaim.
- **Optimizing the golden runner** — measured fast (24 scenarios in 0.52 s).
- **esp-radio power-save during an upload session** — already off.
  `WifiController::new` calls `set_power_saving(PowerSaveMode::default())` and
  that default is `None`, with an upstream comment saying the blob default is
  bad for bandwidth. Refuted from source 2026-07-30; it was suspect 2 in the
  upload-ceiling list and is now deleted from it.
- **Decimating the 15 ms input tick** — now has a number, not just a
  principle. One tick is ~150–250 µs of CPU (~1.0–1.7% duty) and the rest is
  already WFI, so decimating to 60 ms saves ~**0.06 mA of ~15–20 mA** while
  costing 4× the press latency. The standing rejection is correct.
- **A9 as a bandwidth item on the X3** — a plane write is ~26 ms of which
  ~20.9 ms is wire time at the datasheet-ceiling 20 MHz, so there is at most
  ~3 ms to win, and double-buffering re-spends the 8 KB `TX_BAND` that A3
  freed on the tight board. A9 survives only as the home for prestage overlap.
- **Moving a large stack temporary into a static to "save stack".**
  `_stack_end` is exactly `ADDR(.bss) + SIZEOF(.bss)` on both boards —
  verified by address arithmetic and by a natural experiment — so every
  `.bss` byte trades **1:1** against the main stack. Hoisting is neutral at
  best and a net loss when the temporary was not on the peak call chain. Only
  deleting `.bss`, or shrinking a frame on the peak chain, buys margin.
- **Holding the panel RST line through deep sleep, on the hardware we ship.**
  Closed unmerged as PR #70 — the one entry here that was decided rather than
  measured, so the condition matters more than usual. Upstream's ~36 h-to-dead
  field report names the **UC8179**, whose DC-DC booster is host-programmed via
  BTST and restarts off a drifting RST; it calls the SSD1677 tolerant, having no
  external booster and a deep sleep that actively discharges. Two separate
  reasons not to re-propose it as written: today's units are SSD1677 and UC8253,
  and #70's `rst.set_high()` would not have worked anyway, because a C3 pad goes
  high-Z in deep sleep whatever level it was last driven to. It comes back only
  with `uc8179-x4-driver` or `uc8279-x3-driver`, and then as a pad hold armed
  before sleep and released before the wake reset pulse — both halves, or the
  reset pulse bounces off the latch. Tracked on those two PRDs. The separate SD
  rail suspect is `x3-sd-rail-sleep-power`; do not conflate them.

## Doc drift to fold into whichever PR touches the area

- `hal_ext::rtc::enter_light_sleep_timer` has zero call sites. Either wire it
  up behind C4's successor tier or delete it.
- `docs/FLASHING.md`'s open "Locked-unit confirmation" box describes only the
  stock **bootloader's** descriptor gate. It does not mention the separate risk
  that a locked X3's OEM SD updater rejects `update.bin` through the buggy
  app-side validator in the stock firmware — garbage eFuse block revisions read
  via a misaligned `bootloader_mmap`, which CrossPoint documents and works around
  in its own shipped app. Nothing we ship can change that outcome, since the bug
  is in the firmware being replaced, which is exactly why it belongs in the doc
  next to the existing box rather than in a PRD. (The PRD that asked whether we
  needed CrossPoint's linker wrap was removed: we link no ESP-IDF, so there is no
  such symbol in our image.)

The other two entries that lived here are resolved: `ARCHITECTURE.md` no
longer claims a 1.5 s wake unconditionally (C1 landed and the doc now states
the wake-cause condition), and the dram2 radio-heap paragraph has been
corrected.
