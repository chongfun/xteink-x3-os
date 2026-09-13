# WS-A: Display render path

**Status (2026-07-30): REOPENED.** The "finished" verdict was correct about
the frame it measured and wrong about the frame it did not. It rested on two
premises that a survey of the panel drivers has now falsified:

1. *"The BUSY floor is a hardware constant."* **On the X3 it is not.** Fitting
   the repo's own bench log gives `busy_ms = 136.0 + 12.79 × frames`, and
   both terms are bytes in this repository — the LUT frame counts in
   `display/src/epd/uc8253.rs` and the CDI/PLL/booster registers in
   `INIT_SEQUENCE`. **136 ms of every single refresh is not waveform drive.**
   See A12.
2. *"Everything outside BUSY is single-digit milliseconds."* True of Fast
   only. FastClean carries **230 ms** of software overhead, 200 ms of it a
   timer with nothing behind it, on every view change and every wake. See A13.
   **Resolved by #89, 2026-09-04, and the premise was right.** The 200 ms was
   the timer: FastClean's software tail measured 226 ms before and 26 ms
   after, which is Fast's own tail to the millisecond. Neither mode now
   carries a mode-dependent overhead outside BUSY, so premise 2 is true of
   both rather than of Fast alone.

It also missed a category rather than a number: **WS-A only ever asked how
fast a refresh is, never whether it should happen.** Nothing in the tree
compares a frame against the one already on the glass. See A14.

The measured per-turn figures are unchanged and still correct: **13 ms layout
+ ~405 ms flush (379 ms of it panel BUSY) = ~424 ms press-to-settled**, with
a 24 ms prestage the reader never waits on. Layout is still 3% of the turn,
and the layout/rasterization conclusions still stand. What changed is that
the 379 ms and the flush around it are no longer off limits.

**Board conflation to retire, because it is what produced the wrong verdict:**
the "the BUSY floor is OTP, so it cannot be touched" reasoning is an **X4
(SSD1677)** statement. It was carried onto the **X3 (UC8253)**, which has no
OTP waveform — it uploads its LUTs and sets its own frame timing on every
flush. A5 below still describes an X4 mechanism while quoting the X3's
379 ms. Do not size A5 against a number from the other board.

Owns: `display/`, `fw/src/display_flush/`, flush/prestage region of
`fw/src/tasks/display.rs`, `hal-ext/src/spi_dma.rs`.
Do not touch: `fw/src/sd_session.rs` (WS-D), boot-init region of the display
task (WS-C).

## The BUSY model (X3), measured

Fitted against `target/bench/latest.jsonl` (516 refresh events) and confirmed
by re-reading the log directly. The UC8253 LUT format is 7 groups × 6 bytes
`[level, f0, f1, f2, f3, repeat]`; within a bank the five LUTs carry identical
frame counts and differ only in the level byte. Frame totals: **Fast 19,
FastClean (HALF) 25, Full 62.**

| mode | n | measured median | `136.0 + 12.79 × frames` |
|---|---|---|---|
| Fast | 458 | **379 ms** (378–380) | 19 → 379.0 |
| FastClean | 42 | **455 ms** (455–456) | 25 → 455.8 |
| Full | 16 | **928 ms** (max 930) | 62 → 929.0 |

Three modes, two parameters, sub-1 ms residuals. Full's `min=379` is its
second pass reloading the Fast bank, which the model also predicts.

**Re-confirmed independently 2026-09-04**, on a different card and book and a
different capture, while measuring #89. Fast 379 (n=9 and n=7, min equal to
max), FastClean 456 (n=26 and n=50, min equal to max), Full 930 max with a
379 min on the same plan. The model was fitted on one log in July and
predicts a fresh one to the millisecond, so **A12's 136 ms fixed term is a
property of the controller rather than of that capture.** That strengthens
A12: the term it proposes to attack is real and stable.

**Two corrections fall straight out of this table.** The often-quoted
**421 ms Fast BUSY** is a June 10 2026 capture (`docs/IMPLEMENTATION_PLAN.md`,
board unnamed) and is superseded by 379 ms. The **"~3.5 s Full refresh"** that
C1, boot and wake estimates were all sized against **is not an X3 number** —
Full BUSY on the X3 is 928 ms. Anything ranked against 3.5 s needs re-sizing.

## Open, if the render path is ever revisited

Order: A14 (largest, and it subsumes several tracked items) → A12 (test
first, one byte) → A11 (size it first) → A4 (verify first) → A5
(experiment). **A13 is done, #89.** A14 keeps the top slot and #89's captures
handed it new evidence; see A14 below.

### A12 (S to test, M to ship): 136 ms of every X3 refresh is controller interval, not drive

The fixed term in the model above is **36% of the 379 ms Fast page-turn
BUSY** and it is not pixel drive — it is ~10.6 frame-times of controller-side
interval. Dominant suspect is the CDI data-interval nibble: the low nibble of
*both* mode bytes is 9 and has never been varied
(`display/src/epd/uc8253.rs:52-53`, `CDI_DIFFERENTIAL = 0x29` /
`CDI_ABSOLUTE = 0xA9`, with `CDI_INTERVAL = 0x07` as the second byte).
Booster soft-start (`CMD_BOOSTER_SOFT_START`) and `CMD_PLL_CONTROL` are the
secondary candidates. Every one of these bytes is verbatim from the CrossPoint
reference and none has been touched in this tree.

Supporting arithmetic, independent of the fit: on the standard UC-series CDI
mapping (`0x0` = 17 frames … `0xF` = 2), nibble 9 is 8 frames, and
8 × 12.79 ms = **102 ms** — most of the 136 ms constant, from a byte nobody
has changed. *(The nibble→frames mapping is inferred; no UC8253 datasheet is
in the repo. It does not need to be right for the experiment to be worth
running.)*

**Independent confirmation and one warning, from the 2026-08-13 upstream sweep
(freeink `cd2bcc5`).** Upstream's UC8253 X3 driver reaches CDI through
`loadBankCdi(bus, 0x29, 0x07, bank)` — the same two bytes this item names, from
a separate lineage, which is as close to corroboration as these constants get
without a datasheet. Their header documents the high byte as mode selection:
CDI `0x29` differential, `0xA9` absolute. That matches our reading, so the
value under suspicion really is the interval field and not something else
wearing its bits.

**The warning is worth more than the confirmation.** The same commit fixes a
CDI bug on their UC8279 driver of exactly the shape A12 might invent: the
driver had been sending a *first-vs-later* CDI split (`0xD7` then `0x97`), and
byte-level RE of the stock firmware showed stock sends **one constant value on
every refresh**, with `0xD7` belonging to a separate settle pass their driver
does not run. The invented split caused border ghosting on later pages. So: if
the experiment below leads to varying CDI *per refresh* rather than changing
one constant, that is the failure mode to expect, and border ghosting on the
second and later pages is its signature — not something a single-page capture
would show.

Their method is also the cheapest way to settle what stock actually sends, and
we have not tried it: read the constants out of the factory firmware image
rather than inferring them from behaviour.

- Impact if the interval accounts for even 6 of the ~10.6 frames:
  **~77–100 ms off every refresh in every mode**, page turns included —
  **18–24% of the whole 424 ms turn**, an order of magnitude more than
  everything else left in WS-A combined. *(Estimate for the size; the 136 ms
  term itself is arithmetic from measurements.)*
- **The one measurement:** flash with the `CDI_DIFFERENTIAL` low nibble
  changed from `9` to `0xF`, nothing else, and read `bench: refresh
  mode=Fast busy_ms=` from a `page-turn` capture. **Kills it:** `busy_ms`
  stays 379, meaning the constant is soft-start or PON-internal and this
  shrinks to a booster-timing experiment.
- Risk: medium-high. CDI packs DDX (data polarity) and VBD (border) in the
  same byte — **only the low nibble may move.** Too short an interval shows
  classically as border flash or edge ghosting, so this needs a ghosting and
  border soak before it ships, not just a timing capture.

### A13: LANDED as #89, 2026-09-04

**Predicted −200 ms, measured −199.** Kept in full below because the item was
right for the reason it gave, and the measurement it demanded is the template
for the rest of WS-A. What shipped, and what the device said:

- **FastClean `flush_ms` 681 → 482** median on an X3 (p95 726 → 482, n=26
  before and 50 after). The pre-#89 median was 681 rather than the 686 quoted
  below, re-taken on today's `main` because the 686 predates #79 through #88.
- **Every control held.** `busy_ms` 456 both sides with min equal to max
  across 76 samples, so no waveform, register or wire byte moved. Prestage 24
  both sides with min equal to max, so the interval was deferred rather than
  dropped. Fast untouched at 405 flush and 379 busy. Page turn 426 → 427.
- **The kill condition, answered.** The tail was to be shown as the timer and
  nothing else. `flush` minus `busy`: Fast 26 ms before and after; FastClean
  226 ms before, 26 ms after. A clean flush now carries exactly a fast turn's
  overhead.
- **The power-on clean path saved the same 200 ms**, 808 → 608, so
  `CLEAN_POWER_ON_STEPS` is covered as well as `CLEAN_POWERED_STEPS`. That is
  the wake, and it is a call site that holds the interval in place rather
  than deferring it, since no prestage follows a wake's first flush.
- **X3 only, by construction.** Of `SETTLE_MS`'s four uses in the step
  tables, the two in `FULL_STEPS` and `FULL_POWERED_STEPS` are mid-plan and
  stay awaited inside the flush. The X4's FastClean is a separate path
  (`display_flush/ssd1677.rs`) that ends on a completed BUSY wait and owes
  nothing.
- **Cost:** +128 B `.bss` on both boards for the extra await points, 1:1
  against the main stack. X3 region 38,320 → 38,192 B.

Two device observations from those captures, neither caused by #89:

- **A cold boot's Full is followed 714 ms later by a FastClean of the
  identical view and page, and a wake's first FastClean by another 714 ms
  later.** Identical on both builds. This is A14's case, now with numbers.
- **The clean transition flashes bright white and relaxes back.** The HALF
  bank is an absolute drive (WW==BW at 0xAA/0xA0, WB==BB at 0x55/0x50, 25
  frames regardless of what is on the glass), so white-target pixels are
  pushed past the settled paper tone. Pre-existing and confirmed on `main`.
  #89 makes it *more noticeable* only because `Settled` no longer sits behind
  the 200 ms the ink relaxes in. The electrical interval is preserved in
  full. Worth knowing before anyone reads a future clean-path change as
  having introduced it.

The original item follows, unedited.

`SETTLE_MS = 200` (`display/src/epd/uc8253.rs:192`) is the **last** step of
both `CLEAN_POWERED_STEPS` and `CLEAN_POWER_ON_STEPS`, awaited inside `flush`,
and `Settled` is only sent afterwards. Measured: FastClean `flush_ms` is
**686 ms** median against `busy_ms` 455.5 — a **204 ms tail** whose only plan
content is `DelayMs(200)`.

The delay's physical job is to keep the panel quiet before the *next* RAM
write, and the next RAM write is `prestage_previous`, which already runs
**after** `Settled`. So sending `Settled` first and then sleeping preserves
the interval exactly while removing 200 ms from the wait. Pure ordering — no
waveform, no register, no byte on the wire changes.

- Impact: **−200 ms (−29%) on every FastClean, measured.** FastClean is ~9%
  of renders but fires on *every* view change, every type-settings change,
  every wake, every library-menu step, and every 9th turn under
  `FullEveryTen`. A Home→Library→Reading→Home→Library→Reading walk is six
  consecutive 691 ms flushes that should be 491 ms.
- Effort S: add a `settle_after_ms` to `FlushPlan` (or return the trailing
  `DelayMs` rather than awaiting it) and have the display task do
  `Settled` → `Timer` → prestage. Single-task ownership keeps the interval
  guaranteed; no lock is held across the await.
- **The one measurement:** FastClean `flush_ms` must fall 686 → ~486 with
  `busy_ms` unchanged at 455 and prestage unchanged. **Kills it:** `flush_ms`
  does not drop ~200, meaning the tail is something other than the timer.
- Risk: low. The only failure mode is an implementation that *drops* the
  delay instead of deferring it.

### A14 (S–M): nothing compares a frame against what is already on the glass

The display task already keeps `prev_fb`, a byte-exact copy of the displayed
frame, maintained after every successful flush. A single
`fb.bytes() == prev_fb.bytes()` at the flush seam turns every identical-pixel
render — from *any* cause — into a no-op, for a compare estimated at
**250–800 µs** against a **435 ms** saving (379 BUSY + plane write +
prestage). Break-even hit rate is ~0.2%.

This was found independently by the display survey and the app-core survey,
which converged on the same fix and the same predicate. **It subsumes several
separately-tracked items:** the double repaint on every page turn, the
62 consecutive identical refreshes at a book's last page, the loading plate's
duplicate flush, and six no-op input sites. See issue 07 for the full
inventory and for the app-core-side fix, which is more precise and testable
and should land alongside rather than instead of this.

**Safe predicate:** skip only when
`screen_on() && last_request().is_some() && mode == RefreshMode::Fast &&
fb.bytes() == prev_fb.bytes()`. On a skip the task must still send `Settled`
and the power event (or the app's render lock never clears), must still
update `last_request` but **not** bump `fast_refreshes` (or `FullEveryTen`'s
clean drifts), and must leave `prev_prestaged` alone. Restricting to `Fast`
keeps deliberate ghost-clearing passes unconditional; a failed flush and a
sleep both clear `last_request`, which the predicate requires, so the panel
can never disagree with `prev_fb` on the skip path.

- **The one measurement — counting, not timing.** Add `identical=<bool>` to
  the existing `bench: render` line with no behaviour change, then run a
  normal session plus a held `Next` at a book's last page. **Kills it:**
  under ~2% of renders are byte-identical outside the already-known cases.
  Also time the compare; over ~5 ms, revisit.
- **Partial evidence arrived free with #89, 2026-09-04, and it favours A14.**
  Two `reader-soak` captures on an X3, one per build, both show the same
  duplicate pair: a cold boot's Full followed **714 ms** later by a FastClean
  of the identical view *and* page, and a wake's first FastClean followed by
  another 714 ms later. Two of them per boot, on 34 and 57 renders, so ~4-6%
  of renders in an ordinary session are candidates before the end-of-book
  case is counted at all. That clears the ~2% floor on its own. The pairs are
  a *suspicion* rather than a hit, because nothing compared the bytes: the
  page and view match, and 714 ms is far too regular to be an operator
  press. So `identical=<bool>` is still the measurement, and it now has a
  named, reproducible case to confirm against rather than a hunt. **Note the
  predicate would not skip these as specified**: both are FastClean and the
  safe predicate restricts to `Fast`. If they turn out byte-identical,
  either the restriction costs A14 its most common case or the boot and wake
  double-paint is a separate item.

**The measurement was taken, 2026-09-12, on main `0fdde34`.** `identical`
and `cmp_us` were added to `bench: render`, and a `bench: plate` line to the
loading plate, which flushes without a render record and would otherwise
have gone uncounted. Nothing branched on either; this counted only. One
`reader-soak`, 230 renders and 29 plates. The instrumentation was not
landed, since paying the compare without the skip buys nothing.

**A14 lives, by a wide margin, and the cost model is better than the item
assumed.**

| | renders | identical |
|---|---|---|
| `bench: render` | 230 | 8 (3.5%) |
| loading plate | 32 | **32 (100%)** |
| both | 262 | 40 (15.3%) |

The compare does not cost one price. It early-exits on the first differing
byte, so a miss is **14 to 16 µs** and only a hit pays the full scan of the
frame, **4.6 to 5.3 ms**. The item's 250 to 800 µs estimate described
neither. The common path is 40 times cheaper than estimated, and the
expensive path is the one that saves 435 ms, so the item's "over ~5 ms,
revisit" trigger was written for a cost that is not paid unconditionally.
Break-even on the measured mean is **0.17%** against a measured **3.5%**,
about a 20x return on the render path alone before the plates are counted.

**The named suspicion from #89 is disproved, and it was the item's main
evidence.** Both patterns it described are there in the capture, a boot's
Full followed 446 ms later by a FastClean of the same view and page, and a
wake's FastClean followed by a Fast of the same view and page, 46 such
pairs across the run. **None of them is byte-identical.** So the "~4-6% of
renders are candidates" reading was wrong, and with it the worry that
restricting the predicate to `Fast` would cost A14 its most common case.
The restriction costs nothing: every one of the 8 render hits is already
`Fast`, and so is every plate.

**Where the hits actually are**, which is a different inventory than the
item expected:

- **The loading plate, 29 of 29.** Every plate drawn during the soak
  already matched the glass. On its own that is 12.6 s of panel time in one
  run, and it needs no predicate subtlety: the plate is opportunistic, the
  app is not waiting on it, and a skip there is a pure win.
- **The Loaded repaint, 7 hits,** in pairs 772 ms apart on pages 1 to 4:
  the second Fast refresh of A18, when the loaded text happens to match
  what the placeholder drew. Later pages in the same run show the pair with
  differing bytes, so this is a fraction of A18's cases rather than all of
  them, and A14 and A18 are complements rather than substitutes.
- **The end-of-book redraw, 1 hit,** the case the item predicted, confirmed
  at page 302.

**Both halves are answered, and the reading rate is the higher one.** The
soak's own renders split by where the reader was:

| | renders | identical |
|---|---|---|
| Ordinary reading, pages 0 to 297 | 172 | 7 (4.1%) |
| At the book's last page | 4 | 1 (25%) |
| Library, Home, Chapters | 54 | 0 |
| Loading plates, two runs | 32 | **32 (100%)** |

So a reading session runs at **4.1%** over 116 distinct pages, better than
the 3.5% across all renders, and the menus contribute nothing. A dedicated
`page-turn` session was attempted twice and is not where this figure came
from: the device resumes book index 0 at page 297 of 303, so both runs met
the end of the book within six turns. The soak is the larger and more
varied sample in any case, 116 pages against a session's 50.

**Worth a look while building:** 52,272 bytes compared in 4.6 ms is about
11 MB/s, slow enough to suggest a byte-wise compare rather than a word-wise
one. It does not change the verdict, and a faster compare would only widen
the margin.

### A15 (unresolved — measure before ranking): `fill_plane`'s 528 single-row transfers

`fill_plane` (`fw/src/display_flush/uc8253.rs:228-241`) writes the white plane
one 99-byte row at a time — 528 `ram_chunk` calls — where `send_plane` streams
the same 52,272 bytes in 7 banded transfers. It runs in `init_panel` (twice)
and for every `FrameSource::White` in the Full plans.

**Two surveys costed this and disagree by an order of magnitude**, so it is
recorded here unranked rather than guessed at:

| | per-transaction overhead | cost of the fill |
|---|---|---|
| Display survey | 0.16–0.26 ms | ~87–139 ms |
| Power survey | ~9.5 µs | ~10 ms, "not worth the RAM" |

Neither reconciles the third data point: a banded plane write is ~26 ms of
which ~20.9 ms is wire time at 20 MHz, leaving ~5 ms across 7 transfers —
~0.7 ms each, higher than *either* estimate. The per-transaction model is
therefore not identified, and no one should write code against it.

**The one measurement, ~20 minutes:** wrap `fill_plane` and `send_plane` in
`Instant::now()` prints on X3 and compare for the same 52,272 bytes. If they
come out equal (~25–30 ms), there is nothing here. If the fill is 100 ms+,
it is worth ~85–140 ms per Full refresh and ~170–280 ms of boot, for a
792 B stack local. X3-only — the X4 fills RAM with `CMD_AUTO_WRITE_*_RAM`.

### A16 (S, fold into adjacent work): three measured micros

1. **Prestage after a `Full` flush is a verbatim duplicate.** The Full plans
   already end with `WritePlane(Old, Current)` + `DataStop`, and the display
   task then unconditionally writes DTM1 = current again. **32 ms**, on ~6 of
   479 renders. Skip the prestage when the plan already synced Old.
2. **The loading plate leaves the next real render unstaged.** The plate sets
   `prev_prestaged = false` and never prestages, so the first page render
   after a book open takes the unstaged path: **447 ms against 414 ms** for
   every other Fast, measured — +33 ms on-path, while the seconds of book
   building right afterwards are entirely idle panel time. Prestage after the
   plate flush.
3. **`load_bank` reloads identical LUTs on every flush** — six commands, 12
   SPI transactions, ~2–3 ms/turn. Cache the last `(bank, cdi)` in the driver
   and invalidate on init, sleep and failure. Only worth doing alongside A15,
   since it is the same overhead constant.

### A11 (M): batch landscape glyph rows

### A11 (M): batch landscape glyph rows

Fallout from #50: **landscape is now the slower frame per page** — host, one
full reading page, landscape 146 µs against portrait's 93 µs. Landscape still
calls `blit_row` once per glyph row, re-paying per-row native-y computation,
frame match, and `reverse_bits` + `blit_native_bits` per source byte: roughly
700 ops per 16-row glyph against portrait's ~270. A whole-glyph landscape
path inside `blit_bitmap` should close most of the gap — batch the row setup;
the byte writes are already 8 pixels wide, so the win is setup amortization,
not fewer stores.

Lower priority than the portrait work was: portrait is the default
orientation (#5), landscape reading layout is already 16–18 ms, and the whole
term is single-digit milliseconds against a 424 ms turn. **Size the win
before building it.**

### A9 (L): overlapped SPI DMA band transmits

Double-buffered SPI DMA so band *N+1* is prepared while band *N* is on the
wire, taking flush transmission down to the hardware clock limit. This is
also the only honest home for prestage overlap: genuine overlap with panel
BUSY needs this machinery and cannot cancel a plane write mid-transaction.

### A4 (S code, medium hw risk): skip the RED-plane write when CTRL1 bypasses it

Non-Fast flushes write the same framebuffer to BW and RED
(`fw/src/display_flush/ssd1677.rs:55-62`, ~23 ms each), but `update_control_1`
for Full/FastClean is `[0x40, 0x00]` (`display/src/epd/ssd1677.rs:148-153`) —
the RED-bypass bit — and the prestage overwrites RED immediately afterwards.

Needs a hardware A/B before shipping; cold-boot ghost clearing is the
sensitive case. The emulator panel model validates RED writes, so its op plan
changes too (`tools/emulator/src/panel.rs`).

### A5 (M, high hw risk, opt-in only): temperature-override "hot" LUT for Fast

**Scope corrected 2026-07-30: this is an X4 (SSD1677) item and must not be
sized against the X3's 379 ms.** The X4's fast waveform is a sensed-temperature
OTP LUT, which is what this overrides. The X3 has no OTP waveform at all — it
uploads its LUTs every flush, so its floor is software-defined and the lever
there is A12, not this. The claim "the only lever below the floor" was true
only of the X4 and is what kept A12 invisible for four rounds. Note also that
**the owner has no X4**, so this item cannot be validated by anyone currently
working on the project.

`FastClean` already proves the mechanism
(`FAST_CLEAN_TEMPERATURE` = 90 °C, skip the load-temp bit, restore after).
Apply a moderate override (35–50 °C) to `Fast` as an **opt-in RefreshPolicy
tier, never the default**. `RefreshMode` is shared with the emulators and is
never forked per panel.

Potentially 60–120 ms/turn. Risks: ghosting and contrast, unit and
temperature variance, more frequent FastClean eating the win. The emulator
cannot model waveform physics — this is pure hardware validation
(`page-turn` BUSY distribution, `thermal-run` cold/warm, long `reader-soak`
for ghost accumulation).

### A18 (finding, 2026-09-10): a turn across a one-page section costs two Fast refreshes

Found while calibrating the unattended page-turn injector against a hand
(#90). On a book whose sections are a page long, a turn that crosses a
section sends an extend, and `loaded_repaints` repaints the page when the
section loads: a second Fast refresh carrying the replaced text, requested a
median of 2 ms after the first frame settles with a tail to 1,071 ms. An
injector pressing the instant the first frame settled superseded and hid
it (56 refreshes in 50 turns); at reading cadence the hand and the injector
both pay it (82 and 68 refreshes in 50). The reader sees the page paint
twice, and on a slow section load the second paint can land more than a
second after the first.

What it is not: a duplicate-frame problem A14 would catch, since the two
frames differ (placeholder text, then loaded text). Whether the first paint
should wait for a load that usually completes within the flush, or the
second should be suppressed when the loaded text matches what was drawn, is
a design question for whoever next touches `loaded_repaints`. Measure the
section-load distribution first; the fix depends on where the 1,071 ms tail
comes from.

## Done

- **A1** (#12) — send `DisplayEvent::Settled` before the prestage and chapter
  tracking. This is what put the prestage off the critical path, and it is
  why A7 was backwards.
- **A2 / A2-P** (#24, #46) — byte-run rasterizer fast paths. `fill_span` and
  `blit_row` hoist the `map()` transform, bounds checks, bit-shifts and
  division out of the inner loop. Landscape reading layout 16–18 ms; portrait
  reached 28–34 ms; menus 82–90 → 3–8 ms.
- **A3** (#46) — panel-native framebuffer byte order. `Framebuffer::data` is
  stored in native order, bit-reversal and mirroring fold into the indexing
  math, `fill_transformed_band_impl` and `REVERSE_BITS_LUT` are gone, and the
  8 KB `TX_BAND` static is freed. Firmware streams `fb.band()` zero-copy over
  SPI. Goldens re-blessed deliberately.
- **A6** (#47) — O(1) ASCII direct indexing for glyph advance lookups, and a
  bypass of the kerning search when the font has no kerning entries.
- **A10-as-shipped** (#50) — **not what A10 specified.** A frame row runs down
  a native *column* in portrait, so `blit_row` could place only one pixel per
  read-modify-write and re-paid the column setup for each of a glyph's rows.
  Eight consecutive frame rows land in eight bits of the *same* native byte,
  so transposing an 8×8 block of the glyph (`transpose8`, Hacker's Delight
  7-3) collapses 64 masked writes into 8. `draw_glyph` and the SD custom-font
  path both route through it; empty blocks skip the transpose and the writes.
  **Portrait reading layout 33 → 13 ms median on X3 (2.5×, −20 ms/turn.)**
  Host predicted 2.1×; the device gained more, as expected — the removed work
  is dependent per-pixel loads, stores and unpredictable branches that an
  out-of-order host core hides and the C3 pays in full. Cost +852 B `.text`,
  +16 B `.rodata`, no `.bss` or stack change. `blit_bitmap` is pinned against
  the row-at-a-time loop across all four frames, both boards, every clipping
  edge; all goldens passed unblessed.

## Do not re-propose

- **A10 as originally specified — line-wrap / layout caching for the reading
  view.** Measured false. There is no wrapping in the render path to cache:
  the EPUB sink wraps at cache-build time, `push_line_block` stores one
  physical line per `BlockRecord` with `line_count: 1`, and `reader_page_at`
  is already an O(1) index into `ReaderStore::pages`. Host measurement of one
  portrait page: **2.8 µs** of pagination against **194 µs** of drawing.
  Reading-view render time moves on rasterizer work only.

  A first attempt (reverted) only flipped `block_height` to trust
  `BlockRecord::line_count` above one. Zero device effect, and it made an
  unvalidated cache byte authoritative geometry — a damaged byte could give
  one short block up to `255 * advance` of height. If paragraph-sized blocks
  are ever stored, that count needs validating against the block text on load,
  or a versioned multiline block format.

  **The premise survived three roadmap revisions because nobody measured the
  layout/rasterization split before ranking it first.** Any future "cache the
  layout" proposal needs that measurement before it is ranked.

- **A7 — skipping or deferring the RED prestage** (#48, reverted #52).
  Measured backwards on both counts.

  *It never fired.* Across two X3 50-turn captures at opposite cadences the
  prestage ran on **100 of 100** renders and the skip's own log line printed
  **zero** times — not for lack of queued work, since the burst capture
  contains a render with no preceding input. One `embassy_futures::yield_now()`
  is a single scheduling pass and cannot cover the round trip from the display
  task sending `Settled` to the app task clearing `rendering` and pushing the
  next `RenderRequest`.

  *And firing would have cost time.* The prestage write is already off the
  critical path thanks to A1. Skipping does not delete it — it defers it into
  the next Fast flush, ahead of `DisplayRefresh`, where the reader does wait:

  | | Next Fast flush |
  |---|---|
  | Prestaged | LoadBank → WritePlane(New) → DisplayRefresh |
  | Unstaged | LoadBank → **WritePlane(Old, Previous) → DataStop** → WritePlane(New) → DisplayRefresh |

  `fast_plan_only_writes_previous_plane_when_not_prestaged` pins that
  asymmetry and the X4 writes RED from `prev_fb` for the same reason. The skip
  is self-sustaining too — each skipped turn leaves the next unstaged — so a
  held button pays the write on-path *every* turn instead of off-path once.
  The reasoning lives as a comment at the call site. Genuine overlap is A9.

- **A8 — 4-way strided unrolling of the portrait `blit_row` column loop**
  (#49 closed). #50 removed that loop from the glyph path and took 2.5×
  instead of A8's projected 20–30%. Only `fill_span` is left for it, which is
  menu furniture already at 3–8 ms.

- **`wait_ready`'s fixed 1 ms pre-delay** — already fixed on `main` and
  removed from this file. `hal-ext/src/spi_dma.rs` now does a bounded 2 ms
  edge wait returning `NeverAsserted`, and the comment records the change. It
  never applied to the X3 in any case, which uses `wait_two_phase`.

### A17 (S, X4 only, needs device A/B): unconditional Fast-DU `0x1C` shortcut

**New 2026-08-22, surfaced from the sibling-repo cross-reference sweep.** The
`RefreshMode::Fast` arm in `display/src/epd/ssd1677.rs:143` uses the `0x1C`
display-update shortcut unconditionally. Upstream (crosspoint-reader) **made
this opt-in after ghosting and blotching reports on real X4 units**. The
neighbouring `FastClean` arm carries a "deliberately" comment explaining its
bit choice; the `Fast` arm carries none, so the unconditional `0x1C` currently
reads as considered when it is inherited.

crosspoint-reader's implementation lives on branch
`origin/feature/fast-du-setting` (tip `7463c78`), which makes the shortcut a
user-configurable setting rather than a constant. The reasoning was recovered
from a dangling `ssd1677-production-fixes` PRD (commit `bc96b25` in this
repo's UC8179 PRD comments).

- Impact: quality, not speed. The shortcut may cause ghosting or blotching on
  some X4 panels; disabling it would use the full DU waveform, which is
  correct but carries the same BUSY time.
- **The one measurement:** photograph 20 consecutive Fast turns on an X4 unit
  with `0x1C`, then repeat with the full DU command, and compare ghost
  residue. **Kills it:** no visible difference.
- Risk: **the owner has no X4 hardware**, so this cannot be validated here.
  Record it and leave it for someone with an X4 to photograph.
- Cross-reference: UC8179 PRD comments (2026-08-13 rescued PRD paragraph),
  `origin/fix-rst-pin-hold` (`d34a733`).

## Do not re-propose

- Partial-window refresh (shelved twice), SPI above 40 MHz (rated ceiling),
  `MIRROR_Y=true` (tested, wrong), software work on the Full waveform (noise).
  RED prestaging itself already exists — A1 and A3 build on it.
  **Note the Full-waveform "noise" verdict is an X4 statement too** — the X3's
  Full is 62 LUT frames it uploads itself, and A12's constant applies to it
  like any other mode.

- **Batching `send_plane`'s band transfers further, or A9's double-buffered
  DMA, on the X3.** Bounded by measurement: a plane write is ~26 ms of which
  ~20.9 ms is wire time at the datasheet-ceiling 20 MHz, so the entire
  addressable overhead is ~5 ms and A9 has at most ~3 ms to win there. A9 also
  re-spends the 8 KB `TX_BAND` that A3 freed, on the board that is tight on
  RAM. A9 stays in the queue only as the home for *prestage* overlap; it is
  not a bandwidth item on the X3. `fill_plane` is a separate question — A15.

## Bench protocol, learned the hard way

`page-turn` is operator-driven and its `page turn` statistic is
input→next-render, so it is **not cadence-robust**: a press landing mid-render
is credited only with the remainder, and burst captures show durations as low
as 2 ms. Quote `page turn` only from deliberate cadence — one press per
settled page. `layout_ms`, `flush_ms` and `busy_ms` are per-render and safe
from either.

Captures taken before the 2026-07-27 instrumentation split (#54) printed the
render event *after* the prestage, so their `page turn` runs ~24 ms long by
construction. **Do not compare them against newer runs.** The 354 ms figure
that once appeared as a baseline was a burst-cadence artifact and is retired;
it is not a number this system ever hit.
