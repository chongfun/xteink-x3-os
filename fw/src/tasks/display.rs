use crate::book_build::{self, ReaderCacheScratch};
use crate::display_flush::{self, Epd};
use crate::{
    DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, StorageCommand, DISPLAY_COMMANDS,
    DISPLAY_EVENTS, LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS,
};
use app_core::storage_loop::{
    loop_arm, Drained, LoopArm, OpenAction, OpenSequence, SleepAction, SleepRefusal, SleepSequence,
};
use app_core::{
    book_open_outcome, display_orientation_from_u8, refresh_policy_from_u8, AppView, ChapterCursor,
    DisplayEventHolder, DisplayHoldOutcome, DisplayOrientation, EvictionStep, EvictionWalk,
    HoldOutcome, LibraryEventHolder, PersistedAppState, ReaderSource, RefreshPlanner, RenderKind,
    RenderRequest, SyncSession, SyncStatus,
};
use core::cell::Cell;
use core::sync::atomic::Ordering;
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use embassy_futures::select::{select, select5, Either, Either5};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Output;
use proto::nvm::AppStateRecord;
use reader_cache::store::{
    BookLoadStatus, LibraryScanStatus, ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS,
};
use reader_cache::{
    READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH, READER_HEADER_SCRATCH, READER_OPF_SCRATCH,
    READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use static_cell::ConstStaticCell;

/// Same-book page-turn progress is coalesced: at most one durable state write
/// per this interval, with a guaranteed flush before display sleep. A
/// battery pull can lose at most this many seconds of reading position.
const PROGRESS_WRITE_MIN_SECS: u64 = 15;

static EPUB_TAIL: ConstStaticCell<[u8; READER_TAIL_SCRATCH]> =
    ConstStaticCell::new([0; READER_TAIL_SCRATCH]);
static EPUB_HEADER: ConstStaticCell<[u8; READER_HEADER_SCRATCH]> =
    ConstStaticCell::new([0; READER_HEADER_SCRATCH]);
static EPUB_NAME: ConstStaticCell<[u8; proto::epub::MAX_ENTRY_NAME_BYTES]> =
    ConstStaticCell::new([0; proto::epub::MAX_ENTRY_NAME_BYTES]);
static EPUB_COMPRESSED: ConstStaticCell<[u8; READER_COMPRESSED_SCRATCH]> =
    ConstStaticCell::new([0; READER_COMPRESSED_SCRATCH]);
static EPUB_CONTAINER: ConstStaticCell<[u8; READER_CONTAINER_SCRATCH]> =
    ConstStaticCell::new([0; READER_CONTAINER_SCRATCH]);
static EPUB_OPF: ConstStaticCell<[u8; READER_OPF_SCRATCH]> =
    ConstStaticCell::new([0; READER_OPF_SCRATCH]);
static EPUB_XHTML: ConstStaticCell<[u8; READER_XHTML_SCRATCH]> =
    ConstStaticCell::new([0; READER_XHTML_SCRATCH]);
static EPUB_BOOK_SECTIONS: ConstStaticCell<[proto::cache::BookV2SectionRecord; MAX_BOOK_SECTIONS]> =
    ConstStaticCell::new([EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS]);
// Const-constructed like every other scratch above, which is the whole point
// of `ZipInflateScratch`'s shape: it is ~32 KB against a 42 KB stack, so it
// must reach `.bss` without ever being built as a value. A `StaticCell` here
// meant writing one through a pointer, and that left a ~21 KB frame.
static EPUB_ZIP_INFLATE: ConstStaticCell<proto::epub::ZipInflateScratch> =
    ConstStaticCell::new(proto::epub::ZipInflateScratch::new());
// Separate from the scratch above, and deliberately: miniz builds this ~10 KB
// decoder only by value, so holding it *inside* the const scratch would make
// that const no longer provably all-zero and push the whole 43 KB out of
// `.bss` and into `.data` — 43 KB of flash, copied at every boot. An uninit
// `StaticCell` stays in `.bss` and pays for the value once, here.
static EPUB_DECOMPRESSOR: static_cell::StaticCell<proto::epub::DecompressorOxide> =
    static_cell::StaticCell::new();
static EPUB_SCRATCH: static_cell::StaticCell<ReaderCacheScratch<'static>> =
    static_cell::StaticCell::new();

/// How many times an open will build further to pin down a stored place.
/// Each step covers at least one more section, and the bound stops a book
/// whose index refuses to grow from holding the open open.
const PLACE_RESOLVE_STEPS: usize = 8;

#[embassy_executor::task]
pub async fn run(
    mut epd: Epd,
    mut sd_cs: Output<'static>,
    deep_sleep_wake: bool,
    probe_diag: hal_ext::epd_probe::ProbeDiag,
) {
    esp_println::println!("display: started t_ms={}", Instant::now().as_millis());

    static FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB.init(Framebuffer::new());
    // The previous-frame buffer sits in dram2 so the radio's statics fit
    // in main DRAM; same exclusive &'static mut as the old local cell.
    let prev_fb = crate::sync_mem::take_prev_fb().expect("prev_fb claimed once");

    let mut epub_scratch = None;
    // Storage-command admission for the sync session lifecycle; the loan
    // transition and refusal rules live in app-core with the contracts.
    let mut sync_session = SyncSession::default();
    // The book whose spine walk is still running in the background after a
    // progressive open published it early, if any. The walk's own state lives
    // beside the section records it owns, in the EPUB scratch; this is only
    // what the loop needs to schedule the next step and to tell whether the
    // reader is still on that book.
    let mut background_build: Option<BackgroundBuild> = None;
    // The place whose bytes this session has already read, or found the
    // card already knew. A place rather than a row number: a rescan
    // renumbers rows, and a row that comes back as another book would
    // otherwise be taken for one already read.
    let mut evidence_settled: Option<book_build::EvidencePlace> = None;
    let mut pending_evidence: Option<book_build::SourceEvidenceJob> = None;
    // On a deep-sleep (Power button) wake the panel still shows the sleep
    // screen: deep_sleep_wake is true only when the RTC wake cause is the
    // armed GPIO *and* the pre-sleep handshake recorded that the sleep frame
    // settled on the panel (see sleep_marker). The seeded planner then picks
    // the ~1.5 s one-flicker FastClean for the wake render instead of the
    // ~3.5 s multi-flash Full. Any other boot — battery pull, crash, software
    // reset, or a sleep whose final flush failed — leaves the seed false and
    // keeps the full waveform for unknown panel contents.
    let mut refresh_planner = RefreshPlanner::new().with_panel_shows_sleep_screen(deep_sleep_wake);
    let mut pending_progress: Option<AppStateRecord> = None;
    let mut last_progress_write: Option<Instant> = None;
    // Durable state is consulted once per boot, after the first catalog with
    // entries lands; later catalog refreshes must not yank reading state.
    let mut state_restored = false;
    // True while RED RAM is known to hold exactly prev_fb's content, letting
    // a fast refresh skip its previous-frame stream. Reset on any failure,
    // sleep, or panel re-init; false just means the next flush writes RED.
    let mut prev_prestaged = false;
    static SD_LIBRARY: ConstStaticCell<ReaderStore> = ConstStaticCell::new(ReaderStore::new());
    let sd_library = SD_LIBRARY.take();
    // ReaderStore::new() is all-zero bytes so the 47 KB static lives in
    // .bss (not a flashed .data image); fill in the non-zero defaults once,
    // in place, before anything reads the store.
    sd_library.init_runtime_defaults();
    // ASCII glyph metrics for the custom font pack; shared by the build's
    // line measurement and the reading-page draw so both stay off the card.
    static FONT_METRICS: ConstStaticCell<crate::custom_font::MetricCache> =
        ConstStaticCell::new(crate::custom_font::MetricCache::new());
    let font_metrics = FONT_METRICS.take();

    // No panel init here: the first-render guard in the loop below (fresh
    // planner — screen off, no last request) owns the boot init, exactly as
    // it already owned re-init after a display sleep. Initializing at task
    // start too made every boot's first render pay reset + init twice (on
    // the X3 that second pass re-whitens both ~52 KB DTM planes).

    // One-shot firmware self-update: if the card holds a pending image, flash it
    // into the inactive OTA slot and reboot into it before the reader starts.
    // Runs here because SD access lives behind this task's shared SPI bus, and
    // the radio is still idle so the flash writes are safe. Runs on every boot,
    // deep-sleep wakes included: the card is user-removable, so an update can
    // be staged offline while the device sleeps and arrive through a Power-
    // button wake — wifi-staged updates are not the only source. The no-
    // trigger probe costs one failed open on the mounted root, and the cold
    // card init it pays is one the first render's SD reads would pay anyway.
    match crate::sd_session::with_root(
        &mut epd,
        &mut sd_cs,
        crate::ota_update::apply_pending_update,
    ) {
        Ok(outcome) if outcome.needs_reset() => {
            esp_println::println!("display: {:?}; resetting", outcome);
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
            esp_hal::system::software_reset();
        }
        Ok(_) => {}
        Err(e) => esp_println::println!("display: update check skipped: {:?}", e),
    }

    // Park the boot probe's verdict on the card. It goes here, on the storage
    // task, because SD access lives behind this task's shared SPI bus — and
    // after the update check, so a boot that is about to reflash and reset
    // does not spend a card write on a report the new image will rewrite. The
    // card is warm by now, so this is a file write, not another cold acquire.
    crate::probe_report::write(&mut epd, &mut sd_cs, &probe_diag);

    // Flash-path self-test (feature `ota-selftest` only, off in release): copy
    // the running image into the inactive slot and boot into it, once. A card-
    // reader-free way to re-validate the esp-storage + otadata path on device.
    #[cfg(feature = "ota-selftest")]
    if crate::ota_update::run_selftest() {
        esp_println::println!("selftest: staged; resetting");
        embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
        esp_hal::system::software_reset();
    }

    loop {
        // Three ways to make progress, and the third is why the first two are
        // not enough on their own. A settling event with no room in the
        // channel is held here rather than dropped, and placing it means
        // waiting for the app task to drain — which the app task cannot do
        // while it is blocked handing this task a render. So the wait for
        // room runs *beside* the display queue, not in front of it: servicing
        // a render is what releases the consumer that frees the slot.
        //
        // Storage stands down while something is held. It is the producer of
        // settling events, the holder has one slot, and a second command
        // could fill it with nowhere for the first to go.
        //
        // The fourth is the same waiting-for-room branch for a render
        // acknowledgement, and it constrains nothing: rendering is what
        // produces acknowledgements, but it is also what releases the app to
        // drain them, so standing down would be waiting on itself.
        //
        // The fifth is the only one nobody else is waiting on: it is work this
        // task already owes itself, so it comes last on purpose and runs a
        // slice of a suspended book build whenever the four above have nothing.
        // It waits before claiming the loop, which is what keeps a ready branch
        // from starving the others — and what gives every other task a
        // scheduling point between slices, where a single unsliced build used
        // to hold the executor for a minute. A yield was not enough of a wait:
        // it hands the app task one poll, which does not cover receiving a
        // button, reducing it and sending the render, so the loop would commit
        // to another multi-second slice ahead of a page turn already pressed.
        // See `background_build_step_due`.
        match select5(
            DISPLAY_COMMANDS.receive(),
            storage_command_while_free(),
            place_held_library_event(),
            place_held_display_event(),
            background_build_step_due(
                (background_build.is_some()
                    || pending_evidence.is_some()
                    || book_build::evidence_place(sd_library)
                        .is_some_and(|place| evidence_settled.as_ref() != Some(&place)))
                    && !sync_session.active()
                    && holder().storage_may_run()
                    && !sd_library.text_holds_toc(),
                background_build.map_or(0, |pending| pending.attempts),
            ),
        )
        .await
        {
            Either5::Fifth(()) => {
                let Some(pending) = background_build else {
                    // No walk owed, so the slice goes to reading the open
                    // book's bytes, which is the other thing this task owes
                    // itself and the one that has to wait for the reader to
                    // have a page before it starts.
                    let open_place = book_build::evidence_place(sd_library);
                    // A job follows the book that is open. The reader
                    // moving on takes this one with them, half read: the
                    // copy they moved to is the one whose bytes are worth
                    // having, and the one they left is read again whenever
                    // it is opened again.
                    if pending_evidence
                        .as_ref()
                        .is_some_and(|job| open_place.as_ref() != Some(job.place()))
                    {
                        pending_evidence = None;
                    }
                    if pending_evidence.is_none() {
                        if let Some(place) = &open_place {
                            if evidence_settled.as_ref() != Some(place) {
                                pending_evidence = book_build::evidence_job(sd_library);
                                if pending_evidence.is_none() {
                                    evidence_settled = Some(place.clone());
                                }
                            }
                        }
                    }
                    if let Some(job) = &mut pending_evidence {
                        let place = job.place().clone();
                        match book_build::continue_source_evidence(&mut epd, &mut sd_cs, job) {
                            book_build::EvidenceStep::Continued => {}
                            // Settled either way: a copy the card would not
                            // give up is not asked for again in this
                            // session, and a book reopened after one is
                            // asked about afresh.
                            book_build::EvidenceStep::Finished
                            | book_build::EvidenceStep::Abandoned => {
                                pending_evidence = None;
                                evidence_settled = Some(place);
                            }
                        }
                    }
                    continue;
                };
                // Deliberately not gated on the latest reader request id.
                // Reading normally through a background build issues extends
                // and bumps that id constantly, and every one of them has
                // already passed through `apply_build_outcome`, which is what
                // decides whether the walk survived. The step itself re-checks
                // the catalog row it is building against the card.
                let advertised_before = sd_library.advertised_page_count();
                // Where the reader is now, which is the page each step must
                // leave resident. Only a Reading render carries a global page;
                // from anywhere else fall back to the book's start, which any
                // later page turn extends from.
                let reader_page = refresh_planner
                    .last_request()
                    .filter(|request| {
                        request.book_id == pending.book_id && request.view == AppView::Reading
                    })
                    .map_or(0, |request| request.page);
                let scratch = ensure_epub_scratch(&mut epub_scratch);
                let step = book_build::continue_book_build(
                    &mut epd,
                    &mut sd_cs,
                    sd_library,
                    reader_page,
                    scratch,
                    font_metrics,
                );
                let finished = step == book_build::BackgroundStep::Finished;
                match step {
                    book_build::BackgroundStep::Continued => {
                        // A step that ran clears the budget: it is consecutive
                        // failures to begin that mean the card is gone, not a
                        // single one somewhere in a minute of building.
                        background_build = Some(BackgroundBuild {
                            attempts: 0,
                            ..pending
                        });
                    }
                    // Nothing was touched and the walk is re-armed, so it is
                    // simply kept — for as long as this book stays open, however
                    // long the card is away. A reader at the frontier has no
                    // page turn that would provoke a rebuild, which leaves this
                    // walk as the only thing that can still raise their page
                    // count; there is nothing to hand the job over to. The wait
                    // before the next attempt is what makes holding on
                    // affordable, and the walk still dies the moment the book
                    // changes or its cache is cleared.
                    book_build::BackgroundStep::Retry => {
                        let attempts = pending.attempts.saturating_add(1);
                        esp_println::println!(
                            "storage: background build retry {} in {} ms book_id={}",
                            attempts,
                            app_core::storage_loop::background_retry_delay_ms(attempts),
                            pending.book_id
                        );
                        background_build = Some(BackgroundBuild {
                            attempts,
                            ..pending
                        });
                    }
                    _ => background_build = None,
                }
                if finished {
                    esp_println::println!(
                        "storage: background build done book_id={} pages={}",
                        pending.book_id,
                        sd_library.advertised_page_count()
                    );
                    bench_log!(
                        "bench: storage_background_build book_id={} pages={} elapsed_ms={}",
                        pending.book_id,
                        sd_library.advertised_page_count(),
                        pending.started.elapsed().as_millis(),
                    );
                }
                // Announcing forces a full repaint, so an abandoned step may
                // never do it: its store may be mid-move and the arena may
                // still hold whatever the builder touched last rather than the
                // page on screen. Silence leaves the panel showing the frame it
                // already has, and the next page turn issues an ordinary extend
                // that reloads properly.
                let announce = match step {
                    book_build::BackgroundStep::Abandoned => {
                        esp_println::println!(
                            "storage: background build abandoned book_id={}",
                            pending.book_id
                        );
                        false
                    }
                    // The walk is over, but it grew the book before it broke and
                    // left the store whole. Those pages are on the card and the
                    // resident index reaches them; only the app's page count is
                    // behind, and at the frontier that count is what makes the
                    // next-page button do nothing.
                    book_build::BackgroundStep::Stopped => {
                        esp_println::println!(
                            "storage: background build stopped book_id={} pages={}",
                            pending.book_id,
                            sd_library.advertised_page_count()
                        );
                        app_core::storage_loop::stopped_announce(
                            advertised_before,
                            sd_library.advertised_page_count(),
                            reader_page,
                        )
                    }
                    // Not one page was built, so there is nothing to say and a
                    // repaint would only redraw the frontier the reader is
                    // already looking at. The walk being kept is the answer
                    // here, not the announcement.
                    book_build::BackgroundStep::Retry => false,
                    book_build::BackgroundStep::Continued
                    | book_build::BackgroundStep::Finished => {
                        app_core::storage_loop::background_announce(
                            finished,
                            reader_page,
                            advertised_before,
                        )
                    }
                };
                if announce {
                    // `position: None` — the book grew, the reader did not
                    // move, and adopting a page here would yank them.
                    send_loaded_library_event(&LibraryEvent::Loaded {
                        book_id: pending.book_id,
                        pages: sd_library.advertised_page_count(),
                        chapters: sd_library.chapter_count_for_ui(),
                        current_chapter: sd_library.current_chapter(),
                        chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                        position: None,
                        // The step reloaded the reader's section on its way
                        // out, and this announce is only reached when the
                        // repaint is the point of it (`background_announce`).
                        text_replaced: true,
                    });
                }
            }
            Either5::Third(()) | Either5::Fourth(()) => {}
            Either5::First(DisplayCommand::Render(request)) => {
                // The dequeue instant. This is NOT the pairing boundary --
                // `request.requested_at_ms` is, stamped by the app as it froze
                // the state. A render can sit in the channel behind a flush, a
                // prestage, a storage command or a background build step, so
                // the two differ by however long this task was busy, and a
                // press arriving in that gap belongs to the *next* frame.
                // Reported as `deq_ms` purely so that queue wait is visible:
                // `deq_ms - req_ms` is the delay, and it was invisible before.
                let dequeued_at_ms = Instant::now().as_millis();
                let content_context_changed = refresh_planner
                    .last_request()
                    .map(|last| (last.view, last.book_id))
                    != Some((request.view, request.book_id));
                // The catalog is streamed from the card, so make the slice this
                // view needs resident before the (pure) render reads it. Library
                // pulls the list window around the selection; other views need
                // the active book's entry, refreshed only when the book changes.
                // Skipped once the sync session is running.
                if !sync_session.active() {
                    if request.view == AppView::Library {
                        // The list is the folder the reader is in, not a
                        // window over the flat catalog: slide the page over
                        // the rows this render will show.
                        crate::library_sd::ensure_folder_page(
                            &mut epd,
                            &mut sd_cs,
                            sd_library,
                            request.selection,
                            app_core::is_portrait(request.orientation),
                        );
                    } else if ReaderSource::from_book_id(request.book_id).is_sd() {
                        if let Some(index) = ReaderStore::selected_book_index(request.book_id) {
                            if content_context_changed {
                                crate::library_sd::load_active_entry(
                                    &mut epd, &mut sd_cs, sd_library, index,
                                );
                            }
                            // Long TOCs are windowed like the catalog; slide
                            // the window over the rows this render will show.
                            if request.view == AppView::Chapters && sd_library.text_holds_toc() {
                                book_build::ensure_toc_window(
                                    &mut epd,
                                    &mut sd_cs,
                                    sd_library,
                                    index,
                                    request.selection as usize,
                                    app_core::is_portrait(request.orientation),
                                );
                            }
                        }
                    }
                }
                let layout_start = Instant::now();
                if !render_custom_reader(
                    &mut epd,
                    &mut sd_cs,
                    fb,
                    request,
                    sd_library,
                    font_metrics,
                ) {
                    crate::views::render(fb, request, sd_library);
                }
                let layout_ms = layout_start.elapsed().as_millis();

                // Sole panel-init site: true for a boot's first render (fresh
                // planner) and again after any display sleep — record_sleep
                // clears last_request, which also covers the aborted-sleep
                // path where a late button press interrupts the handshake
                // after the panel already powered down.
                if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                    esp_println::println!("display: wake init start");
                    if let Err(error) = display_flush::init_panel(&mut epd).await {
                        // The panel never came up; flushing into it would
                        // stream into a dead controller. Fail this render —
                        // the app clears its render lock and the next
                        // request retries init from scratch.
                        esp_println::println!("display: wake init failed: {:?}", error);
                        prev_prestaged = false;
                        let (display_event, power_event) =
                            app_core::display_refresh_outcome(false, None);
                        send_display_event(&display_event);
                        send_required_power_event(power_event).await;
                        continue;
                    }
                    esp_println::println!("display: wake init complete");
                    prev_prestaged = false;
                }

                let mode = refresh_planner.mode_for(request);
                if content_context_changed {
                    esp_println::println!(
                        "display: context changed, refresh policy {:?} -> {:?}",
                        request.refresh_policy,
                        mode
                    );
                }
                let flush_start = Instant::now();
                if let Ok(settle) = display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    refresh_planner.screen_on(),
                    mode,
                    prev_prestaged,
                )
                .await
                {
                    let flush_ms = flush_start.elapsed().as_millis();
                    refresh_planner.record_render(request, mode);
                    prev_fb.copy_from(fb);
                    // Keep the current chapter tracking the page just shown, past
                    // the reducer's 128-chapter cap. Cheap in-RAM check; only the
                    // loaded SD reader has an uncapped page map, so this no-ops on
                    // other views and reads SD only when the chapter changes. It
                    // rides out inside Settled: the app must apply it before it
                    // clears the render lock, and one message is the only way to
                    // promise that (see DisplayEvent::Settled).
                    let chapter_cursor = if request.view == AppView::Reading {
                        book_build::track_reading_chapter(
                            &mut epd,
                            &mut sd_cs,
                            request.page,
                            sd_library,
                        )
                        .map(|current_chapter| ChapterCursor {
                            book_id: request.book_id,
                            page: request.page,
                            current_chapter,
                        })
                    } else {
                        None
                    };
                    // Settle before the ~23 ms RED prestage: the panel is visually
                    // done, so unblock the input/power pipeline. The prestage still
                    // runs on this task before the next command is dequeued, so
                    // `prev_prestaged` is always current by the next flush, and a
                    // Sleep queued by power_task after DisplaySettled waits behind it.
                    let (display_event, power_event) =
                        app_core::display_refresh_outcome(true, chapter_cursor);
                    let settled_at_ms = Instant::now().as_millis();
                    send_display_event(&display_event);
                    send_required_power_event(power_event).await;
                    // Emitted here, at the settle, and not after the prestage
                    // below. This timestamp is what the bench pairs each input
                    // against, so printing it later charged the reader for a
                    // write they never waited on: `Settled` has already gone
                    // out, and press-to-settled ends on this line.
                    bench_log!(
                        "bench: render view={:?} mode={:?} page={} chapter={} layout_ms={} flush_ms={} req_ms={} deq_ms={} t_ms={}",
                        request.view,
                        mode,
                        request.page,
                        request.chapter,
                        layout_ms,
                        flush_ms,
                        request.requested_at_ms,
                        dequeued_at_ms,
                        settled_at_ms,
                    );
                    // The clean plans' settle, deferred out of the flush so it
                    // falls after `Settled`. It guards the prestage below and
                    // nothing else, and the chapter read above now sits inside
                    // it rather than after it, so the gap cannot shrink while
                    // 200 ms comes off press-to-settled. Zero on other plans.
                    settle.wait().await;
                    let prestage_start = Instant::now();
                    // Unconditional, deliberately. Skipping this write when another
                    // render is already queued reads like a saving and is the exact
                    // opposite: it is off the critical path here — Settled has gone
                    // out, the glass is done, nobody is waiting — while the write it
                    // defers lands *inside* the next Fast flush, ahead of
                    // DisplayRefresh, where the reader does wait.
                    // `fast_plan_only_writes_previous_plane_when_not_prestaged`
                    // (display/src/epd/uc8253.rs) pins the asymmetry: an unstaged
                    // Fast carries an extra WritePlane(Old, Previous) + DataStop,
                    // and the X4 writes RED from `prev_fb` for the same reason
                    // (fw/src/display_flush/ssd1677.rs). The skip is also
                    // self-sustaining — each skipped turn leaves the next unstaged —
                    // so a held button would pay the write on-path every turn
                    // instead of off-path once.
                    prev_prestaged = display_flush::prestage_previous(&mut epd, fb).await.is_ok();
                    // Its own event, after the render one above: prestage is
                    // real work on this task and still gates the next command,
                    // but it sits outside press-to-settled and is measured
                    // separately so neither number can absorb the other.
                    bench_log!(
                        "bench: prestage staged={} elapsed_ms={} t_ms={}",
                        prev_prestaged,
                        prestage_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                } else {
                    esp_println::println!("display: SPI transfer failed");
                    prev_prestaged = false;
                    // The flush may have run partially, so the panel's RAM
                    // and waveform state no longer match the planner's model;
                    // forget it so the next render re-inits the panel and
                    // takes the full waveform instead of fast-diffing
                    // against a frame that may never have landed.
                    refresh_planner.record_failure();
                    let (display_event, power_event) =
                        app_core::display_refresh_outcome(false, None);
                    send_display_event(&display_event);
                    send_required_power_event(power_event).await;
                }
            }
            Either5::First(DisplayCommand::Sleep { generation }) => {
                let sleep_start = Instant::now();
                bench_log!(
                    "bench: sleep phase=requested screen_on={} t_ms={}",
                    refresh_planner.screen_on(),
                    sleep_start.as_millis(),
                );
                // A background build is deliberately *not* dropped here. Sleep
                // is terminal — waking is a fresh boot — so a walk that goes
                // down needs no clearing, and the book it left behind is a
                // valid partial cache whose frontier a later open rebuilds
                // past. A sleep that is refused, or a handshake the user
                // abandons, returns to the loop with the walk still standing,
                // which is what should happen: nothing about it was finished.
                //
                // It also stays out of the pre-sleep drain by construction.
                // The drain works the storage queue, and a background step is
                // not a queued command — it is a branch of the loop's select —
                // so it can never spend the drain's budget or delay the panel.
                //
                // Everything owed to the card, in order, before the panel goes
                // down. The ordering rules live in `SleepSequence` so they can
                // be driven from a host test; this arm only does what it is
                // told and reports back what the hardware said.
                let mut sleep = SleepSequence::new(STORAGE_COMMANDS.capacity());
                // The main loop keeps storage shut while an event is held; this
                // drain applies storage commands too, so it owes the same rule.
                // Checked before the first take and after every applied
                // command, because either end can be where the holder fills:
                // sleep can arrive with one already waiting, or the first
                // command drained can produce it. Carrying on past that point
                // is how a second completion would reach an occupied holder.
                let mut may_keep_draining = holder().sleep_may_proceed();
                let refusal = loop {
                    if !may_keep_draining {
                        break None;
                    }
                    match sleep.next() {
                        SleepAction::TakeQueued => match STORAGE_COMMANDS.try_receive() {
                            Err(_) => sleep.queue_empty(),
                            Ok(command) => match sleep.drained(&command) {
                                Drained::Apply => {
                                    esp_println::println!("storage: draining before sleep");
                                    handle_storage_command(
                                        command,
                                        &mut epd,
                                        &mut sd_cs,
                                        sd_library,
                                        font_metrics,
                                        &mut epub_scratch,
                                        &mut sync_session,
                                        &mut pending_progress,
                                        &mut last_progress_write,
                                        &mut state_restored,
                                        &mut background_build,
                                        last_portrait(&refresh_planner),
                                    );
                                    sleep.applied();
                                    may_keep_draining = holder().sleep_may_proceed();
                                }
                                Drained::RequeueAndRefuse => {
                                    // This send cannot fail today: nothing
                                    // between the receive above and here
                                    // awaits, and no task at interrupt priority
                                    // sends storage commands, so no producer
                                    // can take the slot the command just
                                    // vacated. Its answer is still taken rather
                                    // than assumed, because the only way it
                                    // could fail is a producer having refilled
                                    // the queue — which changes what the drain
                                    // must do next, and the sequence needs to
                                    // know.
                                    sleep.requeued(STORAGE_COMMANDS.try_send(command).is_ok());
                                }
                            },
                        },
                        SleepAction::FlushProgress => {
                            let stored = flush_pending_progress(
                                &mut epd,
                                &mut sd_cs,
                                sd_library,
                                &mut pending_progress,
                                &mut last_progress_write,
                            );
                            sleep.flushed(stored);
                        }
                        SleepAction::Refuse(refusal) => break Some(refusal),
                        SleepAction::Proceed => break None,
                    }
                };
                // A drained `ClearBookCache` can leave its completion held, and
                // sleep is terminal — waking is a fresh boot — so going down
                // now would take the event with it and strand the app in
                // `Busy`. Stay awake instead; the loop's placing branch runs
                // the moment this returns, the rest of the queue is still
                // there to drain, and the power task re-requests sleep after.
                let sleep_holds_an_event = refusal.is_none() && !holder().sleep_may_proceed();
                if sleep_holds_an_event {
                    esp_println::println!("display: sleep deferred; library event still held");
                }
                if refusal.is_some() || sleep_holds_an_event {
                    // Stay awake. The power task's idle clock re-requests sleep
                    // once this failure releases its handshake wait, by which
                    // time the upload session has run or the pending record has
                    // been retried by the next flush.
                    if let Some(refusal) = refusal {
                        match refusal {
                            SleepRefusal::UploadQueued => {
                                esp_println::println!(
                                    "display: sleep deferred; upload session pending"
                                )
                            }
                            // The request itself is gone, so the browser is left
                            // waiting on a writer that will not start and
                            // UPLOAD_SESSION_ACTIVE stays set. Nothing here can
                            // recover that. What this refusal does protect is the
                            // rest of the queue, which is full: the ordinary loop
                            // applies it before the next sleep attempt.
                            SleepRefusal::UploadLost => esp_println::println!(
                                "display: sleep deferred; upload request lost, storage queue full"
                            ),
                            SleepRefusal::ProgressUnwritten => esp_println::println!(
                                "display: sleep deferred; progress persistence failed"
                            ),
                        }
                    }
                    send_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                    continue;
                }
                let request = refresh_planner.last_request().or_else(|| {
                    sleep_request_from_saved_state(
                        &mut epd,
                        &mut sd_cs,
                        sd_library,
                        &pending_progress,
                    )
                });
                if let Some(request) = request {
                    crate::views::render_sleep(fb, request, sd_library);
                } else {
                    crate::views::render_sleep_blank(fb);
                }
                let sleep_frame_settled = if let Ok(settle) = display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    refresh_planner.screen_on(),
                    RefreshMode::Full,
                    prev_prestaged,
                )
                .await
                {
                    // Nothing writes panel RAM before the power-down below.
                    // `Full` owes zero; holding it keeps that the plan's fact
                    // rather than this call site's assumption.
                    settle.wait().await;
                    prev_fb.copy_from(fb);
                    bench_log!(
                        "bench: sleep phase=refresh ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    true
                } else {
                    esp_println::println!("display: sleep framebuffer flush failed");
                    bench_log!(
                        "bench: sleep phase=refresh ok=false elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    false
                };
                prev_prestaged = false;
                let panel_slept = display_flush::sleep_panel(&mut epd).await.is_ok();
                // Whenever the panel actually slept the planner must know the
                // screen is off — an aborted handshake (a late button press
                // beating DisplayAsleep) otherwise renders to a powered-down
                // panel without re-init. The settled flag rides along so a
                // failed flush wakes with the deep full waveform, not a fast
                // clean over stale pixels.
                if panel_slept {
                    refresh_planner.record_sleep(sleep_frame_settled);
                }
                // Persist whether the panel really holds the sleep frame
                // before DisplayAsleep releases the power task to cut power:
                // the next boot's GPIO wake seeds its fast-wake planner from
                // this marker, and a flush or panel-sleep failure must leave
                // it false so that boot falls back to the full waveform.
                crate::sleep_marker::record_sleep_image(panel_slept && sleep_frame_settled);
                if panel_slept {
                    // Emitted before the acknowledgement, not after the park:
                    // `DisplayAsleep` releases the power task to cut power, and
                    // deep sleep is terminal, so a line printed past that point
                    // only ever reaches the capture on an abandoned handshake.
                    // That left `phase=complete ok=true` absent from every
                    // successful sleep — the marker `sleep-sync` counts cycles
                    // by and the report reads as a terminal sleep.
                    bench_log!(
                        "bench: sleep phase=complete ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    send_display_event(&DisplayEvent::Asleep);
                    send_required_power_event(PowerEvent::DisplayAsleep(generation)).await;
                    park_until_resumed(generation).await;
                } else {
                    // The panel never acknowledged the sleep sequence, so it
                    // may still be mid-refresh. Cutting power now would
                    // freeze whatever is on screen; report failure so the
                    // power task stays awake and retries on its idle clock.
                    // The handshake may also have partially powered the
                    // controller down, so the planner's screen model is no
                    // longer trustworthy: forget it so the next render
                    // re-inits the panel with the full waveform.
                    refresh_planner.record_failure();
                    esp_println::println!("display: sleep transition failed");
                    send_display_event(&DisplayEvent::SleepFailed);
                    send_required_power_event(PowerEvent::DisplaySleepFailed(generation)).await;
                    // The failing path stays awake, so this one can be stamped
                    // where the phase actually ends.
                    bench_log!(
                        "bench: sleep phase=complete ok=false elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                }
            }
            Either5::Second(command) => match loop_arm(&command, sync_session) {
                // The display task is the upload writer until Sleep or
                // wireless Exit closes the session; a Sleep exit has
                // already been re-queued on DISPLAY_COMMANDS.
                LoopArm::UploadSession => {
                    crate::sd_session::upload_session(&mut epd, &mut sd_cs).await;
                }
                LoopArm::RefusedUpload => {
                    esp_println::println!("storage: upload refused outside sync");
                }
                LoopArm::Apply => {
                    // A layout change re-paginates the book, which blocks this
                    // task for the whole rebuild. Paint the title/author plate
                    // first so the wait reads as loading, not frozen: the store
                    // still reports the old settings here, so the reader view
                    // lands on the loading branch. A same-layout open already
                    // shows the plate through the normal render path (the book
                    // isn't loaded yet), so it is skipped here.
                    //
                    // Only for an open the handler will actually act on. The
                    // same begin() gate it applies drops a stale request here
                    // too -- otherwise a superseded open would spend a
                    // multi-second full flush painting a plate for a target the
                    // reader has already navigated past, then be skipped.
                    // A fenced-out open begins straight into its refusal, so
                    // this asks whether the sequence will actually stage a
                    // book rather than merely whether it begins: a plate for
                    // an open that is about to be refused would spend a
                    // multi-second flush on a book nobody opens.
                    let will_stage = OpenSequence::begin(
                        &command,
                        LATEST_READER_REQUEST_ID.load(Ordering::Relaxed),
                        sd_library.catalog_epoch(),
                    )
                    .is_some_and(|open| !matches!(open.next(), OpenAction::Refuse { .. }));
                    if refresh_planner.screen_on() && will_stage {
                        if let Some(loading_request) = open_loading_plate_request(
                            &command,
                            sd_library,
                            refresh_planner.last_request(),
                        ) {
                            crate::views::render(fb, loading_request, sd_library);
                            let mode = refresh_planner.mode_for(loading_request);
                            if let Ok(settle) = display_flush::flush(
                                &mut epd,
                                fb,
                                prev_fb,
                                refresh_planner.screen_on(),
                                mode,
                                prev_prestaged,
                            )
                            .await
                            {
                                // Held, not deferred like the render path's: no
                                // prestage follows a plate, so there is no
                                // nearer owner for the interval than here.
                                settle.wait().await;
                                refresh_planner.record_render(loading_request, mode);
                                prev_fb.copy_from(fb);
                                prev_prestaged = false;
                            } else {
                                // No Settled/Failed events here — the app isn't
                                // waiting on this opportunistic plate — but the
                                // panel state is as unknown as after any failed
                                // flush: drop the prestage claim and the
                                // planner's screen model.
                                esp_println::println!("display: loading plate flush failed");
                                prev_prestaged = false;
                                refresh_planner.record_failure();
                            }
                        }
                    }
                    handle_storage_command(
                        command,
                        &mut epd,
                        &mut sd_cs,
                        sd_library,
                        font_metrics,
                        &mut epub_scratch,
                        &mut sync_session,
                        &mut pending_progress,
                        &mut last_progress_write,
                        &mut state_restored,
                        &mut background_build,
                        last_portrait(&refresh_planner),
                    );
                }
            },
        }
    }
}

/// List the library root again after a scan, and tell the app.
///
/// A scan replaces the catalog, and the rows on screen came off the card it
/// was built from. Nobody pressed anything, so what goes out is unsolicited,
/// naming the catalog it was taken from. A move already in flight against
/// that same catalog keeps the screen, since it can still land; a move
/// against the catalog this scan replaced is overruled, because the reset has
/// already taken the storage task somewhere else and the move can only come
/// back refused.
///
/// A card that answered the scan and then would not answer for the rows is
/// reported as unreadable rather than as an empty library. The two look
/// identical in a row count and are not the same thing: one is a library to
/// add books to, the other is a library that could not be read, and the
/// screen says different things about them.
fn relist_library_folder(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    portrait: bool,
) {
    let started = Instant::now();
    let listed = crate::sd_session::with_root(epd, sd_cs, |root| {
        reader_cache::browse::relist_root(sd_library, root, portrait)
    })
    .ok()
    .flatten();
    // Part of what a boot costs, and the one listing nobody pressed for.
    bench_log!(
        "bench: folder_relist rows={} ok={} ms={} t_ms={}",
        listed.map_or(0, |listing| listing.count),
        listed.is_some(),
        started.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    let browse_epoch = sd_library.browse_epoch();
    match listed {
        Some(listing) => send_required_library_event(&LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch,
            depth: listing.depth,
            count: listing.count,
            books: listing.books,
            selection: listing.selection,
        }),
        None => {
            esp_println::println!("library: the card would not list the root after a scan");
            // The scan's own verdict stands for the catalog; this one is
            // about whether the library can be shown at all, and it cannot.
            sd_library.status = LibraryScanStatus::Error;
            send_required_library_event(&LibraryEvent::LibraryUnreadable { browse_epoch });
        }
    }
}

/// Places the settling event [`send_required_library_event`] could not, once
/// the channel has room. Pending forever when nothing is held, so it can sit
/// in the main loop's select as a branch that only fires when it has work.
///
/// The sender itself cannot wait: it runs inside `handle_storage_command`,
/// which is synchronous and has to stay that way — it owns the SD session and
/// multi-KB scratch near the stack floor. Nor may the *loop* simply wait here
/// and nowhere else. The app task blocks on `DISPLAY_COMMANDS.send` to hand
/// over a render, so a display task doing nothing but waiting for library-
/// event room would be waiting on a consumer waiting on it. Selecting this
/// against the display queue is what breaks that: servicing the render
/// releases the app task, which returns to its own select and drains.
async fn place_held_library_event() {
    let Some(event) = holder().pending() else {
        return core::future::pending::<()>().await;
    };
    LIBRARY_EVENTS.send(event).await;
    // Nothing awaits between the send completing and this, so the holder
    // cannot be observed empty with the event still unsent, or cleared for a
    // send that a cancellation abandoned.
    let _ = with_holder(LibraryEventHolder::placed);
}

/// The next storage command, but only while nothing is held.
///
/// Storage is where settling events come from and the holder has one slot, so
/// applying another command while one waits could produce a second with
/// nowhere to go. Pending forever until the holder clears, which the loop's
/// other branches are free to do meanwhile.
async fn storage_command_while_free() -> StorageCommand {
    if !holder().storage_may_run() {
        return core::future::pending::<StorageCommand>().await;
    }
    STORAGE_COMMANDS.receive().await
}

/// A book whose spine walk is still running after a progressive open published
/// it early.
///
/// This is not the build's state — that lives in the EPUB scratch, beside the
/// section records it describes, so the two cannot drift. This is only what the
/// loop needs: which book, and when the walk began, for the closing bench line.
#[derive(Clone, Copy)]
struct BackgroundBuild {
    book_id: u32,
    started: Instant,
    /// Consecutive steps that never began. Cleared by anything that proves the
    /// card is answering — a step that actually ran, or a foreground open that
    /// carried this walk through — so a hiccup does not go on slowing a build
    /// the card has already come back for.
    attempts: u8,
}

/// Carry the loop's background-build handle across one open or extend.
///
/// The distinction that matters is `Carried`: a page turn crossing a section
/// boundary arrives as an extend and is answered from the cache, which must
/// not be read as "the build ended". Only the reader-cache layer can tell the
/// difference — it knows whether the fast path answered — so this just follows
/// its verdict.
fn apply_build_outcome(
    background_build: &mut Option<BackgroundBuild>,
    outcome: book_build::BookBuildOutcome,
    book_id: u32,
) {
    match outcome {
        book_build::BookBuildOutcome::Settled => *background_build = None,
        book_build::BookBuildOutcome::Started => {
            *background_build = Some(BackgroundBuild {
                book_id,
                started: Instant::now(),
                attempts: 0,
            })
        }
        // The handle is normally already there, and what it needs is its retry
        // budget cleared: reaching here means a foreground open just took an SD
        // session, read this book's index and a section out of it, and came back
        // with the walk still valid. That is direct evidence the card is
        // answering again, so a walk sitting out a 30 s backoff should not wait
        // the rest of it — the reader can cross the frontier inside that window.
        //
        // Adopting a *missing* handle is the separate safety net, for anything
        // that drops it without ending the walk — a cache clear for another
        // book, say — so a still-valid build is picked back up rather than
        // stranded half-written. A handle naming another book is stale by the
        // same reasoning, since `Carried` proves the resume belongs to this one.
        book_build::BookBuildOutcome::Carried => match background_build {
            Some(pending) if pending.book_id == book_id => pending.attempts = 0,
            _ => {
                *background_build = Some(BackgroundBuild {
                    book_id,
                    started: Instant::now(),
                    attempts: 0,
                })
            }
        },
    }
}

/// Ready when the loop should spend a slice on a suspended book build.
///
/// The gate is the same one storage answers to, for the same reason: a step can
/// produce a settling `Loaded`, and the holder has one slot. It also stands
/// down for a sync session, whose loan takes the scratch the build is walking
/// out of.
///
/// And it stands down for the Chapters overview. That screen borrows the same
/// single text arena the build writes through, and says so with
/// `text_holds_toc`; a slice would quietly take the arena back, and the
/// overview only reloads its window while that flag is still set — so the
/// chapter list would go stale with nothing to restore it until the reader
/// left the screen. The walk simply waits, which costs nothing: leaving
/// Chapters reloads the reading section anyway.
///
/// The wait is what makes an otherwise always-ready branch safe to sit in a
/// `select`. Returning immediately would let this task run slice after slice
/// without handing the executor back — every other task starved for the length
/// of the whole build. A single yield was not enough either: measured on
/// device, one step ended and the next began 0.2 ms later, with a page turn
/// pressed 86 ms earlier still not reduced into a render, so the reader waited
/// out another 2912 ms step for a page that already existed. Waiting
/// `BACKGROUND_SETTLE_MS` gives the app room to produce that render, which the
/// first branch then services ahead of the next slice.
///
/// A walk that is only retrying waits on its backoff instead, which is what
/// lets a step that never began be kept indefinitely rather than given up on.
async fn background_build_step_due(pending: bool, attempts: u8) {
    if !pending {
        return core::future::pending::<()>().await;
    }
    // Always a wait, never a bare yield. The two reasons differ but the failure
    // of yielding is the same in both: this branch is ready again the instant a
    // step ends, so a single poll is not enough for the app task to get its
    // work in, and the loop commits to another multi-second slice ahead of it.
    let wait = if attempts > 0 {
        app_core::storage_loop::background_retry_delay_ms(attempts)
    } else {
        app_core::storage_loop::BACKGROUND_SETTLE_MS
    };
    Timer::after(Duration::from_millis(wait)).await;
}

/// Holds the display task still from the moment the panel goes down until the
/// power task says the sleep was abandoned.
///
/// This is the whole guarantee that a slept panel keeps showing its sleep
/// image. A render is routinely queued behind the `Sleep` — the pre-sleep
/// storage drain provokes one itself, since applying a book open emits
/// `Loaded` and the app repaints on it, and the sleep frame's full-waveform
/// flush gives it seconds to arrive. Returning to the command loop with that
/// render waiting re-initialises the panel and paints a page over the sleep
/// image, racing the power cut. Nothing the loop could do with that render is
/// right: painting it is the bug, and answering it discards the repaint the
/// abandoning press is owed. So the task does not go back to the loop at all.
///
/// On the ordinary path this never returns — `enter_deep_sleep_button` is `!`
/// and the chip reboots on wake. It returns only when the press that abandoned
/// the handshake releases it, and then the queued render is still queued and
/// repaints, which is exactly what that press asked for.
///
/// The wait is bounded so a lost `DisplayAsleep` cannot freeze the device.
/// That acknowledgement is a 20 ms bounded send into a queue the power task is
/// already draining, so losing it should not happen; if it ever does, the power
/// task waits for an ack that will not come and an unbounded park here would
/// wait on a resume that will not come either, leaving both tasks stopped
/// behind a dark panel. Waking early risks repainting over the sleep image —
/// far better than a device that has to be reset.
async fn park_until_resumed(generation: u32) {
    // Deep sleep follows its acknowledgement within about one input poll tick
    // (the power task's wake-button handoff), so seconds here are already many
    // orders of margin.
    const ABANDONED_HANDSHAKE_CEILING_SECS: u64 = 5;
    // `Timer::at` rather than a remaining-time subtraction: discarding a stale
    // resume can put the deadline in the past, and `Instant - Instant` panics
    // there. A deadline already passed simply fires at once.
    let deadline =
        Instant::now() + embassy_time::Duration::from_secs(ABANDONED_HANDSHAKE_CEILING_SECS);
    loop {
        match select(
            crate::DISPLAY_RESUME.wait(),
            embassy_time::Timer::at(deadline),
        )
        .await
        {
            Either::First(resumed) if resumed == generation => {
                esp_println::println!("display: sleep abandoned; resuming");
                return;
            }
            // A resume left over from a sleep this task never parked for, e.g.
            // one whose panel handshake failed. Not ours; keep waiting.
            Either::First(stale) => {
                esp_println::println!("display: ignoring stale resume generation={}", stale)
            }
            Either::Second(_) => {
                esp_println::println!(
                    "display: no deep sleep or resume within {} s; releasing the panel",
                    ABANDONED_HANDSHAKE_CEILING_SECS
                );
                return;
            }
        }
    }
}

pub(crate) fn send_library_event(event: &LibraryEvent) {
    // An event that settles something the app is holding cannot go out the
    // lossy way: the work is done and will not be redone, so a dropped one
    // strands the wait. The event says which it is, so no call site has to.
    if event.must_be_delivered() {
        send_required_library_event(event);
        return;
    }
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: library event queue full");
    }
}

fn render_custom_reader(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> bool {
    if request.view != AppView::Reading
        || !ReaderSource::from_book_id(request.book_id).is_sd()
        || request.font_family != display::font::FontFamily::Custom
        || display::font::builtin_custom_available()
        || !sd_library.custom_font_available()
    {
        return false;
    }
    crate::sd_session::with_root(epd, sd_cs, |root| {
        crate::views::render_custom_reader_from_root(fb, request, sd_library, font_metrics, root)
    })
    .unwrap_or(false)
}

/// The reader-view render to paint as a loading plate before an open/extend
/// that cannot be answered from the already loaded RAM section. The app sends
/// a normal Reading render around the same time, but the storage receiver can
/// win that race; painting here keeps a first cache build from looking frozen
/// on the previous screen.
fn open_loading_plate_request(
    command: &StorageCommand,
    sd_library: &ReaderStore,
    last_request: Option<RenderRequest>,
) -> Option<RenderRequest> {
    let (book_id, index, target_pages, type_settings, portrait) = match *command {
        StorageCommand::OpenBook {
            book_id,
            index,
            target_pages,
            type_settings,
            portrait,
            ..
        } => (book_id, index, target_pages, type_settings, portrait),
        StorageCommand::ExtendSection {
            book_id,
            index,
            target_pages,
            type_settings,
            portrait,
            ..
        } => (book_id, index, target_pages, type_settings, portrait),
        _ => return None,
    };
    // Only SD books re-paginate and route to the reader loading plate; the
    // built-in book renders from embedded content and never rebuilds.
    if !ReaderSource::from_book_id(book_id).is_sd() {
        return None;
    }
    if sd_library.type_settings() == type_settings
        && sd_library.portrait() == portrait
        && sd_library.covers_global_page(index as usize, target_pages as u32)
    {
        return None;
    }
    let mut request = last_request?;
    request.view = AppView::Reading;
    request.book_id = book_id;
    request.page = target_pages as u32;
    request.font_size = type_settings.size;
    request.line_spacing = type_settings.spacing;
    request.font_weight = type_settings.weight;
    request.font_family = type_settings.family;
    Some(request)
}

/// Kept out of line so the task loop's poll frame stays small; the storage
/// arms below carry multi-KB scratch and run near the stack floor.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn handle_storage_command(
    command: StorageCommand,
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    sync_session: &mut SyncSession,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    state_restored: &mut bool,
    background_build: &mut Option<BackgroundBuild>,
    portrait: bool,
) {
    // The session decides what may run: progress writes stay alive during a
    // sync session (they are cheap and harmless); everything
    // that touches the EPUB scratch is gone until the session's reset.
    if !sync_session.admits(&command) {
        esp_println::println!("storage: refused during sync session");
        return;
    }
    match command {
        StorageCommand::LoanSyncMemory => {
            // The background handle is deliberately *not* dropped here. A loan
            // that gets refused below returns with the scratch — and the walk's
            // section records in it — completely intact, and dropping the only
            // thing that schedules that walk would strand it: the loop's branch
            // is gated on the handle, and a reader already at the frontier
            // cannot issue the extend that would re-adopt it. It is cleared
            // once the scratch is actually gone.
            //
            // The session only ends in a reset, so any coalesced position
            // must reach the card before the scratch is dismantled.
            if !flush_pending_progress(
                epd,
                sd_cs,
                sd_library,
                pending_progress,
                last_progress_write,
            ) {
                // The wifi task is blocked on this answer; a silent return
                // would strand it (and the Wireless screen) forever. Refuse
                // observably so it can report the failure and re-park.
                esp_println::println!("storage: sync loan refused; progress persistence failed");
                let _ = crate::SYNC_LOANS.try_send(Err(app_core::SyncError::Storage));
                return;
            }
            ensure_epub_scratch(epub_scratch);
            let Some(scratch) = epub_scratch.take() else {
                let _ = crate::SYNC_LOANS.try_send(Err(app_core::SyncError::Storage));
                return;
            };
            // The scratch is out of the reader's hands now, taking the walk's
            // section records with it. Past this point the loan is granted and
            // the session ends in a reset, so there is nothing left to schedule.
            *background_build = None;
            sync_session.loan_granted();
            let mut loan = book_build::dismantle_scratch(scratch);
            let stored_wifi = book_build::load_wifi_credentials(epd, sd_cs);
            // The hint is matched against the credentials here rather than in
            // the wifi task, because this is the one place holding both
            // records — and a hint for another network must never steer this
            // join. A mismatch is not an error; it just means scan.
            loan.wifi_hint = stored_wifi.as_ref().and_then(|creds| {
                let ssid = &creds.ssid[..creds.ssid_len.min(32) as usize];
                book_build::load_wifi_ap_hint(epd, sd_cs)
                    .filter(|hint| hint.matches_ssid(ssid))
                    .map(|hint| app_core::WifiApHint {
                        bssid: hint.bssid,
                        channel: hint.channel,
                    })
            });
            loan.wifi = stored_wifi.map(|record| app_core::WifiCredentials {
                ssid: record.ssid,
                ssid_len: record.ssid_len,
                password: record.password,
                password_len: record.password_len,
            });
            loan.catalog_len = crate::library_sd::write_catalog_listing(epd, sd_cs, loan.http_b);
            if crate::SYNC_LOANS.try_send(Ok(loan)).is_err() {
                // Unreachable in practice: the wifi task blocks on each
                // answer before it can request again. The memory is gone
                // either way.
                esp_println::println!("storage: sync loan channel full");
            }
        }
        StorageCommand::LoadCatalogCache => {
            // Boot-time probe: name the saved network so the Wireless
            // screen can offer connect/forget honestly. The command runs
            // once per boot, before any session can start.
            if let Some(record) = book_build::load_wifi_credentials(epd, sd_cs) {
                let ssid = app_core::WifiSsid {
                    bytes: record.ssid,
                    len: record.ssid_len,
                };
                esp_println::println!("wifi: saved network '{}'", ssid.as_str());
                let _ = crate::SYNC_EVENTS.try_send(crate::SyncEvent::NetworkSaved(ssid));
            } else {
                esp_println::println!("wifi: no saved network");
            }
            book_build::load_custom_font_manifest(epd, sd_cs, sd_library);
            send_library_event(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            if crate::library_sd::load_catalog_cache(epd, sd_cs, sd_library) {
                // Restored goes out first so the very next Home repaint
                // already shows the saved book; the Scanned default then
                // sees an SD book active and leaves it alone.
                restore_saved_state(epd, sd_cs, sd_library, state_restored);
                let count = sd_library.catalog_count_u16();
                send_library_event(&LibraryEvent::Scanned {
                    count,
                    catalog_epoch: sd_library.catalog_epoch(),
                });
                relist_library_folder(epd, sd_cs, sd_library, portrait);
            } else {
                let _ = STORAGE_COMMANDS.try_send(StorageCommand::RefreshCatalog);
            }
        }
        StorageCommand::RefreshCatalog => {
            book_build::load_custom_font_manifest(epd, sd_cs, sd_library);
            send_library_event(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            crate::library_sd::scan_books(epd, sd_cs, sd_library);
            restore_saved_state(epd, sd_cs, sd_library, state_restored);
            send_library_event(&LibraryEvent::Scanned {
                count: sd_library.catalog_count_u16(),
                catalog_epoch: sd_library.catalog_epoch(),
            });
            relist_library_folder(epd, sd_cs, sd_library, portrait);
        }
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            ..
        }
        | StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            ..
        } => {
            let storage_start = Instant::now();
            let latest_request_id = LATEST_READER_REQUEST_ID.load(Ordering::Relaxed);
            // The transaction's order lives in `OpenSequence` so a host test can
            // drive it against a card model that fails whichever write it likes;
            // this arm supplies a real card and reports back what it did.
            let Some(mut open) =
                OpenSequence::begin(&command, latest_request_id, sd_library.catalog_epoch())
            else {
                esp_println::println!(
                    "storage: stale open skipped request={} latest={} book_id={} index={}",
                    request_id,
                    latest_request_id,
                    book_id,
                    index
                );
                return;
            };
            // `Some(ram_hit)` once a section load was reached; a transaction the
            // close-out refused never gets that far and must not report an open
            // that did not happen.
            let mut section_loaded = None;
            // Read at the saved-position step and spent at the section load,
            // which is where a place first has a pagination to resolve in.
            let mut pending_place: Option<book_build::SavedPlace> = None;
            loop {
                match open.next() {
                    OpenAction::CloseOutDeparting(previous) => {
                        let stored = close_out_departing_book(
                            epd,
                            sd_cs,
                            sd_library,
                            pending_progress,
                            last_progress_write,
                            previous,
                        );
                        if !stored {
                            esp_println::println!(
                                "storage: book open {:?} book_id={} departing={}",
                                book_open_outcome(false, false),
                                book_id,
                                previous.book_id,
                            );
                        }
                        open.departing_stored(stored);
                    }
                    OpenAction::Refuse { book_id } => {
                        // Nothing has been opened, so the reader is still whole
                        // on the book it was reading. Announcing the new one
                        // would strand that page: the app has already left the
                        // book that owns it and will never reissue it.
                        send_required_library_event(&LibraryEvent::BookOpenFailed { book_id });
                        open.refused();
                    }
                    OpenAction::StageBook {
                        index,
                        type_settings,
                        portrait,
                    } => {
                        // Read this book's catalog record into the active-entry
                        // slot so the reader pipeline (load_position,
                        // build_or_load) resolves it from the card rather than
                        // the list window. A failure leaves the entry unset and
                        // the open falls through to the usual bad-index error.
                        crate::library_sd::load_active_entry(
                            epd,
                            sd_cs,
                            sd_library,
                            index as usize,
                        );
                        // Adopt the command's type settings before the RAM fast
                        // path: a settings change drops the loaded page
                        // coverage, so the request falls through to the cache
                        // load/rebuild below.
                        sd_library.set_layout(type_settings, portrait);
                        open.staged();
                    }
                    OpenAction::LoadSavedPosition { index } => {
                        // The place is read here and resolved after the load:
                        // it names content, and which page that content falls
                        // on is decided by the pagination this open is about
                        // to build.
                        pending_place =
                            book_build::load_place(epd, sd_cs, sd_library, index as usize);
                        open.saved_position(pending_place.map(book_build::SavedPlace::provisional));
                        if open.resumed() {
                            esp_println::println!(
                                "storage: resume book {} at chapter {} screen {}",
                                book_id,
                                open.target_chapter(),
                                open.target_page()
                            );
                        }
                    }
                    OpenAction::LoadSection {
                        index,
                        chapter,
                        page,
                    } => {
                        // The requested page is usually inside the section
                        // window that is already loaded; answering from RAM
                        // keeps ordinary page turns free of card init, FAT, and
                        // cache-file traffic.
                        let ram_hit = sd_library.covers_global_page(index as usize, page as u32);
                        section_loaded = Some(ram_hit);
                        if ram_hit {
                            esp_println::println!(
                                "storage: open hit in RAM request={} book_id={} page={}",
                                request_id,
                                book_id,
                                page
                            );
                        } else {
                            esp_println::println!(
                                "storage: open command request={} book_id={} index={} chapter={} target={}",
                                request_id,
                                book_id,
                                index,
                                chapter,
                                page
                            );
                            sd_library.set_reader_status(BookLoadStatus::Loading);
                            let scratch = ensure_epub_scratch(epub_scratch);
                            // The transaction around this call is untouched by
                            // a progressive publish: it moves positions, and
                            // this book's position is real whether or not its
                            // tail is indexed yet.
                            let outcome = book_build::build_or_load_book_cache(
                                epd,
                                sd_cs,
                                sd_library,
                                index as usize,
                                chapter,
                                page as usize,
                                scratch,
                                font_metrics,
                            );
                            apply_build_outcome(background_build, outcome, book_id);
                        }
                        // The index now describes this book under this
                        // layout, which is the first moment a stored place
                        // can be turned into a page.
                        //
                        // A progressive build stops once it covers the page
                        // it was asked for, and the page a place names is not
                        // known until the pagination holding it exists, so
                        // the two take turns: resolve, build further, resolve
                        // again. Bounded, because a book whose index refuses
                        // to grow must not hold the open open.
                        if let Some(place) = pending_place.take() {
                            for _ in 0..PLACE_RESOLVE_STEPS {
                                let target = book_build::resolve_place(
                                    epd,
                                    sd_cs,
                                    sd_library,
                                    index as usize,
                                    place,
                                );
                                let (page, again) = match target {
                                    book_build::PlaceTarget::Keep => break,
                                    book_build::PlaceTarget::Page(page) => (page, false),
                                    book_build::PlaceTarget::Extend(page) => (page, true),
                                };
                                if !sd_library.covers_global_page(index as usize, page) {
                                    let scratch = ensure_epub_scratch(epub_scratch);
                                    let outcome = book_build::build_or_load_book_cache(
                                        epd,
                                        sd_cs,
                                        sd_library,
                                        index as usize,
                                        chapter,
                                        page as usize,
                                        scratch,
                                        font_metrics,
                                    );
                                    apply_build_outcome(background_build, outcome, book_id);
                                    section_loaded = Some(false);
                                }
                                if !again {
                                    open.resolve_place(page);
                                    break;
                                }
                                // Nothing new was built, so asking again
                                // would ask the same question forever.
                                if sd_library.advertised_page_count() <= page {
                                    break;
                                }
                            }
                        }
                        open.section_loaded();
                    }
                    OpenAction::StorePointer(state) => {
                        let record = record_for_persisted(sd_library, state);
                        let stored = book_build::store_global_state(epd, sd_cs, record);
                        if stored {
                            *pending_progress = None;
                            *last_progress_write = Some(Instant::now());
                        } else {
                            // Left owed rather than retried here: the book is
                            // open and the reader is in it, so the only cost of
                            // waiting for the next flush is a reboot in that
                            // window landing back on the old book.
                            *pending_progress = Some(record);
                        }
                        let outcome = book_open_outcome(true, stored);
                        debug_assert!(outcome.book_changed());
                        esp_println::println!(
                            "storage: book open {:?} book_id={} page={}",
                            outcome,
                            record.book_id,
                            record.screen,
                        );
                        bench_log!(
                            "bench: store_global_state ok={} book_id={} page={} t_ms={}",
                            stored,
                            record.book_id,
                            record.screen,
                            Instant::now().as_millis(),
                        );
                        open.pointer_stored(stored);
                    }
                    OpenAction::Announce { book_id, position } => {
                        // The one thing only this task knows: whether the load
                        // put different text under the reader. `None` means no
                        // section load was reached at all (a refused
                        // transaction), which is not a RAM hit; only a
                        // confirmed one read nothing from the card.
                        //
                        // Sent unconditionally. The app clamps navigation
                        // against the counts in here and clears its open lock
                        // on the event itself, so withholding it is never a
                        // saved refresh — it decides the repaint for itself in
                        // `loaded_repaints`.
                        let text_replaced = !matches!(section_loaded, Some(true));
                        send_loaded_library_event(&LibraryEvent::Loaded {
                            book_id,
                            pages: sd_library.advertised_page_count(),
                            chapters: sd_library.chapter_count_for_ui(),
                            current_chapter: sd_library.current_chapter(),
                            chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                            position,
                            text_replaced,
                        });
                        open.announced();
                    }
                    OpenAction::Done => break,
                }
            }
            if let Some(ram_hit) = section_loaded {
                if !ram_hit {
                    // Also the bench harness's legacy parse of a completed open.
                    esp_println::println!(
                        "storage: open complete status={:?} pages={} chapters={}",
                        sd_library.reader_status(),
                        sd_library.advertised_page_count(),
                        sd_library.chapter_count_for_ui()
                    );
                }
                bench_log!(
                    "bench: storage_open request={} book_id={} index={} ram_hit={} elapsed_ms={} status={:?} pages={} chapters={}",
                    request_id,
                    book_id,
                    index,
                    ram_hit,
                    storage_start.elapsed().as_millis(),
                    sd_library.reader_status(),
                    sd_library.advertised_page_count(),
                    sd_library.chapter_count_for_ui(),
                );
            }
        }
        StorageCommand::LoadChapters {
            request_id,
            book_id,
            index,
        } => {
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            // The overview opens with the cursor on the current chapter, so
            // center the first TOC window there.
            let ok = book_build::load_chapters_into_store(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                sd_library.current_chapter() as usize,
            );
            esp_println::println!(
                "storage: chapters loaded book_id={} ok={} count={}",
                book_id,
                ok,
                sd_library.overview_chapter_count()
            );
            // Re-render the overview with the full list resident, syncing the
            // selection range to the full chapter count. The reader has not
            // moved, so the app's own page stands.
            send_loaded_library_event(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                position: None,
                // The full list replaced the reading section in the buffer,
                // and the overview is holding its frame for this event.
                text_replaced: true,
            });
        }
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            type_settings,
            portrait,
        } => {
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            sd_library.set_layout(type_settings, portrait);
            // The TOC is still in the buffer; resolve the chapter's start page
            // before loading the section overwrites it. Re-ensure the window
            // covers the selection in case it slid since the overview render.
            book_build::ensure_toc_window(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter as usize,
                portrait,
            );
            let target_page = sd_library.overview_page_at(chapter as usize);
            let scratch = ensure_epub_scratch(epub_scratch);
            let outcome = book_build::build_or_load_book_cache(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter,
                target_page as usize,
                scratch,
                font_metrics,
            );
            apply_build_outcome(background_build, outcome, book_id);
            // The page came from the on-disk TOC, not from the app, so it
            // rides with the load rather than following as a second event.
            send_loaded_library_event(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: reader_cache::store::chapter_pages_for_event(sd_library),
                position: Some(target_page as u32),
                // A jump lands the reader on another chapter's text.
                text_replaced: true,
            });
        }
        StorageCommand::ReceiveUpload => {
            // Handled in the task loop before dispatch; reaching here means
            // the loop refused it already.
        }
        StorageCommand::StoreWifiCredentials(credentials) => {
            let record = proto::nvm::WifiCredentialsRecord {
                ssid: credentials.ssid,
                ssid_len: credentials.ssid_len,
                password: credentials.password,
                password_len: credentials.password_len,
            };
            let written = book_build::store_wifi_credentials(epd, sd_cs, record);
            // Reacquire the card and use the exact boot-time read path before
            // telling the portal it may show success. This proves the record
            // survived handle/volume closure, closing the race where the
            // portal's success page beat a write that never actually landed
            // and the session-ending reset lost the credentials.
            let confirmed = written
                && book_build::load_wifi_credentials(epd, sd_cs)
                    .is_some_and(|stored| stored == record);
            esp_println::println!(
                "storage: wifi credentials written={} confirmed={}",
                written,
                confirmed
            );
            let _ = crate::WIFI_STORAGE_RESULTS.try_send(confirmed);
        }
        StorageCommand::StoreWifiApHint { ssid, hint } => {
            let record = proto::nvm::WifiApHintRecord {
                ssid_hash: proto::nvm::WifiApHintRecord::hash_ssid(ssid.as_str().as_bytes()),
                bssid: hint.bssid,
                channel: hint.channel,
            };
            // No confirmation channel, unlike the credentials: nothing waits
            // on this and a lost hint costs one scan.
            let written = book_build::store_wifi_ap_hint(epd, sd_cs, record);
            esp_println::println!(
                "storage: wifi ap hint written={} channel={}",
                written,
                hint.channel
            );
        }
        StorageCommand::ForgetWifiCredentials => {
            let forgotten = book_build::forget_wifi_credentials(epd, sd_cs);
            esp_println::println!("storage: wifi credentials forgotten={}", forgotten);
        }
        StorageCommand::ClearBookCache {
            request_id,
            index,
            browse_epoch,
        } => {
            // Deleting a cache dir out from under a background build would
            // leave it writing an index for section files that no longer
            // exist. Both halves of the walk go, not just the handle: leaving
            // the resume in the scratch would let the next open of this row
            // report `Carried` and schedule steps over a cache that is gone.
            //
            // Ended unconditionally, even though the clear may name a different
            // book than the one building. A walk is only ever an optimisation —
            // the worst case is one redundant rebuild — and "the handle and the
            // resume die together" is an invariant worth more than that.
            *background_build = None;
            if let Some(scratch) = epub_scratch.as_mut() {
                book_build::clear_build_resume(scratch);
            }
            // The row was picked in a folder this task may since have left,
            // which would leave a different book sitting under it. Refuse
            // rather than guess: the user can pick again from the list they
            // can actually see.
            //
            // A Library row is a row of the folder listing, not a catalog
            // index, so it is resolved the way an open resolves one: by the
            // identity its locator, root, and size give, which is what the
            // catalog record was written under. Reading it as a catalog index
            // would clear whatever book happened to sit at that number.
            let ok = if browse_epoch == sd_library.browse_epoch() {
                match reader_cache::browse::row_book(sd_library, index) {
                    Some((at, locator, size)) => {
                        // By the place the row named, for the same reason the
                        // open does: the identity derived from it is 32 bits
                        // and can collide, and clearing nothing is the mild
                        // end of that.
                        match crate::library_sd::find_index_by_locator(
                            epd,
                            sd_cs,
                            at,
                            locator.as_str(),
                            size,
                        ) {
                            crate::library_sd::CatalogRow::Found(row) => {
                                book_build::clear_book_cache(epd, sd_cs, sd_library, row)
                            }
                            // Clearing a cache is not worth a rebuild, and a
                            // catalog that would not answer is not worth
                            // anything. Both report nothing cleared.
                            crate::library_sd::CatalogRow::Rebuild
                            | crate::library_sd::CatalogRow::Unreadable => false,
                        }
                    }
                    // A folder, or a row the resident page does not cover.
                    None => false,
                }
            } else {
                esp_println::println!(
                    "storage: clear cache index={} stale browse epoch={} now={}",
                    index,
                    browse_epoch,
                    sd_library.browse_epoch()
                );
                false
            };
            esp_println::println!(
                "storage: clear cache request={} index={} ok={}",
                request_id,
                index,
                ok
            );
            send_library_event(&LibraryEvent::CacheCleared { request_id, ok });
        }
        StorageCommand::ChooseLibraryRow {
            request_id,
            index,
            browse_epoch,
        } => {
            // The row was picked in a folder this task may since have left: a
            // scan takes browsing back to the root, and the same row number
            // there names a different child of a different place. Refuse
            // rather than guess, and the reader picks again from the list
            // they can see.
            let choice = if browse_epoch == sd_library.browse_epoch() {
                crate::library_sd::choose_library_row(epd, sd_cs, sd_library, index, portrait)
            } else {
                esp_println::println!(
                    "storage: choose row={} stale browse epoch={} now={}",
                    index,
                    browse_epoch,
                    sd_library.browse_epoch()
                );
                crate::library_sd::RowChoice::Failed
            };
            match choice {
                crate::library_sd::RowChoice::Entered(listing) => {
                    send_required_library_event(&LibraryEvent::FolderListed {
                        request_id: Some(request_id),
                        browse_epoch: sd_library.browse_epoch(),
                        depth: listing.depth,
                        count: listing.count,
                        books: listing.books,
                        selection: listing.selection,
                    });
                }
                crate::library_sd::RowChoice::Book(index) => {
                    // Answer and stop. The app owns opening: it commits the
                    // reader request id a later open is checked against, arms
                    // the gate that keeps input off the panel until the book
                    // lands, and keeps the rollback that puts the reader back
                    // if the command is refused. An open sent from here would
                    // have none of that, and would carry an id from the browse
                    // counter that the staleness check reads as old.
                    send_required_library_event(&LibraryEvent::RowIsBook {
                        request_id,
                        index,
                        catalog_epoch: sd_library.catalog_epoch(),
                    });
                }
                crate::library_sd::RowChoice::Failed => {
                    send_required_library_event(&LibraryEvent::RowFailed { request_id });
                }
                crate::library_sd::RowChoice::Stale => {
                    // A book the card holds and the catalog does not, because
                    // boot keeps a snapshot that still loads and a computer
                    // can add or move books while the device is off. Rebuild
                    // rather than tell a reader that a book they can see
                    // cannot be opened. Only a card edited since the last
                    // scan pays for this, once, which is what keeps every
                    // other boot on the warm snapshot.
                    crate::library_sd::scan_books(epd, sd_cs, sd_library);
                    restore_saved_state(epd, sd_cs, sd_library, state_restored);
                    send_library_event(&LibraryEvent::Scanned {
                        count: sd_library.catalog_count_u16(),
                        catalog_epoch: sd_library.catalog_epoch(),
                    });
                    // The scan takes browsing back to the root, so this
                    // request's row number no longer names the same child.
                    // Relist and let the reader pick from what they see; the
                    // book is in the catalog now, so the next press opens it.
                    relist_library_folder(epd, sd_cs, sd_library, portrait);
                    send_required_library_event(&LibraryEvent::RowFailed { request_id });
                }
            }
        }
        StorageCommand::LeaveLibraryFolder {
            request_id,
            browse_epoch,
        } => {
            let listed = if browse_epoch == sd_library.browse_epoch() {
                crate::library_sd::leave_library_folder(epd, sd_cs, sd_library, portrait)
            } else {
                esp_println::println!(
                    "storage: leave folder stale browse epoch={} now={}",
                    browse_epoch,
                    sd_library.browse_epoch()
                );
                None
            };
            match listed {
                Some(listing) => send_required_library_event(&LibraryEvent::FolderListed {
                    request_id: Some(request_id),
                    browse_epoch: sd_library.browse_epoch(),
                    depth: listing.depth,
                    count: listing.count,
                    books: listing.books,
                    selection: listing.selection,
                }),
                None => send_required_library_event(&LibraryEvent::RowFailed { request_id }),
            }
        }
        StorageCommand::StoreProgress(record) => {
            let record = record_for_persisted(sd_library, record);
            // Coalesce same-context page turns; anything beyond the screen
            // number changing (book, chapter, orientation, policy) is rare
            // and worth landing immediately. A pending record for the same
            // book is superseded by the new one; only a different book's
            // pending position must be preserved first.
            let context_changed = pending_progress
                .map(|pending| {
                    AppStateRecord {
                        screen: record.screen,
                        ..pending
                    } != record
                })
                .unwrap_or(false);
            let due = last_progress_write
                .map(|written| written.elapsed().as_secs() >= PROGRESS_WRITE_MIN_SECS)
                .unwrap_or(true);
            if pending_progress
                .map(|pending| pending.book_id != record.book_id)
                .unwrap_or(false)
                && !flush_pending_progress(
                    epd,
                    sd_cs,
                    sd_library,
                    pending_progress,
                    last_progress_write,
                )
            {
                // The other book's position couldn't land; overwriting the
                // pending record now would silently discard it.
                esp_println::println!(
                    "storage: progress context switch deferred after write failure"
                );
                return;
            }
            if context_changed || due {
                let progress_start = Instant::now();
                let stored = book_build::store_app_state(epd, sd_cs, sd_library, record);
                if stored {
                    *pending_progress = None;
                    *last_progress_write = Some(Instant::now());
                } else {
                    *pending_progress = Some(record);
                }
                bench_log!(
                    "bench: storage_progress action=write ok={} book_id={} page={} elapsed_ms={} t_ms={}",
                    stored,
                    record.book_id,
                    record.screen,
                    progress_start.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
            } else {
                *pending_progress = Some(record);
                bench_log!(
                    "bench: storage_progress action=coalesce book_id={} page={} t_ms={}",
                    record.book_id,
                    record.screen,
                    Instant::now().as_millis(),
                );
            }
        }
    }
}

/// Queue an event the app is waiting on, making room if the channel is full.
///
/// It used to make room by taking whatever was at the front and throwing it
/// away, without looking at it — so delivering one settling event could
/// silently destroy another, which is the whole thing this function exists to
/// prevent. It walks the ring now; [`EvictionWalk`] owns which events may be
/// spent for the newcomer and is host-tested against a modelled channel.
fn send_required_library_event(event: &LibraryEvent) {
    if LIBRARY_EVENTS.try_send(*event).is_ok() {
        return;
    }
    let mut walk = EvictionWalk::new(app_core::LIBRARY_EVENT_SLOTS);
    while !walk.exhausted() {
        let Ok(head) = LIBRARY_EVENTS.try_receive() else {
            break;
        };
        match walk.inspect(&head) {
            EvictionStep::Discard => {
                // The refresh is spent; its slot is this event's. Nothing
                // else writes this channel, so the send cannot lose the race.
                if LIBRARY_EVENTS.try_send(*event).is_ok() {
                    return;
                }
                break;
            }
            EvictionStep::Requeue => {
                // One slot is free (the head came off), so this always lands.
                let _ = LIBRARY_EVENTS.try_send(head);
            }
        }
    }
    // Every slot holds something the app is waiting on, so nothing here may
    // be spent — but this event is awaited too, and dropping it would leave
    // its wait unanswerable. Hold it for `deliver_held_library_event`, which
    // retries once the app task has had a turn to drain.
    hold_library_event(event);
}

/// The settling event with nowhere to go, and the gates it closes while it
/// waits. [`LibraryEventHolder`] owns both, and is host-tested; this task
/// asks it rather than re-deciding at each of the four sites that must agree.
static HELD_LIBRARY_EVENT: Mutex<CriticalSectionRawMutex, Cell<LibraryEventHolder>> =
    Mutex::new(Cell::new(LibraryEventHolder::new()));

/// Reads the holder. `Copy`, so this is a snapshot — fine for the gates,
/// which only ever narrow: nothing but this task fills the holder, and the
/// one thing that empties it is this task's own placing branch.
fn holder() -> LibraryEventHolder {
    HELD_LIBRARY_EVENT.lock(Cell::get)
}

fn with_holder<T>(update: impl FnOnce(&mut LibraryEventHolder) -> T) -> T {
    HELD_LIBRARY_EVENT.lock(|cell| {
        let mut holder = cell.get();
        let outcome = update(&mut holder);
        cell.set(holder);
        outcome
    })
}

/// Keeps `event` until the channel has room, or says why it could not.
///
/// Both refusals end the same way — one last try for a slot that may have
/// freed since, and the drop reported if it has not. They are reported apart
/// because they mean different things about the code above: `NotSettling` is
/// a caller that routed a refresh here, `Occupied` is two settling events out
/// of one storage command.
fn hold_library_event(event: &LibraryEvent) {
    let outcome = with_holder(|holder| holder.hold(event));
    let refusal = match outcome {
        HoldOutcome::Held => return,
        HoldOutcome::NotSettling => "refresh routed to the holder",
        HoldOutcome::Occupied => "holder occupied",
    };
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: {}, dropped {:?}", refusal, event);
    }
}

/// Sends a library event down the display channel, falling back to its own
/// when that one is full.
///
/// The two channels reach the app independently, so this is a choice of route
/// and not of order — anything that must be ordered against a render
/// acknowledgement travels *inside* it (see `DisplayEvent::Settled`) rather
/// than relying on which queue it landed in. Only `Loaded` comes through here
/// now, and it is order-free: whichever way round it and the acknowledgement
/// arrive, the app folds it and renders.
///
/// The fallback routes by `must_be_delivered` like every other send. It used
/// to go straight to the required path, which let a refresh-only event take
/// the holder — and the holder being occupied is what stops the very next
/// `Settled` from making room for itself, so a droppable event could strand
/// the app's render lock.
fn send_loaded_library_event(event: &LibraryEvent) {
    if DISPLAY_EVENTS
        .try_send(DisplayEvent::Library(*event))
        .is_ok()
    {
        return;
    }
    send_library_event(event);
}

/// Power acknowledgements get a bounded-wait send instead of a silent
/// try_send drop: the power task's sleep handshake blocks on the matching
/// `DisplayAsleep`/`DisplaySleepFailed`, and losing one on a momentarily
/// full queue would leave the MCU awake behind a dark panel until the next
/// input. The wait must stay bounded rather than fully blocking — the power
/// task stops draining `POWER_EVENTS` while it is itself blocked sending a
/// Sleep command into a full `DISPLAY_COMMANDS` queue, which only this task
/// drains, so an unbounded send here could deadlock both tasks. In that
/// window the acks being sent are refresh acks the power task ignores, so
/// timing out and logging the drop is safe; sleep acks are only sent after
/// the power task's Sleep send completed, when it is back in its receive
/// loop and drains the queue within the bound.
async fn send_required_power_event(event: PowerEvent) {
    if embassy_time::with_timeout(
        embassy_time::Duration::from_millis(20),
        POWER_EVENTS.send(event),
    )
    .await
    .is_err()
    {
        esp_println::println!("display: power event queue full, dropped {:?}", event);
    }
}

/// Sends a display event, by the rule the event itself carries.
///
/// The two sleep notifications take the lossy path. The handshake the power
/// task waits on goes over `POWER_EVENTS` beside each of them and the app only
/// logs these, so a dropped one costs the log line — and letting them compete
/// for room with an acknowledgement got the priority exactly backwards, since
/// an acknowledgement is what ends the app's render cycle.
fn send_display_event(event: &DisplayEvent) {
    if event.must_be_delivered() {
        send_required_display_event(event);
        return;
    }
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: display event queue full, dropped {:?}", event);
    }
}

/// Queues an acknowledgement the app is waiting on, holding it if the channel
/// is full.
///
/// This used to make room by walking the queue. That was wrong twice over: the
/// first version's `try_receive` dropped whatever its pattern did not match,
/// and the walk that replaced it had to requeue at the tail, reordering the
/// queue the app reads its acknowledgements from. The queue is left alone now
/// — [`DisplayEventHolder`] explains why nothing in it is worth spending — and
/// the acknowledgement waits for room instead.
fn send_required_display_event(event: &DisplayEvent) {
    if DISPLAY_EVENTS.try_send(*event).is_ok() {
        return;
    }
    let outcome = with_display_holder(|holder| holder.hold(event));
    let refusal = match outcome {
        DisplayHoldOutcome::Held => return,
        DisplayHoldOutcome::NotRequired => "refresh routed to the acknowledgement holder",
        // Both end the render cycle and the app clears its lock on either, so
        // the one already waiting answers for this one too.
        DisplayHoldOutcome::Occupied => "acknowledgement holder occupied",
    };
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: {}, dropped {:?}", refusal, event);
    }
}

/// The acknowledgement that had nowhere to go, waiting for room. Gates
/// nothing: see [`DisplayEventHolder`] for why refusing renders while one is
/// held would deadlock the very task that empties it.
static HELD_DISPLAY_EVENT: Mutex<CriticalSectionRawMutex, Cell<DisplayEventHolder>> =
    Mutex::new(Cell::new(DisplayEventHolder::new()));

fn display_holder() -> DisplayEventHolder {
    HELD_DISPLAY_EVENT.lock(Cell::get)
}

fn with_display_holder<T>(update: impl FnOnce(&mut DisplayEventHolder) -> T) -> T {
    HELD_DISPLAY_EVENT.lock(|cell| {
        let mut holder = cell.get();
        let outcome = update(&mut holder);
        cell.set(holder);
        outcome
    })
}

/// Places the acknowledgement [`send_required_display_event`] could not, once
/// the channel has room. Pending forever when nothing is held, so it can sit
/// in the main loop's select as a branch that only fires when it has work.
///
/// Selecting this against the display queue is what keeps it from deadlocking:
/// the app blocks on `DISPLAY_COMMANDS.send` to hand over a render, and
/// servicing that render is what releases it to drain this event's channel.
async fn place_held_display_event() {
    let Some(event) = display_holder().pending() else {
        return core::future::pending::<()>().await;
    };
    DISPLAY_EVENTS.send(event).await;
    // Nothing awaits between the send completing and this, so the holder
    // cannot be observed empty with the event still unsent, or cleared for a
    // send that a cancellation abandoned.
    let _ = with_display_holder(DisplayEventHolder::placed);
}

/// Step one of a book-open transaction: get the departing book's page onto
/// the card, and clear anything the coalescer was still holding for it.
///
/// Returns whether the open may proceed. A refusal leaves the reader entirely
/// on the old book, with that book's position still owed and retried by the
/// next flush — there is no half-applied switch to reconcile later, which is
/// what lets the pending state stay a single latest-value slot.
fn close_out_departing_book(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    previous: PersistedAppState,
) -> bool {
    // A coalesced record for another book still has to land: it names that
    // book in the global state file, and this transaction is about to point
    // that file somewhere else.
    if pending_progress.is_some_and(|pending| pending.book_id != previous.book_id)
        && !flush_pending_progress(
            epd,
            sd_cs,
            sd_library,
            pending_progress,
            last_progress_write,
        )
    {
        return false;
    }
    let record = record_for_persisted(sd_library, previous);
    let start = Instant::now();
    let stored = book_build::store_book_position(epd, sd_cs, sd_library, record);
    bench_log!(
        "bench: store_book_position ok={} book_id={} page={} elapsed_ms={} t_ms={}",
        stored,
        record.book_id,
        record.screen,
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    if stored {
        // Whatever the coalescer held for this book is now on the card, and
        // the global half of it is about to be rewritten by step three.
        *pending_progress = None;
    } else {
        // Keep it owed so the next flush retries it; the reader is staying on
        // this book, so the record is still the right one to write.
        *pending_progress = Some(record);
    }
    stored
}

/// Kept out of line: first-call initialization constructs `DecompressorOxide`
/// by value into a static; that temporary stack frame must not sit at the base
/// of the EPUB open call chain.
///
/// While `EPUB_ZIP_INFLATE`'s 32 KiB window buffer is const-initialized in `.bss`,
/// `DecompressorOxide::new()` still produces a measured 10,512-byte temporary
/// frame when initializing the decoder. `#[inline(never)]` keeps this ~10.5 KiB
/// allocation transient on a shallow frame rather than resident under the deeper
/// EPUB open call stack, leaving the 13,840-byte EPUB cache builder as the largest
/// frame in the binary. `tools/check.sh stack-frames` is the guard on it.
#[inline(never)]
fn ensure_epub_scratch<'a>(
    epub_scratch: &'a mut Option<&'static mut ReaderCacheScratch<'static>>,
) -> &'a mut ReaderCacheScratch<'static> {
    if epub_scratch.is_none() {
        esp_println::println!("storage: init epub scratch");
        let zip_ref = EPUB_ZIP_INFLATE.take();
        // Build the decoder here rather than letting the first decode do it:
        // this frame is shallow, the EPUB open chain's is not.
        zip_ref.prepare(EPUB_DECOMPRESSOR.init(proto::epub::DecompressorOxide::new()));
        *epub_scratch = Some(EPUB_SCRATCH.init(ReaderCacheScratch::new(
            EPUB_TAIL.take(),
            EPUB_HEADER.take(),
            EPUB_NAME.take(),
            EPUB_COMPRESSED.take(),
            EPUB_CONTAINER.take(),
            EPUB_OPF.take(),
            EPUB_XHTML.take(),
            EPUB_BOOK_SECTIONS.take(),
            zip_ref,
        )));
    }
    epub_scratch.as_deref_mut().unwrap()
}

fn source_identity(library: &ReaderStore, book_id: u32) -> (u32, u32) {
    library.source_identity(book_id)
}

/// The on-card record for a state the app persisted, with the fields only the
/// firmware knows filled in.
///
/// The reducer derives chapter from the 128-capped `sd_chapter_for_page`, so a
/// deep position would save a stuck chapter that the sleep/boot colophon then
/// shows wrong until the book reopens. The firmware tracks the true chapter
/// over the whole book; adopt it for the loaded SD book so saved and restored
/// state name the chapter right.
/// The orientation the last render was asked for, which is the one a
/// storage command should be answered in. True before any request, matching
/// the boot default.
fn last_portrait(planner: &RefreshPlanner) -> bool {
    planner
        .last_request()
        .map(|last| app_core::is_portrait(last.orientation))
        .unwrap_or(true)
}

fn record_for_persisted(library: &ReaderStore, state: PersistedAppState) -> AppStateRecord {
    let (source_hash, source_size) = source_identity(library, state.book_id);
    let chapter = if ReaderSource::from_book_id(state.book_id).is_sd()
        && library.loaded_index == ReaderStore::selected_book_index(state.book_id)
    {
        library.current_chapter()
    } else {
        state.chapter
    };
    AppStateRecord {
        book_id: state.book_id,
        chapter,
        screen: state.screen,
        shell_orientation: state.shell_orientation,
        reading_orientation: state.reading_orientation,
        refresh_policy: state.refresh_policy,
        font_size: state.font_size,
        line_spacing: state.line_spacing,
        font_weight: state.font_weight,
        font_family: state.font_family,
        front_buttons: state.front_buttons,
        source_hash,
        source_size,
        // Freshly derived from the active entry, so always the current
        // interpretation; the flag exists for records read back from the
        // card.
        legacy_source_identity: false,
    }
}

/// Where the reader is in the book at `index`.
///
/// The book's own position file is authoritative. It is written when that book
/// is left and read when it is opened, so it can only ever describe this book —
/// which is the whole point of keeping it: the global state record is a single
/// slot that names one book, and reading position out of it is what let a stale
/// record hand one book's page to another.
///
/// The record's own `chapter`/`screen` are a mirror, still written so MarigoldOS
/// (which reads position from the global file) keeps resuming from cards this
/// firmware wrote. They are consulted only when the per-book file is missing or
/// fails its checksum, and they are safe in that role because the identity that
/// selected this book came from the very same record.
fn book_position(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    index: u16,
    mirror: AppStateRecord,
) -> (u16, u32) {
    // The boot mirror only understands a page, so a place resolves to the
    // chapter it names and page zero inside it. The open that follows refines
    // it against the pagination it builds, the same way an ordinary open does.
    match book_build::load_place(epd, sd_cs, library, usize::from(index))
        .map(book_build::SavedPlace::provisional)
    {
        Some(position) => position,
        None => {
            esp_println::println!(
                "restore: no per-book position for index={}; using the global mirror",
                index
            );
            (mirror.chapter, mirror.screen)
        }
    }
}

/// One boot-time attempt to map durable reader state back onto the scanned
/// catalog by stable source identity (path hash + byte size) and hand the
/// saved position to the app as a `Restored` event. The volatile book id
/// stored in the record is never trusted directly.
fn restore_saved_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    state_restored: &mut bool,
) {
    if *state_restored || library.catalog_is_empty() {
        return;
    }
    *state_restored = true;
    let Some(record) = book_build::load_app_state(epd, sd_cs) else {
        esp_println::println!("restore: no usable durable state");
        return;
    };
    let Some(index) = crate::library_sd::find_index_by_identity(
        epd,
        sd_cs,
        record.source_hash,
        record.source_size,
        record.legacy_source_identity,
    ) else {
        esp_println::println!(
            "restore: no catalog match hash={:08x} size={}",
            record.source_hash,
            record.source_size
        );
        return;
    };
    // Stage the restored book's catalog entry so the position, colophon, and
    // page-count reads below resolve it, and so the first Home paint names it
    // before any open.
    crate::library_sd::load_active_entry(epd, sd_cs, library, usize::from(index));
    let (chapter, screen) = book_position(epd, sd_cs, library, index, record);
    esp_println::println!(
        "restore: index={} chapter={} screen={}",
        index,
        chapter,
        screen
    );
    // Resolve the chapter title now so wake-to-Home (rendered before the book
    // is opened) names the chapter; without this the colophon shows a numeral
    // until the book is first opened this session.
    book_build::load_chapter_title(epd, sd_cs, usize::from(index), chapter, library);
    // The book's total page count, so the Home progress bar has a denominator
    // on wake before the book is opened (read from the cache index header).
    let page_count = book_build::restore_book_page_count(epd, sd_cs, usize::from(index), library);
    send_required_library_event(&LibraryEvent::Restored {
        book_id: ReaderSource::sd(index).book_id(),
        chapter,
        page: screen,
        page_count,
        reading_orientation: record.reading_orientation,
        refresh_policy: record.refresh_policy,
        font_size: record.font_size,
        line_spacing: record.line_spacing,
        font_weight: record.font_weight,
        font_family: record.font_family,
        front_buttons: record.front_buttons,
    });
}

fn sleep_request_from_saved_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    pending_progress: &Option<AppStateRecord>,
) -> Option<RenderRequest> {
    // A coalesced record is state the card has not seen yet, so it outranks
    // both stored copies; only a record read back from the card has to defer
    // to the book's own position file.
    let (record, unflushed) = match *pending_progress {
        Some(record) => (record, true),
        None => (book_build::load_app_state(epd, sd_cs)?, false),
    };
    let index = crate::library_sd::find_index_by_identity(
        epd,
        sd_cs,
        record.source_hash,
        record.source_size,
        record.legacy_source_identity,
    )?;
    crate::library_sd::load_active_entry(epd, sd_cs, library, usize::from(index));
    let (chapter, screen) = if unflushed {
        (record.chapter, record.screen)
    } else {
        book_position(epd, sd_cs, library, index, record)
    };
    book_build::load_chapter_title(epd, sd_cs, usize::from(index), chapter, library);
    let page_count = book_build::restore_book_page_count(epd, sd_cs, usize::from(index), library);
    Some(RenderRequest {
        kind: RenderKind::Page,
        // The sleep frame is not queued and answers no press.
        requested_at_ms: 0,
        view: AppView::Home,
        page: screen,
        page_count,
        chapter,
        selection: 0,
        book_id: ReaderSource::sd(index).book_id(),
        orientation: display_orientation_from_u8(record.reading_orientation)
            .unwrap_or(DisplayOrientation::PortraitButtonsLeft),
        front_buttons: app_core::front_buttons_from_u8(record.front_buttons)
            .unwrap_or(app_core::FrontButtons::PagesRight),
        reading_sheet: false,
        library_menu: app_core::LibraryMenu::None,
        library_move_pending: false,
        refresh_policy: refresh_policy_from_u8(record.refresh_policy)
            .unwrap_or(app_core::RefreshPolicy::FullOnWake),
        font_size: display::font::FontSize::from_u8(record.font_size)
            .unwrap_or(display::font::FontSize::Medium),
        line_spacing: display::font::LineSpacing::from_u8(record.line_spacing)
            .unwrap_or(display::font::LineSpacing::Normal),
        font_weight: display::font::FontWeight::from_u8(record.font_weight)
            .unwrap_or(display::font::FontWeight::Normal),
        font_family: display::font::FontFamily::from_u8(record.font_family)
            .unwrap_or(display::font::FontFamily::Literata),
        last_button: None,
        aux_raw: 0,
        nav_raw: 0,
        page_raw: 0,
        battery_mv: 0,
        battery_percent: 100,
        library_count: library.catalog_count_u16(),
        sync_status: SyncStatus::NotConfigured,
        wifi_ssid: [0; 32],
        wifi_ssid_len: 0,
        dirty: display::Rect::FULL,
    })
}

fn flush_pending_progress(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
) -> bool {
    if let Some(record) = *pending_progress {
        let start = Instant::now();
        let stored = book_build::store_app_state(epd, sd_cs, sd_library, record);
        if stored {
            *pending_progress = None;
            *last_progress_write = Some(Instant::now());
        }
        bench_log!(
            "bench: storage_progress action=flush ok={} book_id={} page={} elapsed_ms={} t_ms={}",
            stored,
            record.book_id,
            record.screen,
            start.elapsed().as_millis(),
            Instant::now().as_millis(),
        );
        stored
    } else {
        true
    }
}
