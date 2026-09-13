use crate::{
    catalog, Button, DisplayCommand, DisplayEvent, InputEvent, PowerEvent, ReaderSource,
    RenderKind, StorageCommand, SyncCommand, DISPLAY_COMMANDS, DISPLAY_EVENTS, INPUT_EVENTS,
    LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS, SYNC_COMMANDS,
    SYNC_EVENTS,
};
use app_core::{
    extend_section_command, library_action_command_for_transition,
    library_browse_command_for_transition, storage_command_for_transition, AppView,
    BookOpenRollback, ParkedStorage, ReaderState, ReducerContext, RepaintRetry, SleepBlockers,
    SleepGate, StorageDispatch, SyncStatus,
};
use core::sync::atomic::Ordering;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_time::{Duration, Instant};

const POST_OPEN_CONFIRM_BLOCK_MS: u64 = 700;

#[embassy_executor::task]
pub async fn run() {
    esp_println::println!("app: started");
    let ctx = reducer_context();
    let mut state = ReaderState::boot();
    // Compile-time dev credentials name the network immediately; the
    // display task's boot probe of /READER/WIFI.BIN arrives later and
    // overrides, matching the wifi task's stored-beats-built-in order.
    if let Some((ssid, _)) = crate::tasks::wifi::credentials() {
        if let Some(ssid) = app_core::WifiSsid::new(ssid) {
            state = state.apply_sync_event(app_core::SyncEvent::NetworkSaved(ssid));
        }
    }
    let mut rendering = false;
    let mut render_pending = false;
    let mut catalog_refresh_requested = true;
    let mut pending_storage = ParkedStorage::new();
    // Type settings changed while away from Reading: the loaded section is
    // paginated under the old layout, so the next entry into Reading must
    // send an extend even though page and chapter are unchanged.
    let mut reader_relayout_pending = false;
    let mut opening_book: Option<u32> = None;
    // Where to put the reader back if the inflight open is refused rather
    // than answered. Set for a book change, which leaves the reader between
    // two books, and for a catalog-fenced row open, which storage refuses
    // outright when that catalog has been replaced. See `app_core::open_hold`.
    let mut open_rollback: Option<BookOpenRollback> = None;
    // A Power press arriving while the app still owes the storage task work.
    let mut sleep_gate = SleepGate::new();
    // The one repaint a failed panel transition is owed, so the reader is not
    // left looking at the page before the one the app has already moved to.
    let mut repaint_retry = RepaintRetry::new();
    let mut suppress_input_until_open_settled = false;
    let mut block_confirm_until: Option<Instant> = None;
    // Defer the first paint. ReaderState::boot() defaults to the built-in guide
    // (book_id 1), so drawing it now flashes "About This Reader" until the saved
    // book loads from SD ~1.5s later. Instead, kick the catalog + saved-position
    // restore now and leave the retained image (the sleep screen, on a deep-sleep
    // wake) up until it resolves; the first render is sent when Restored/Scanned
    // lands (see first_render_kind), so wake is a single refresh straight onto the
    // restored book. Now that sleep is terminal, wake is a full reboot — before
    // that this cold-boot path never showed, the real book stayed resident.
    let mut boot_render_pending = true;
    if STORAGE_COMMANDS
        .try_send(StorageCommand::LoadCatalogCache)
        .is_err()
    {
        esp_println::println!("app: storage queue full for catalog cache");
    }

    loop {
        // The parked queue rides alongside the receivers rather than being
        // drained only where a display cycle ends: a refused offer used to
        // wait for the next Settled, and nothing promises another one -- the
        // commands that freed the channel need not produce any app event. A
        // parked open would hold `opening_book` and swallow every press until
        // the reader was rebooted. Racing the send here retries the moment
        // capacity opens, and staying inside the select is what keeps the
        // earlier deadlock closed: the library receiver is still live, so the
        // display task's held event can always land.
        let received = select(
            select4(
                INPUT_EVENTS.receive(),
                DISPLAY_EVENTS.receive(),
                LIBRARY_EVENTS.receive(),
                SYNC_EVENTS.receive(),
            ),
            offer_parked_storage(&pending_storage),
        )
        .await;
        let event = match received {
            Either::First(event) => event,
            Either::Second(command) => {
                accept_parked_storage(
                    &mut pending_storage,
                    &mut opening_book,
                    &mut suppress_input_until_open_settled,
                    command,
                );
                continue;
            }
        };
        match event {
            Either4::First(event) => {
                if matches!(event, InputEvent::Sample { button: None, .. }) {
                    // A button-less sample is a pure battery reading (the input
                    // task emits one at boot, before the first paint). Fold the
                    // charge into state but spend no panel refresh on it -- the
                    // value rides out on the next real paint. At boot that's the
                    // deferred Restored paint (see boot_render_pending), so the
                    // first screen shows the true charge instead of boot()'s
                    // 100% placeholder.
                    state = state.apply_input(ctx, event);
                    continue;
                }
                if matches!(
                    event,
                    InputEvent::Sample {
                        button: Some(Button::Power),
                        ..
                    }
                ) {
                    // Hand off to the power task, which drives the display to its
                    // sleep image and then deep-sleeps the SoC with the Power
                    // button armed as the wake source. Waking is a fresh boot
                    // (deep sleep is terminal), so there is no in-app "asleep"
                    // state to toggle back out of here.
                    //
                    // Unless the app is still holding storage work. Sleep would
                    // reach the display task down its own channel and be picked
                    // up ahead of anything queued behind it, and the pre-sleep
                    // flush there cannot see a command parked in this task -- so
                    // a book open in either position would go down with the
                    // reader's place in the book it was leaving.
                    if sleep_gate.press(sleep_blockers(
                        opening_book,
                        &pending_storage,
                        suppress_input_until_open_settled,
                    )) {
                        esp_println::println!("app: sleep requested");
                        let _ = POWER_EVENTS.send(PowerEvent::SleepNow).await;
                    } else {
                        esp_println::println!("app: sleep deferred until book open settles");
                    }
                    continue;
                }

                if state.view == AppView::Reading
                    && should_block_post_open_confirm(event, &mut block_confirm_until)
                {
                    esp_println::println!("app: confirm ignored after book open");
                    continue;
                }

                if opening_book.is_some() || suppress_input_until_open_settled {
                    esp_println::println!("app: input ignored while book open pending");
                    continue;
                }

                let previous = state;
                let previous_persisted = state.persisted();
                state = state.apply_input(ctx, event);
                // Activity carries the post-input view so entering a view
                // immediately gets that view's idle leash (e.g. opening a
                // book starts the long Reading timeout right away).
                let _ = POWER_EVENTS.try_send(PowerEvent::Activity(state.view));
                if previous.type_settings() != state.type_settings()
                    || app_core::is_portrait(previous.orientation)
                        != app_core::is_portrait(state.orientation)
                {
                    reader_relayout_pending = true;
                }
                // Allocate the id speculatively and only commit it if a command
                // actually goes out: bumping the counter for a transition that
                // sends nothing would make an inflight open look stale, and the
                // storage task would skip it without ever answering.
                let request_id = peek_reader_request_id();
                let mut storage_command =
                    storage_command_for_transition(&previous, &state, request_id);
                if storage_command.is_none()
                    && reader_relayout_pending
                    && state.view == AppView::Reading
                {
                    if let Some(index) = ReaderSource::from_book_id(state.book_id).sd_index() {
                        storage_command = Some(extend_section_command(&state, index, request_id));
                    }
                }
                let dispatched = dispatch_transition_storage(
                    &mut pending_storage,
                    &mut state,
                    &previous,
                    storage_command,
                    request_id,
                    &mut opening_book,
                    &mut suppress_input_until_open_settled,
                    &mut open_rollback,
                    &mut reader_relayout_pending,
                    None,
                );
                let awaiting_chapter_list = dispatched.awaiting_chapter_list;
                let switch_dispatched = dispatched.switch_dispatched;
                // Read back after the dispatch: a rejected open has rolled the
                // state to where it started, which leaves nothing to persist
                // and no risk of writing the arriving book's position for a
                // switch that never happened.
                let next_persisted = state.persisted();
                if previous_persisted != next_persisted && !switch_dispatched {
                    dispatch_storage(
                        &mut pending_storage,
                        StorageCommand::StoreProgress(next_persisted),
                    );
                }
                if let Some(command) = forget_command_for_transition(&previous, &state) {
                    dispatch_storage(&mut pending_storage, command);
                }
                if let Some(command) = library_browse_command_for_transition(&previous, &state) {
                    if dispatch_storage(&mut pending_storage, command) == StorageDispatch::Rejected
                    {
                        // The reducer has already frozen the Library list on a
                        // move, waiting for an answer that only the storage
                        // task sends -- and it never got the command. Settle
                        // the wait here or the list stays frozen for good.
                        state = state.library_browse_rejected();
                    }
                }
                if let Some(command) = library_action_command_for_transition(&previous, &state) {
                    if dispatch_storage(&mut pending_storage, command) == StorageDispatch::Rejected
                    {
                        // The reducer has already put the screen on
                        // "clearing…", waiting for an event that only the
                        // storage task sends -- and it never got the command.
                        // Settle the wait here or it never ends.
                        state = state.library_action_rejected();
                    }
                }
                if let Some(command) = sync_command_for_transition(&previous, &state) {
                    esp_println::println!("app: sync command {:?}", command);
                    if SYNC_COMMANDS.try_send(command).is_err() {
                        esp_println::println!("app: sync command queue full");
                    }
                }
                // We used to suppress the render when an open was inflight
                // and wait for the Loaded event. That's fine when the cache
                // hits and the open returns in milliseconds, but on a cache
                // miss the rebuild can take a minute and the UI looks frozen
                // on the previous screen. Let the render through immediately:
                // the Reading view draws "OPENING EPUB" while sd_library's
                // loaded_index doesn't match the requested book. The chapter
                // overview is the exception: its list arrives in a beat, so it
                // waits for that Loaded rather than painting a partial frame.
                if awaiting_chapter_list {
                    render_pending = false;
                } else if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, &state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
            Either4::Second(event) => match event {
                DisplayEvent::Settled { chapter_cursor } => {
                    // Before the render lock comes off: clearing it is what
                    // lets the next navigation read state.chapter, and this
                    // correction is the whole reason the two travel together.
                    if let Some(cursor) = chapter_cursor {
                        state = state.apply_chapter_cursor(cursor);
                    }
                    rendering = false;
                    // Below the render lock, so a scenario waiting on this
                    // sees the same instant the app calls the cycle over.
                    #[cfg(feature = "bench-selftest")]
                    crate::bench_selftest::note_settled();
                    // The panel took a frame, so a later failure is a fresh one
                    // and gets its own retry.
                    repaint_retry.settled();
                    if !catalog_refresh_requested {
                        catalog_refresh_requested = true;
                        if STORAGE_COMMANDS
                            .try_send(StorageCommand::LoadCatalogCache)
                            .is_err()
                        {
                            esp_println::println!("app: storage queue full for catalog cache");
                        }
                    }
                    drain_parked_storage(
                        &mut pending_storage,
                        &mut opening_book,
                        &mut suppress_input_until_open_settled,
                    )
                    .await;
                    if render_pending {
                        send_render(RenderKind::Page, &state).await;
                        rendering = true;
                        render_pending = false;
                    } else if app_core::open_gate_may_lift(
                        suppress_input_until_open_settled,
                        opening_book.is_some(),
                        false,
                    ) {
                        suppress_input_until_open_settled = false;
                        block_confirm_until = Some(
                            Instant::now() + Duration::from_millis(POST_OPEN_CONFIRM_BLOCK_MS),
                        );
                    }
                    // Last, so a deferred press waits for the frame that
                    // resolves the open: the sleep screen is drawn from
                    // whatever last reached the panel, and releasing above
                    // would freeze the pre-open frame onto it.
                    release_deferred_sleep(
                        &mut sleep_gate,
                        opening_book,
                        &pending_storage,
                        suppress_input_until_open_settled,
                    )
                    .await;
                }
                DisplayEvent::Asleep => {
                    // Informational only. When the power task's handshake is
                    // still active, deep sleep follows and reboots the chip,
                    // so app state is moot. When that handshake was abandoned
                    // on Activity, the very input that abandoned it queued
                    // render/open work behind the Sleep command — resetting
                    // the render lock or open suppression here would erase
                    // that work's bookkeeping mid-flight. Every render is
                    // answered by its own Settled/RefreshFailed regardless of
                    // interleaved sleeps, so those acks alone advance state.
                    esp_println::println!("app: display asleep");
                }
                DisplayEvent::SleepFailed => {
                    // A sleep transition failed, not the current render: a
                    // render sent after the input that aborted the sleep
                    // handshake may still be queued behind that Sleep
                    // command, and its own Settled/RefreshFailed is coming.
                    // Clearing the render lock here would double-render and
                    // drop the coalesced pending frame, so leave both.
                    esp_println::println!("app: display sleep failed");
                }
                DisplayEvent::RefreshFailed => {
                    // The frame never reached the panel. Clear the render
                    // lock so the repaint below is not queued behind an
                    // acknowledgement that will never arrive, and drop the
                    // coalesced pending render: it described a frame for a
                    // panel state that no longer holds, and the repaint draws
                    // current state anyway, which is at least as fresh.
                    esp_println::println!("app: display refresh failed");
                    rendering = false;
                    render_pending = false;
                    // This failure ends the display cycle the same way
                    // Settled would, and it is the only other drain point
                    // for the parked storage commands: a queued book open
                    // left in pending_storage would otherwise hold
                    // opening_book forever and suppress every input.
                    drain_parked_storage(
                        &mut pending_storage,
                        &mut opening_book,
                        &mut suppress_input_until_open_settled,
                    )
                    .await;
                    // The panel is showing the page before the one the app is
                    // on -- or half of it -- and nothing else will correct
                    // that: the reader's next press would advance the page
                    // again and render *that*, so the frame they never saw
                    // reads as a page the device skipped. Storage used to
                    // paper over this by accident, because a page turn's
                    // extend answered with Loaded just behind the failure and
                    // any Loaded repaints; the RAM-hit turn no longer sends
                    // one. Ask for the repaint instead of depending on it, and
                    // spend it at most once per settled frame (RepaintRetry).
                    //
                    // Ordered exactly as the Settled arm: a render going out
                    // now leaves the open suppression for that render's own
                    // acknowledgement to lift, and only a cycle that ends here
                    // for good lifts it here.
                    if repaint_retry.failed() {
                        send_render(RenderKind::Page, &state).await;
                        rendering = true;
                    } else if app_core::open_gate_may_lift(
                        suppress_input_until_open_settled,
                        opening_book.is_some(),
                        false,
                    ) {
                        // Loaded may already have cleared opening_book before
                        // this failure discarded its render; without Settled
                        // ever arriving, the suppression flag must be released
                        // here or input stays ignored for good.
                        suppress_input_until_open_settled = false;
                        block_confirm_until = Some(
                            Instant::now() + Duration::from_millis(POST_OPEN_CONFIRM_BLOCK_MS),
                        );
                    }
                    // After the flag, as in Settled. A retry still in flight
                    // holds a deferred press only when an open is riding on it
                    // -- that is the frame the press must not overtake. A plain
                    // failed page turn releases, and the retry is already ahead
                    // of the Sleep command in the display queue, so the sleep
                    // screen is drawn from the repaired frame rather than the
                    // torn one.
                    release_deferred_sleep(
                        &mut sleep_gate,
                        opening_book,
                        &pending_storage,
                        suppress_input_until_open_settled,
                    )
                    .await;
                }
                DisplayEvent::Library(event) => {
                    if !handle_library_event(
                        ctx,
                        &mut state,
                        &mut opening_book,
                        &mut open_rollback,
                        &mut boot_render_pending,
                        &mut rendering,
                        &mut render_pending,
                        &mut suppress_input_until_open_settled,
                        &mut block_confirm_until,
                        &mut sleep_gate,
                        &mut pending_storage,
                        &mut reader_relayout_pending,
                        &event,
                    )
                    .await
                    {
                        continue;
                    }
                }
            },
            Either4::Third(event) => {
                if !handle_library_event(
                    ctx,
                    &mut state,
                    &mut opening_book,
                    &mut open_rollback,
                    &mut boot_render_pending,
                    &mut rendering,
                    &mut render_pending,
                    &mut suppress_input_until_open_settled,
                    &mut block_confirm_until,
                    &mut sleep_gate,
                    &mut pending_storage,
                    &mut reader_relayout_pending,
                    &event,
                )
                .await
                {
                    continue;
                }
            }
            Either4::Fourth(event) => {
                state = state.apply_sync_event(event);
                if state.view != AppView::Wireless {
                    continue;
                }
                if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, &state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
        }
    }
}

/// The first paint after boot uses `RenderKind::Boot` — a full refresh that
/// re-initialises the panel from its post-deep-sleep off state. Every paint
/// after that is an ordinary page. Consumes the one-shot flag.
fn first_render_kind(boot_render_pending: &mut bool) -> RenderKind {
    if core::mem::take(boot_render_pending) {
        RenderKind::Boot
    } else {
        RenderKind::Page
    }
}

/// Folds a library event into reader state and reports whether it owes a
/// render. Called by [`handle_library_event`], which owns the render-or-gate
/// decision that follows.
fn fold_library_event(
    ctx: ReducerContext,
    state: &mut ReaderState,
    opening_book: &mut Option<u32>,
    open_rollback: &mut Option<BookOpenRollback>,
    boot_render_pending: bool,
    event: &crate::LibraryEvent,
) -> bool {
    if let crate::LibraryEvent::BookOpenFailed { book_id } = *event {
        // The book was never opened, so the reader has to land back on the
        // one it was reading rather than sit on a title the storage task
        // refused. Always repaints: the screen is currently showing the open
        // that is not going to happen.
        if *opening_book == Some(book_id) {
            *opening_book = None;
        }
        if let Some(rollback) = open_rollback.take() {
            esp_println::println!(
                "app: book open failed book_id={book_id}; back to book_id={}",
                rollback.book_id
            );
            *state = state.restore_after_failed_open(rollback);
        }
        return true;
    }
    if let Some(book_id) = loaded_book_id(event) {
        if *opening_book == Some(book_id) {
            *opening_book = None;
            *open_rollback = None;
        }
    }
    // Folded first, and unconditionally: `Loaded` carries the navigation bounds
    // the reducer clamps against, and a fold skipped to save a refresh is a
    // bound left stale (see `loaded_repaints`). The repaint is then decided by
    // comparing what the fold actually moved.
    let folded = state.apply_library_event(ctx, *event);
    let should_render = if boot_render_pending {
        library_event_allows_first_render(event)
    } else {
        library_event_affects_view(state, &folded, event)
    };
    *state = folded;
    should_render
}

/// Folds a library event and either renders or lifts the open gate.
///
/// Returns `true` when the event was rendered (or pended), `false` when it owed
/// no repaint — in which case the open gate was checked, and the caller should
/// `continue` to the next loop iteration.
///
/// Shared by the two arms that deliver library events — `DisplayEvent::Library`
/// and the direct `LIBRARY_EVENTS` channel — which must handle them identically.
#[allow(clippy::too_many_arguments)]
async fn handle_library_event(
    ctx: ReducerContext,
    state: &mut ReaderState,
    opening_book: &mut Option<u32>,
    open_rollback: &mut Option<BookOpenRollback>,
    boot_render_pending: &mut bool,
    rendering: &mut bool,
    render_pending: &mut bool,
    suppress_input_until_open_settled: &mut bool,
    block_confirm_until: &mut Option<Instant>,
    sleep_gate: &mut SleepGate,
    parked: &mut ParkedStorage,
    reader_relayout_pending: &mut bool,
    event: &crate::LibraryEvent,
) -> bool {
    // A library event can put the reader on a different book: `RowIsBook`
    // answers a Library press with the catalog row it resolved to, and the
    // fold moves into Reading on it. That transition owes an open exactly as a
    // keypress into Reading does, and the app is the only place that can pay
    // it: the request id it commits is the one a later open is checked
    // against, the gate it arms keeps input off the panel until the book
    // lands, and the rollback it keeps is what puts the reader back if the
    // command is refused.
    let before = *state;
    if !fold_library_event(
        ctx,
        state,
        opening_book,
        open_rollback,
        *boot_render_pending,
        event,
    ) {
        lift_settled_open_gate(
            *rendering,
            *opening_book,
            suppress_input_until_open_settled,
            block_confirm_until,
            sleep_gate,
            parked,
        )
        .await;
        return false;
    }
    let request_id = peek_reader_request_id();
    let command = storage_command_for_transition(&before, state, request_id);
    let previous_persisted = before.persisted();
    // The one event whose index came from a row the storage task just looked
    // up. The open it owes is fenced to the catalog that lookup ran against.
    let catalog_fence = match event {
        crate::LibraryEvent::RowIsBook { catalog_epoch, .. } => Some(*catalog_epoch),
        _ => None,
    };
    let dispatched = dispatch_transition_storage(
        parked,
        state,
        &before,
        command,
        request_id,
        opening_book,
        suppress_input_until_open_settled,
        open_rollback,
        reader_relayout_pending,
        catalog_fence,
    );
    let next_persisted = state.persisted();
    if previous_persisted != next_persisted && !dispatched.switch_dispatched {
        dispatch_storage(parked, StorageCommand::StoreProgress(next_persisted));
    }
    if *rendering {
        *render_pending = true;
    } else {
        send_render(first_render_kind(boot_render_pending), state).await;
        *rendering = true;
        *render_pending = false;
    }
    true
}

fn library_event_affects_view(
    state: &ReaderState,
    folded: &ReaderState,
    event: &crate::LibraryEvent,
) -> bool {
    match *event {
        // A scan replaces the catalog wholesale. Even at an unchanged count
        // the rows may have been reordered, and the reducer acts on that:
        // it adopts the epoch, can clamp the selection, and dismisses an open
        // action sheet. Repainting on the count alone would leave the panel
        // showing the old rows — and an open sheet the state no longer has —
        // so the next press would be read against a list the reader cannot
        // see, up to Confirm opening a book under a sheet still on screen.
        crate::LibraryEvent::Scanned {
            count,
            catalog_epoch,
        } => {
            state.view == AppView::Library
                && (state.library_count != count || state.catalog_epoch != catalog_epoch)
        }
        crate::LibraryEvent::Loaded {
            book_id,
            pages: _,
            chapters: _,
            current_chapter: _,
            chapter_pages: _,
            position: _,
            text_replaced,
        } => {
            state.book_id == book_id
                && app_core::loaded_repaints(
                    state.view == AppView::Reading,
                    text_replaced,
                    folded.page != state.page,
                )
        }
        // Handled before the reducer; never reaches here.
        crate::LibraryEvent::BookOpenFailed { .. } => true,
        crate::LibraryEvent::ChapterPage {
            book_id,
            chapter,
            page,
        } => {
            state.book_id == book_id
                && state
                    .sd_chapter_pages
                    .get(chapter as usize)
                    .map(|stored| *stored != page.min(u16::MAX as u32) as u16)
                    .unwrap_or(false)
        }
        crate::LibraryEvent::CustomFont { .. } => state.view == AppView::Settings,
        crate::LibraryEvent::Restored { .. } => true,
        // The settled note only shows while the user is still waiting in
        // Library on this very request; an abandoned clear's answer changes
        // nothing on screen, so it must not cost a panel refresh either.
        crate::LibraryEvent::CacheCleared { request_id, .. } => {
            state.view == AppView::Library
                && matches!(
                    state.library_menu,
                    app_core::LibraryMenu::Busy { request_id: outstanding, .. }
                        if outstanding == request_id
                )
        }
        // A move through the tree replaces the rows, and a book found under
        // one leaves Library outright. Compare the fold rather than the
        // event: an answer nobody is waiting on any more changes nothing,
        // and must not cost a panel refresh either.
        // Always: the status it travels with is what the screen reads, and
        // the fold cannot see it, so a repaint that waited on the fold moving
        // would leave the panel showing rows the store no longer has.
        crate::LibraryEvent::LibraryUnreadable { .. } => true,
        crate::LibraryEvent::FolderListed { .. }
        | crate::LibraryEvent::RowIsBook { .. }
        | crate::LibraryEvent::RowFailed { .. } => {
            folded.view != state.view
                || folded.library_depth != state.library_depth
                || folded.library_count != state.library_count
                || folded.library_books != state.library_books
                || folded.selection != state.selection
                || folded.library_browse != state.library_browse
        }
    }
}

fn library_event_allows_first_render(event: &crate::LibraryEvent) -> bool {
    matches!(
        event,
        crate::LibraryEvent::Restored { .. } | crate::LibraryEvent::Scanned { .. }
    )
}

/// Sends `command`, parks it behind whatever is already waiting, or reports
/// that it could do neither. See [`ParkedStorage`] for why an open is never
/// the thing that gets dropped.
fn dispatch_storage(parked: &mut ParkedStorage, command: StorageCommand) -> StorageDispatch {
    let outcome = parked.dispatch(command, |command| {
        STORAGE_COMMANDS.try_send(command).is_ok()
    });
    match outcome {
        StorageDispatch::Sent => log_storage_command("send", command),
        StorageDispatch::Parked => log_storage_command("queue", command),
        StorageDispatch::Rejected => log_storage_command("rejected", command),
    }
    outcome
}

/// Lifts the open's input gate on an event that owed no repaint.
///
/// The gate is normally lifted where a display cycle ends, because for an open
/// that repaints the cycle *is* the open landing on the panel. An open answered
/// out of the resident section window at the page the reader was already on
/// moves nothing on screen, so `loaded_repaints` sends no render and no cycle
/// ends — and this event is the last thing that open produces. Left here, the
/// gate never opens again: input ignored, sleep deferred, until the battery
/// goes.
///
/// A frame still in flight is left alone: its own acknowledgement lifts the
/// gate, and it is the frame a deferred press must not overtake.
async fn lift_settled_open_gate(
    rendering: bool,
    opening_book: Option<u32>,
    suppress_input_until_open_settled: &mut bool,
    block_confirm_until: &mut Option<Instant>,
    sleep_gate: &mut SleepGate,
    parked: &ParkedStorage,
) {
    if !app_core::open_gate_may_lift(
        *suppress_input_until_open_settled,
        opening_book.is_some(),
        rendering,
    ) {
        return;
    }
    *suppress_input_until_open_settled = false;
    // The same debounce the acknowledgement path applies: the press that opened
    // the book must not carry through into the page now on screen.
    *block_confirm_until = Some(Instant::now() + Duration::from_millis(POST_OPEN_CONFIRM_BLOCK_MS));
    // After the flag, as everywhere else, so the blockers it feeds are current.
    release_deferred_sleep(
        sleep_gate,
        opening_book,
        parked,
        *suppress_input_until_open_settled,
    )
    .await;
}

/// Sends a Power press that was held back, once the app owes nothing — neither
/// storage work nor the frame that resolves an open.
///
/// Called from the two points that end a display cycle and from
/// [`lift_settled_open_gate`], which is the case where no cycle ends. In all
/// three *after* the open suppression is lifted, so the press cannot overtake
/// the frame it is waiting for.
async fn release_deferred_sleep(
    gate: &mut SleepGate,
    opening_book: Option<u32>,
    parked: &ParkedStorage,
    awaiting_open_frame: bool,
) {
    if gate.release(sleep_blockers(opening_book, parked, awaiting_open_frame)) {
        esp_println::println!("app: deferred sleep released");
        let _ = POWER_EVENTS.send(PowerEvent::SleepNow).await;
    }
}

fn sleep_blockers(
    opening_book: Option<u32>,
    parked: &ParkedStorage,
    awaiting_open_frame: bool,
) -> SleepBlockers {
    SleepBlockers {
        open_unresolved: opening_book.is_some(),
        parked_storage: !parked.is_empty(),
        awaiting_open_frame,
    }
}

/// Hands parked commands to the storage task in arrival order, for as long as
/// the channel takes them. A refusal ends the drain with the rest still
/// parked, in order; nothing is lost, and the loop's own offer branch retries
/// as soon as a slot frees. Async only so it reads like the other arms of the
/// loop — it never awaits, which is the point (see the body).
async fn drain_parked_storage(
    parked: &mut ParkedStorage,
    opening_book: &mut Option<u32>,
    suppress_input_until_open_settled: &mut bool,
) {
    while let Some(command) = parked.front() {
        // Offered, never awaited. Blocking here would park this task inside
        // the event arm, where it is neither receiving library events nor
        // returning to its select — and the display task stops taking storage
        // commands while it holds a settling event that only this task can
        // receive. Two live tasks, each waiting on the other. Waiting for a
        // slot is only safe from the select, where the receivers stay live.
        if STORAGE_COMMANDS.try_send(command).is_err() {
            return;
        }
        accept_parked_storage(
            parked,
            opening_book,
            suppress_input_until_open_settled,
            command,
        );
    }
}

/// Waits for the storage task to take the oldest parked command, or forever
/// when nothing is parked.
///
/// Safe to await only as a branch of the main loop's select: losing the race
/// cancels the send, and winning it means the command is already queued, so
/// the caller owes it a `pop_front`. `select` polls the receivers first and
/// returns on the first ready branch, so a completed send is never discarded.
async fn offer_parked_storage(parked: &ParkedStorage) -> StorageCommand {
    let Some(command) = parked.front() else {
        return core::future::pending::<StorageCommand>().await;
    };
    STORAGE_COMMANDS.send(command).await;
    command
}

/// Records a parked command that has just reached the storage task: it is off
/// the queue, and an open now owns the input lock until its frame settles.
fn accept_parked_storage(
    parked: &mut ParkedStorage,
    opening_book: &mut Option<u32>,
    suppress_input_until_open_settled: &mut bool,
    command: StorageCommand,
) {
    parked.pop_front();
    log_storage_command("send", command);
    if let Some(book_id) = open_book_id(command) {
        *opening_book = Some(book_id);
        *suppress_input_until_open_settled = true;
    }
}

fn open_book_id(command: StorageCommand) -> Option<u32> {
    match command {
        StorageCommand::OpenBook { book_id, .. } => Some(book_id),
        _ => None,
    }
}

fn loaded_book_id(event: &crate::LibraryEvent) -> Option<u32> {
    match *event {
        crate::LibraryEvent::Loaded { book_id, .. } => Some(book_id),
        _ => None,
    }
}

fn should_block_post_open_confirm(event: InputEvent, block_until: &mut Option<Instant>) -> bool {
    let Some(until) = *block_until else {
        return false;
    };
    if Instant::now() >= until {
        *block_until = None;
        return false;
    }
    matches!(
        event,
        InputEvent::Sample {
            button: Some(Button::Confirm),
            ..
        }
    )
}

async fn send_render(kind: RenderKind, state: &ReaderState) {
    // Freeze and stamp together. This is the only place a render is sent, and
    // the instant the state stops being able to change is the boundary the
    // bench pairs presses against -- see `RenderRequest::requested_at_ms`.
    // Stamping on the consumer side instead was wrong: a render queued while
    // the display task is mid-flush, mid-prestage, or inside a storage or
    // background-build step waits for all of it, and a press arriving during
    // that wait would be credited to a frame frozen before it existed.
    // Publishing here and not after apply_input is the difference between
    // seeing the app's state and seeing only the presses. A Library pick is
    // answered by the card, so the move to Reading arrives as a storage
    // event and never touches the input arm: a scenario watching the input
    // side would still read "library" after the book opened, and press
    // Confirm into a hold that Library keeps while a pick is in flight.
    #[cfg(feature = "bench-selftest")]
    crate::bench_selftest::publish_view(
        state.view,
        state.orientation,
        state.front_buttons == app_core::FrontButtons::PagesLeft,
        state.library_depth,
        state.page,
    );
    let mut request = state.render_request(kind);
    request.requested_at_ms = Instant::now().as_millis();
    DISPLAY_COMMANDS.send(DisplayCommand::Render(request)).await;
}

/// What a dispatched transition owes the rest of the loop.
struct OpenDispatch {
    /// The open carried the departing book's position, so no separate
    /// progress write may follow it.
    switch_dispatched: bool,
    /// A chapter list is genuinely in flight, so the current frame holds
    /// until it lands.
    awaiting_chapter_list: bool,
}

/// Dispatch the storage command a transition owes, with the bookkeeping an
/// open comes with.
///
/// Shared by the two paths that can put the reader on a different book: a
/// keypress, and a library event that resolves a Library row to one. Both owe
/// exactly the same things, and the reason this is one function rather than
/// two is that they drifted apart the moment they were two: the row-open path
/// sent its command from the storage task instead, which committed no reader
/// request id (so the open read as stale and was skipped), armed no open gate,
/// and kept no rollback for a refusal.
#[expect(clippy::too_many_arguments)] // The parked storage, the state, the transition and the four sinks it needs; fires in every X4 and X3 build
fn dispatch_transition_storage(
    parked: &mut ParkedStorage,
    state: &mut ReaderState,
    previous: &ReaderState,
    storage_command: Option<StorageCommand>,
    request_id: u32,
    opening_book: &mut Option<u32>,
    suppress_input_until_open_settled: &mut bool,
    open_rollback: &mut Option<BookOpenRollback>,
    reader_relayout_pending: &mut bool,
    catalog_fence: Option<u32>,
) -> OpenDispatch {
    // An open that came from resolving a Library row says which catalog the
    // row was resolved in, so the storage task can refuse it when a rebuild
    // has put a different book under that number in the meantime. Stamped
    // here rather than inside the transition, because only the caller knows
    // where the index came from.
    let storage_command = match storage_command {
        Some(StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            type_settings,
            portrait,
            previous,
            catalog_epoch,
            resolve_place,
        }) => Some(StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            type_settings,
            portrait,
            previous,
            // The event is the better witness: it carries the epoch the row
            // was resolved in, including for a row naming the book already
            // being read, which the state diff cannot tell from staying put.
            // The reducer's own reading stands only where no event answered.
            catalog_epoch: catalog_fence.or(catalog_epoch),
            resolve_place,
        }),
        other => other,
    };
    // A book change closes out the departing book inside its own open. The
    // separate progress record that used to follow named the *new* book, so it
    // wrote that book's position file at the page the open had not resolved
    // yet, erasing the very place the reader was about to resume from.
    let open_owns_the_switch = matches!(
        storage_command,
        Some(StorageCommand::OpenBook {
            previous: Some(_),
            ..
        })
    );
    // The chapter overview can't paint its rows until the on-disk list lands;
    // hold the current frame and let the Loaded event render once, rather than
    // flashing a partial first frame and spending an extra panel refresh. Only
    // when the command is truly in flight -- a queued command relies on the
    // render's Settled to be drained, so it must still render.
    let mut awaiting_chapter_list = false;
    let mut switch_dispatched = false;
    if let Some(command) = storage_command {
        match dispatch_storage(parked, command) {
            StorageDispatch::Rejected => {
                // Nothing reached the storage task, so nothing is coming back.
                // Arming the open lock here would wait on a Loaded that cannot
                // arrive and ignore every button until the battery is pulled;
                // put the reader back on the book it never actually left
                // instead. The render below then redraws that book.
                if open_book_id(command).is_some() {
                    *state = state.restore_after_failed_open(previous.open_rollback());
                }
            }
            outcome => {
                // Open/extend commands carry the current type settings, so any
                // dispatched command syncs the reader store.
                *reader_relayout_pending = false;
                commit_reader_request_id(request_id);
                switch_dispatched = open_owns_the_switch;
                // What an open in flight leaves the app holding, decided in
                // `app_core` where a test can walk the whole sequence: the
                // book being waited on, and where to put the reader back if
                // the open is refused rather than answered.
                let hold = app_core::open_hold(&command, previous);
                if let Some(book_id) = hold.opening_book {
                    *opening_book = Some(book_id);
                    *suppress_input_until_open_settled = true;
                    *open_rollback = hold.rollback;
                }
                if outcome == StorageDispatch::Sent
                    && matches!(command, StorageCommand::LoadChapters { .. })
                {
                    awaiting_chapter_list = true;
                }
            }
        }
    }
    OpenDispatch {
        switch_dispatched,
        awaiting_chapter_list,
    }
}

fn log_storage_command(label: &str, command: StorageCommand) {
    match command {
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            previous,
            ..
        } => esp_println::println!(
            "app: storage {label} open request={request_id} book_id={book_id} index={index} chapter={chapter} target={target_pages} closing={}",
            previous.map_or(0, |state| state.book_id)
        ),
        StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            ..
        } => esp_println::println!(
            "app: storage {label} extend request={request_id} book_id={book_id} index={index} chapter={chapter} target={target_pages}"
        ),
        StorageCommand::StoreProgress(_) => {
            esp_println::println!("app: storage {label} progress")
        }
        StorageCommand::LoadCatalogCache => {
            esp_println::println!("app: storage {label} load catalog cache")
        }
        StorageCommand::RefreshCatalog => {
            esp_println::println!("app: storage {label} refresh catalog")
        }
        StorageCommand::LoanSyncMemory => {
            esp_println::println!("app: storage {label} loan sync memory")
        }
        StorageCommand::StoreWifiCredentials(_) => {
            esp_println::println!("app: storage {label} wifi credentials")
        }
        StorageCommand::StoreWifiApHint { .. } => {
            esp_println::println!("app: storage {label} wifi ap hint")
        }
        StorageCommand::ForgetWifiCredentials => {
            esp_println::println!("app: storage {label} forget wifi credentials")
        }
        StorageCommand::ClearBookCache {
            request_id,
            index,
            browse_epoch,
        } => {
            esp_println::println!(
                "app: storage {label} clear cache request={request_id} index={index} browse={browse_epoch}"
            )
        }
        StorageCommand::ReceiveUpload => {
            esp_println::println!("app: storage {label} receive upload")
        }
        StorageCommand::LoadChapters {
            request_id,
            book_id,
            index,
        } => esp_println::println!(
            "app: storage {label} load chapters request={request_id} book_id={book_id} index={index}"
        ),
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            ..
        } => esp_println::println!(
            "app: storage {label} jump chapter request={request_id} book_id={book_id} index={index} chapter={chapter}"
        ),
        StorageCommand::ChooseLibraryRow {
            request_id,
            index,
            browse_epoch,
        } => esp_println::println!(
            "app: storage {label} choose row request={request_id} index={index} browse={browse_epoch}"
        ),
        StorageCommand::LeaveLibraryFolder {
            request_id,
            browse_epoch,
        } => esp_println::println!(
            "app: storage {label} leave folder request={request_id} browse={browse_epoch}"
        ),
    }
}

fn reducer_context() -> ReducerContext {
    ReducerContext::new(catalog::book_count(), catalog::chapter_count())
}

/// Confirm on the Wireless screen arms `Starting`; leaving the screen after
/// the radio ran has to reset the device because the loaned memory can
/// never come back.
fn sync_command_for_transition(previous: &ReaderState, next: &ReaderState) -> Option<SyncCommand> {
    if previous.sync_status != SyncStatus::Starting && next.sync_status == SyncStatus::Starting {
        return Some(SyncCommand::Start);
    }
    if previous.view == AppView::Wireless && next.view != AppView::Wireless {
        let radio_ran = !matches!(
            previous.sync_status,
            SyncStatus::NotConfigured | SyncStatus::Idle | SyncStatus::ForgetPending
        );
        if radio_ran {
            return Some(SyncCommand::Exit);
        }
    }
    None
}

/// Confirming the pending forget on the Wireless screen deletes the saved
/// credentials from the card. Both states live before any radio work, so
/// the storage path is still whole.
fn forget_command_for_transition(
    previous: &ReaderState,
    next: &ReaderState,
) -> Option<StorageCommand> {
    (previous.sync_status == SyncStatus::ForgetPending
        && next.sync_status == SyncStatus::NotConfigured
        && next.view == AppView::Wireless)
        .then_some(StorageCommand::ForgetWifiCredentials)
}

/// Reserves the next reader request id without publishing it.
///
/// Publishing is a separate step because the id is how the storage task
/// recognises a stale request: bumping the counter for a transition that ends
/// up sending nothing would strand an open already in flight, which would be
/// skipped as stale and never answer with the `Loaded` the app is waiting on.
fn peek_reader_request_id() -> u32 {
    LATEST_READER_REQUEST_ID
        .load(Ordering::Relaxed)
        .wrapping_add(1)
        .max(1)
}

fn commit_reader_request_id(request_id: u32) {
    LATEST_READER_REQUEST_ID.store(request_id, Ordering::Relaxed);
}
