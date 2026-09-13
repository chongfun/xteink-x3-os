//! The storage/display task's command loop, as sequences a host can drive.
//!
//! The task itself owns the SD card, the panel, and a 47 KB reader store, so it
//! cannot run off-device. But almost nothing that has gone wrong in it was about
//! the hardware. The faults were about order: a `Sleep` overtaking work the app
//! had already handed over, a pre-sleep drain swallowing the one command it was
//! not allowed to answer, a book-open transaction announcing a switch whose
//! first write never landed. Every one of those is decidable without touching a
//! card.
//!
//! So the ordering lives here, as sequences the task drives rather than code the
//! task contains. Each `next` names one piece of work; the caller does it and
//! reports back what the hardware said. The firmware answers with a real card;
//! the tests answer with a card model that can be told to fail any individual
//! write, and drive the same state machines the firmware does.
//!
//! RAM (measured): `SleepSequence` is 24 bytes and holds no command — the
//! drained one stays with the caller. `OpenSequence` is 84: it carries the
//! departing 28-byte `PersistedAppState` twice, once as the pending close-out
//! step and once as the base the global pointer record is built from. Both sit
//! on the display task's stack for the length of one command, replacing the
//! `Option<PersistedAppState>` and the resolved chapter/page/resumed locals the
//! open arm carried before, so about 44 bytes more is live across
//! `build_or_load_book_cache` — negligible against that chain's 30-43 KB
//! region, and none of it is added to the deep frames themselves.

use crate::{PersistedAppState, StorageCommand, SyncSession};
use display::font::TypeSettings;

/// Which arm of the loop answers a storage command.
///
/// The routing matters because one command is not the storage handler's to
/// apply: `ReceiveUpload` is a request to *become* the upload writer for the
/// rest of the session, which only the loop can do — it is the loop that owns
/// the card and can park on the upload channels. The handler's arm for it is a
/// deliberate no-op, so anything that reaches the handler with an upload in hand
/// has already lost it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopArm {
    /// Hand the card to the upload session until it ends.
    UploadSession,
    /// An upload arrived with no session to serve it. Nothing owns the writer,
    /// so there is nothing to enter; drop it.
    RefusedUpload,
    /// Everything else goes to the storage handler, which applies the session
    /// gate itself — the pre-sleep drain reaches it without passing here.
    Apply,
}

/// Where a selected storage command goes.
pub fn loop_arm(command: &StorageCommand, session: SyncSession) -> LoopArm {
    if !matches!(command, StorageCommand::ReceiveUpload) {
        return LoopArm::Apply;
    }
    if session.admits(command) {
        LoopArm::UploadSession
    } else {
        LoopArm::RefusedUpload
    }
}

/// Why a sleep request was turned down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepRefusal {
    /// An upload request was still queued, and was put back. The loop picks it
    /// up next and `upload_session` does its own sleep handling, re-queueing
    /// this generation once the filesystem is closed.
    UploadQueued,
    /// The declined upload request would not go back on the queue, which can
    /// only mean a producer refilled the slot it came out of. The request
    /// itself is lost, but a full queue is not: the channel now holds a whole
    /// budget's worth of accepted work, so refuse and let the ordinary loop
    /// apply it through the normal routing rather than this drain's restricted
    /// path.
    UploadLost,
    /// The coalesced progress record would not land. Sleeping now would lose it
    /// for good, so stay awake and let the next flush retry.
    ProgressUnwritten,
}

/// What the pre-sleep sequence wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepAction {
    /// Take one more command off the storage queue, then report it through
    /// [`SleepSequence::drained`] or [`SleepSequence::queue_empty`].
    TakeQueued,
    /// Write the coalesced progress record, then report through
    /// [`SleepSequence::flushed`].
    FlushProgress,
    /// Do not sleep. Tell the power task, and leave the panel up.
    Refuse(SleepRefusal),
    /// Everything owed has reached the card. Render the sleep frame and put the
    /// panel down.
    Proceed,
}

/// What the drain does with one command it took off the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drained {
    /// Apply it against the card before the panel sleeps.
    Apply,
    /// Not the drain's to answer. Put it back and refuse this sleep.
    RequeueAndRefuse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SleepPhase {
    Draining,
    Flushing,
    Refused(SleepRefusal),
    Ready,
}

/// Everything the task owes the card before the panel may sleep, in order.
///
/// Deep sleep is terminal — waking is a fresh boot — so whatever is still
/// queued when the panel goes down is simply gone. The loop takes display
/// commands ahead of storage ones, so a `Sleep` routinely arrives in front of
/// work handed over a moment earlier, and the app cannot close that itself:
/// once it has passed a command to the channel it has no way to know whether
/// the task has applied it. The guarantee has to live where the card is owned.
///
/// The drain is bounded by the channel's own depth. It is only ever catching up
/// on what was already accepted, never following a producer that keeps writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepSequence {
    phase: SleepPhase,
    drained: usize,
    budget: usize,
}

impl SleepSequence {
    /// `drain_budget` is the storage channel's capacity: the most commands that
    /// can be waiting when the sleep is selected.
    pub const fn new(drain_budget: usize) -> Self {
        Self {
            phase: if drain_budget == 0 {
                SleepPhase::Flushing
            } else {
                SleepPhase::Draining
            },
            drained: 0,
            budget: drain_budget,
        }
    }

    pub const fn next(&self) -> SleepAction {
        match self.phase {
            SleepPhase::Draining => SleepAction::TakeQueued,
            SleepPhase::Flushing => SleepAction::FlushProgress,
            SleepPhase::Refused(refusal) => SleepAction::Refuse(refusal),
            SleepPhase::Ready => SleepAction::Proceed,
        }
    }

    /// The verdict for one command taken off the queue. Pure: the caller acts
    /// on it and then reports back through `applied` or `requeued`.
    pub const fn drained(&self, command: &StorageCommand) -> Drained {
        match command {
            StorageCommand::ReceiveUpload => Drained::RequeueAndRefuse,
            _ => Drained::Apply,
        }
    }

    /// The queue had nothing more.
    pub fn queue_empty(&mut self) {
        self.phase = SleepPhase::Flushing;
    }

    /// A drained command was applied against the card.
    pub fn applied(&mut self) {
        self.drained += 1;
        if self.drained >= self.budget {
            self.phase = SleepPhase::Flushing;
        }
    }

    /// Whether the command the drain declined actually made it back onto the
    /// queue.
    ///
    /// Either answer refuses the sleep, but for opposite reasons, and the
    /// difference is why this takes the result rather than assuming it. A
    /// put-back that landed means the loop will answer the request. A put-back
    /// that failed can only mean a producer refilled the slot the command came
    /// out of — so the channel is *full*, not one short. Carrying on would then
    /// be the worst of both: the drain would spend its remaining budget on a
    /// queue that had grown behind it and hand whatever it could not reach to a
    /// terminal sleep.
    pub fn requeued(&mut self, restored: bool) {
        self.phase = SleepPhase::Refused(if restored {
            SleepRefusal::UploadQueued
        } else {
            SleepRefusal::UploadLost
        });
    }

    /// Whether the coalesced progress record reached the card.
    pub fn flushed(&mut self, stored: bool) {
        self.phase = if stored {
            SleepPhase::Ready
        } else {
            SleepPhase::Refused(SleepRefusal::ProgressUnwritten)
        };
    }
}

/// Which walk of an EPUB spine is running, and therefore where it may stop.
///
/// A cold build used to run the whole spine before the reader saw anything —
/// 64 s on the measured 11.7 MB book. It does not have to: section files are
/// written as the walk goes, so the book can be published the moment the
/// requested page is covered and the rest finished in the background. That
/// splits one walk into two kinds with different stopping rules, and this is
/// the difference.
///
/// Stopping is only possible at a spine boundary. Suspending mid-item would
/// mean persisting the XML tokenizer, the block parser, and the inflate window,
/// which does not fit the RAM budget — so a step is always at least one spine
/// item, and [`Self::Background`]'s budget decides how many more it batches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildPhase {
    /// The walk the reader is waiting on. It owes exactly one thing: the
    /// section holding `requested_page`. Everything past that is the
    /// background's problem.
    FirstOpen { requested_page: u32 },
    /// A background continuation. It owes nothing to any wait, so the only
    /// question is how long the display task may stay inside it before a
    /// queued render or page turn gets a turn.
    Background { slice_ms: u64 },
}

impl BuildPhase {
    /// Whether the walk should stop at the spine boundary it has just reached.
    ///
    /// `more_spine` is the whole reason this takes the walk's position rather
    /// than just its counters: suspending with nothing left to build would
    /// hand the caller a continuation that finds no work, publish the book
    /// twice, and mark a complete cache partial.
    pub const fn suspend_here(
        &self,
        total_pages: u32,
        sections: usize,
        elapsed_ms: u64,
        more_spine: bool,
    ) -> bool {
        if !more_spine {
            return false;
        }
        match *self {
            // `>` not `>=`: pages are zero-based, so covering page N means
            // having built N+1 of them.
            Self::FirstOpen { requested_page } => sections > 0 && total_pages > requested_page,
            Self::Background { slice_ms } => elapsed_ms >= slice_ms,
        }
    }

    /// Whether to stop *before* walking the next spine item, given what that
    /// item will probably cost.
    ///
    /// [`Self::suspend_here`] runs after an item and so cannot bound a step: it
    /// passes under budget, the next item runs for seconds, and the step
    /// overshoots. Measured on device, steps were min 423, **median 1997**, max
    /// 3464 ms against a 400 ms budget, and a page turn cannot preempt one --
    /// page turns during the background phase measured up to 3451 ms against a
    /// <550 ms target. Looking at the item's size before committing to it is
    /// what turns the budget into an actual bound.
    ///
    /// `walked_this_step` is what guarantees forward progress: a step always
    /// takes at least one item, however large, or a book whose every chapter
    /// exceeds the budget would never advance. So this bounds a step to roughly
    /// the budget *plus one item*, and a single item larger than the budget
    /// still has to run somewhere -- mid-item suspension would mean persisting
    /// the XML tokenizer, block parser and inflate window, which does not fit
    /// the RAM budget.
    ///
    /// Only [`Self::Background`] answers true. A first open must reach the page
    /// it was asked for however long that takes; stopping short of it would
    /// publish a book that cannot show the page the reader opened.
    pub const fn suspend_before(
        &self,
        elapsed_ms: u64,
        next_item_bytes: u32,
        walked_this_step: usize,
    ) -> bool {
        if walked_this_step == 0 {
            return false;
        }
        match *self {
            Self::FirstOpen { .. } => false,
            Self::Background { slice_ms } => {
                elapsed_ms + estimated_item_ms(next_item_bytes) > slice_ms
            }
        }
    }
}

/// Whether a book index that stops short of the whole book may be read from.
///
/// A progressive open publishes an index spanning only what it has built, and
/// that is safe *while the walk that published it is still running* — the walk
/// keeps raising the page count, and the reader is never fenced in for long.
///
/// It is not safe on its own. The reducer clamps the page to the advertised
/// count, so the reader cannot ask for the first missing page; there is no
/// input that provokes a rebuild. An index left behind by a build that sleep or
/// a reboot ended would therefore cap the book *permanently*, and the truncation
/// would look exactly like the book simply being that short. So an unfinished
/// index with nobody building it is refused, and the open rebuilds — which is
/// itself progressive, so the first page still arrives in about a second.
///
/// `unfinished` must mean "a walk stopped here intending to return", not merely
/// "pages are missing": a book clipped by the spine cap or the section cap is
/// partial forever, and rebuilding it on every open would buy nothing.
pub const fn partial_index_is_usable(unfinished: bool, walk_is_live: bool) -> bool {
    !unfinished || walk_is_live
}

/// Whether a background build step must re-announce the book's shape.
///
/// Announcing is not free. Every `LibraryEvent::Loaded` marks the whole screen
/// dirty, so a step-by-step denominator would buy a panel refresh per section —
/// on the measured book, one every ~640 ms for a minute. Silence is the default
/// and the finish is what publishes the real total.
///
/// The exception is the reader who has caught up with the frontier. The
/// reducer clamps the page to the advertised count, so at `advertised - 1` the
/// next-page button does nothing at all: no state change, no render, no
/// command. That is indistinguishable from a broken device, and it is the one
/// case where the repaint is worth its cost.
pub const fn background_announce(finished: bool, reader_page: u32, advertised_before: u32) -> bool {
    finished || reader_page.saturating_add(1) >= advertised_before
}

/// Whether a background step that stopped early still owes an announcement.
///
/// A step can grow the book and *then* fail — a refused index write leaves the
/// resident index longer than the one the app knows about. The walk is over
/// either way, but those extra pages are built, on the card, and reachable; the
/// only thing keeping the reader off them is a page count nobody updated. That
/// is the same dead next-page button [`background_announce`] exists to prevent,
/// so it gets the same answer, with two conditions rather than one.
///
/// Growth is required because this is not the finish: with nothing new to say,
/// a repaint would cost a full panel refresh to redraw the same number. And the
/// caller must have established that the store is coherent — a step that could
/// not put the reader's page back may not be repainted on at any page count.
pub const fn stopped_announce(
    advertised_before: u32,
    advertised_now: u32,
    reader_page: u32,
) -> bool {
    advertised_now > advertised_before && background_announce(false, reader_page, advertised_before)
}

/// How long a spine item of `bytes` uncompressed XHTML takes to lay out.
///
/// Fitted to the 2026-07-28 X3 capture, where item cost tracked uncompressed
/// size closely across the range that matters: 24,401 B took 1130 ms, 37,437 B
/// took ~1530 ms, 47,601 B took 2282 ms and 58,245 B took 2832 ms -- about
/// 20 bytes per millisecond. It only has to be good enough to tell "this will
/// blow the slice" from "this will not", so a coarse linear fit is the right
/// amount of model; being wrong costs one overlong step, not correctness.
const LAYOUT_BYTES_PER_MS: u32 = 20;

const fn estimated_item_ms(bytes: u32) -> u64 {
    (bytes / LAYOUT_BYTES_PER_MS) as u64
}

/// How long the loop waits after a background step before it may claim the
/// display task again.
///
/// A step ends and the branch that runs it is ready again immediately, so a
/// single `yield_now` handed the app task one poll and no more. Measured on
/// device: 0.2 ms between one step ending and the next beginning, while a page
/// turn pressed 86 ms *earlier* had still not been reduced into a render -- the
/// reader then waited out a whole extra 2912 ms step for a page that was
/// already available. This is the window in which the app can turn a button
/// press into a render, which the loop services ahead of the next slice.
///
/// Small enough to be noise against a slice: 50 ms on a 400 ms budget is 12%
/// of build throughput in the worst case and nothing at all when the reader is
/// idle, because the branch only re-arms after a step that actually ran.
pub const BACKGROUND_SETTLE_MS: u64 = 50;

/// The longest a background build waits between attempts at a step that never
/// began.
pub const BACKGROUND_RETRY_CEILING_MS: u64 = 30_000;

/// How long to wait before re-attempting a step that never began, after
/// `attempts` consecutive failures to begin.
///
/// The distinction this rests on is not how bad the error was but how much of
/// the walk it consumed. A step can fail before touching a single record — the
/// card refuses a session, the EPUB will not open — and everything the walk
/// needs is then exactly as it was. Nothing is lost by coming back for it.
///
/// Coming back matters because nothing else will. A step like that builds no
/// pages, so [`stopped_announce`] has nothing to say, and a reader who has
/// already caught up with the frontier has no page turn that could provoke a
/// rebuild: the reducer clamps Next to the page they are on, so no command is
/// issued at all. Until they turn a page some other way, the walk is the only
/// thing that can still raise their page count — so the walk is not given up
/// on, however long the card stays away.
///
/// The backoff is what makes never giving up affordable. It is also what
/// replaced a hard attempt cap: the cap only existed because the step branch
/// was always ready and a kept walk would spin as fast as the loop came round.
/// Waiting on a timer removes that pressure entirely, and an outage measured in
/// minutes then costs two SD session attempts a minute on a device that is
/// otherwise idle.
pub const fn background_retry_delay_ms(attempts: u8) -> u64 {
    // 250 ms, 1 s, 4 s, 16 s, then the ceiling. Quadrupling covers a card
    // fault of any length in five steps without polling through a long one.
    if attempts == 0 {
        return 0;
    }
    if attempts >= 5 {
        return BACKGROUND_RETRY_CEILING_MS;
    }
    let ms = 250_u64 << (2 * (attempts as u32 - 1));
    if ms > BACKGROUND_RETRY_CEILING_MS {
        BACKGROUND_RETRY_CEILING_MS
    } else {
        ms
    }
}

/// What the book-open transaction wants next.
///
/// The order is the design: the departing book's position is written while its
/// catalog entry is still the active one, the book is opened, and only then does
/// the global pointer move — to the position the open actually landed on. Every
/// step that touches the card is a variant here, so the sequence a card sees is
/// a value a test can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAction {
    /// Write the departing book's position to that book's own file. Must happen
    /// before the catalog slot swaps to the incoming book, or the key this
    /// write needs can no longer be resolved. Report through
    /// [`OpenSequence::departing_stored`].
    CloseOutDeparting(PersistedAppState),
    /// The close-out failed, so the open was never attempted. Announce the
    /// refusal: the app has already left the book that owns that page and will
    /// never reissue it, so a silent return would strand the reader.
    Refuse { book_id: u32 },
    /// Read this book's catalog record into the active-entry slot and adopt the
    /// command's layout. Report through [`OpenSequence::staged`].
    StageBook {
        index: u16,
        type_settings: TypeSettings,
        portrait: bool,
    },
    /// Read this book's own saved position. Report through
    /// [`OpenSequence::saved_position`].
    LoadSavedPosition { index: u16 },
    /// Make `page` resident — from the loaded section window if it covers it,
    /// from the cache or a rebuild otherwise. Report through
    /// [`OpenSequence::section_loaded`].
    LoadSection { index: u16, chapter: u16, page: u16 },
    /// Point the global state file at the book now open. Report through
    /// [`OpenSequence::pointer_stored`].
    StorePointer(PersistedAppState),
    /// Announce the open as one event, carrying the landing position when the
    /// open resolved one. Report through [`OpenSequence::announced`].
    ///
    /// Every transaction reaches here, and every one of them sends: the app
    /// clamps its navigation against the counts the event carries and clears
    /// its open lock on the event itself. Whether the frame is worth repainting
    /// is decided by the app, in `app_core::loaded_repaints`.
    Announce { book_id: u32, position: Option<u32> },
    /// Nothing left to do.
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenPhase {
    CloseOut(PersistedAppState),
    Stage,
    LoadSaved,
    LoadSection,
    StorePointer,
    Announce,
    Refuse,
    Done,
}

/// A book-open (or section-extend) transaction, as the ordered card work it
/// owes.
///
/// The policy is strict on purpose, and the strictness is what removes the need
/// to queue partially finished switches: the reader either completes the move or
/// stays wholly on the book it started from. There is never a half-applied
/// switch for a later command to reconcile, which is why the pending-write state
/// can stay a single latest-value slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenSequence {
    phase: OpenPhase,
    book_id: u32,
    index: u16,
    chapter: u16,
    page: u16,
    type_settings: TypeSettings,
    portrait: bool,
    /// Set only for an open that changes books; also the base record the global
    /// pointer is built from, so device-wide reader settings carry across the
    /// switch instead of resetting to defaults.
    previous: Option<PersistedAppState>,
    /// A bare selection (chapter 0, page 0) of a book, which resumes from that
    /// book's own saved position. Extends never resume.
    resumable: bool,
    resumed: bool,
}

impl OpenSequence {
    /// Begins the transaction `command` describes, or returns `None` when the
    /// request lost to a newer one — a stale open has touched nothing, so there
    /// is no sequence to run and nothing to undo.
    pub fn begin(
        command: &StorageCommand,
        latest_request_id: u32,
        catalog_epoch: u32,
    ) -> Option<Self> {
        // A catalog index is only a book inside the catalog that produced it.
        // An open that says which catalog that was is refused when this is not
        // it, and refused rather than skipped: the app is already on the book
        // it asked for and waiting to hear, so silence would strand it.
        let fenced_out = matches!(
            *command,
            StorageCommand::OpenBook {
                catalog_epoch: Some(named),
                ..
            } if named != catalog_epoch
        );
        let (
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            type_settings,
            portrait,
            previous,
            resolve_place,
        ) = match *command {
            StorageCommand::OpenBook {
                request_id,
                book_id,
                index,
                chapter,
                target_pages,
                type_settings,
                portrait,
                previous,
                catalog_epoch: _,
                resolve_place,
            } => (
                request_id,
                book_id,
                index,
                chapter,
                target_pages,
                type_settings,
                portrait,
                previous,
                resolve_place,
            ),
            // An extend stays inside the book already loaded and owes
            // nothing to any other, so it carries no departing state and
            // never resumes.
            StorageCommand::ExtendSection {
                request_id,
                book_id,
                index,
                chapter,
                target_pages,
                type_settings,
                portrait,
            } => (
                request_id,
                book_id,
                index,
                chapter,
                target_pages,
                type_settings,
                portrait,
                None,
                false,
            ),
            _ => return None,
        };
        if request_id != latest_request_id {
            return None;
        }
        // An open resolves the stored place when it says to, and when the
        // app has no page to offer. The second is the book switch and the
        // cold start; the first is the typography change, where the app has a
        // page and the page means nothing.
        let resumable = matches!(command, StorageCommand::OpenBook { .. })
            && (resolve_place || (chapter == 0 && target_pages == 0));
        Some(Self {
            phase: match (fenced_out, previous) {
                // Nothing has been touched yet, so the refusal is clean: the
                // departing book keeps its position and the reader goes back
                // to it.
                (true, _) => OpenPhase::Refuse,
                (false, Some(previous)) => OpenPhase::CloseOut(previous),
                (false, None) => OpenPhase::Stage,
            },
            book_id,
            index,
            chapter,
            page: target_pages,
            type_settings,
            portrait,
            previous,
            resumable,
            resumed: false,
        })
    }

    pub const fn next(&self) -> OpenAction {
        match self.phase {
            OpenPhase::CloseOut(previous) => OpenAction::CloseOutDeparting(previous),
            OpenPhase::Stage => OpenAction::StageBook {
                index: self.index,
                type_settings: self.type_settings,
                portrait: self.portrait,
            },
            OpenPhase::LoadSaved => OpenAction::LoadSavedPosition { index: self.index },
            OpenPhase::LoadSection => OpenAction::LoadSection {
                index: self.index,
                chapter: self.chapter,
                page: self.page,
            },
            OpenPhase::StorePointer => OpenAction::StorePointer(self.pointer_record()),
            OpenPhase::Announce => OpenAction::Announce {
                book_id: self.book_id,
                position: self.position(),
            },
            OpenPhase::Refuse => OpenAction::Refuse {
                book_id: self.book_id,
            },
            OpenPhase::Done => OpenAction::Done,
        }
    }

    /// The global state record for the book now open: this book, at the position
    /// the open landed on, over the settings the app was already carrying.
    ///
    /// Building it from defaults instead would quietly reset the reader's font
    /// and orientation on every book change, so the departing state is the base
    /// and only the three fields the open actually moved are replaced.
    const fn pointer_record(&self) -> PersistedAppState {
        let base = match self.previous {
            Some(previous) => previous,
            // Unreachable: the pointer only moves for an open that changes
            // books, which is the only case that carries departing state.
            None => PersistedAppState {
                book_id: self.book_id,
                chapter: self.chapter,
                screen: 0,
                shell_orientation: 0,
                reading_orientation: 0,
                refresh_policy: 0,
                font_size: 0,
                line_spacing: 0,
                font_weight: 0,
                font_family: 0,
                front_buttons: 0,
                source_hash: 0,
                source_size: 0,
            },
        };
        PersistedAppState {
            book_id: self.book_id,
            chapter: self.chapter,
            screen: match self.position() {
                Some(page) => page,
                // A book opened at its start is still the active book. The
                // pointer moves at page zero exactly as it does anywhere else.
                None => 0,
            },
            ..base
        }
    }

    /// The page the open resolved, when it resolved one. `None` leaves the app's
    /// own page standing, which is what an explicit page request or an extend
    /// wants.
    const fn position(&self) -> Option<u32> {
        if self.resumed {
            Some(self.page as u32)
        } else {
            None
        }
    }

    /// Whether the departing book's position reached its own file.
    pub fn departing_stored(&mut self, stored: bool) {
        self.phase = if stored {
            OpenPhase::Stage
        } else {
            OpenPhase::Refuse
        };
    }

    /// The catalog entry is staged and the layout adopted.
    pub fn staged(&mut self) {
        self.phase = if self.resumable {
            OpenPhase::LoadSaved
        } else {
            OpenPhase::LoadSection
        };
    }

    /// The book's own saved place, if it had a usable one.
    ///
    /// Adopted whatever it says, including the start of the book. The page
    /// this open carried may have been counted under another layout, in which
    /// case it names nothing here, so "the same as the request" is not a
    /// reason to keep the request.
    pub fn saved_position(&mut self, position: Option<(u16, u32)>) {
        if let Some((chapter, screen)) = position {
            self.chapter = chapter;
            self.page = screen.min(u16::MAX as u32) as u16;
            self.resumed = chapter > 0 || screen > 0;
        }
        self.phase = OpenPhase::LoadSection;
    }

    /// The page a stored place resolved to, once the book was paginated for
    /// this open's layout.
    ///
    /// Separate from [`saved_position`](Self::saved_position) because the two
    /// happen at different times and cannot be merged: a place names content,
    /// and the page that content falls on exists only after the pagination
    /// this open builds.
    pub fn resolve_place(&mut self, page: u32) {
        self.page = page.min(u16::MAX as u32) as u16;
        self.resumed |= self.page > 0 || self.chapter > 0;
    }

    /// The section covering the target page is resident.
    pub fn section_loaded(&mut self) {
        self.phase = if self.previous.is_some() {
            OpenPhase::StorePointer
        } else {
            OpenPhase::Announce
        };
    }

    /// Whether the global pointer reached the card.
    ///
    /// A failed pointer write is recoverable and deliberately not fatal: the
    /// book is open and readable, and only a reboot before the retry would
    /// return to the previous one. So the record is left owed and the open is
    /// announced regardless.
    pub fn pointer_stored(&mut self, _stored: bool) {
        self.phase = OpenPhase::Announce;
    }

    /// The `Loaded` event went out.
    pub fn announced(&mut self) {
        self.phase = OpenPhase::Done;
    }

    /// The refusal event went out.
    pub fn refused(&mut self) {
        self.phase = OpenPhase::Done;
    }

    /// The page this transaction is targeting, after any resume.
    pub const fn target_page(&self) -> u16 {
        self.page
    }

    /// The chapter this transaction is targeting, after any resume.
    pub const fn target_chapter(&self) -> u16 {
        self.chapter
    }

    pub const fn resumed(&self) -> bool {
        self.resumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{book_open_outcome, BookOpenOutcome, ReaderSource};
    use display::font::{FontFamily, FontSize, FontWeight, LineSpacing};

    const SETTINGS: TypeSettings = TypeSettings {
        size: FontSize::Medium,
        spacing: LineSpacing::Normal,
        weight: FontWeight::Normal,
        family: FontFamily::Literata,
    };

    fn persisted(book_id: u32, chapter: u16, screen: u32) -> PersistedAppState {
        PersistedAppState {
            book_id,
            chapter,
            screen,
            shell_orientation: 1,
            reading_orientation: 2,
            refresh_policy: 3,
            font_size: 4,
            line_spacing: 5,
            font_weight: 6,
            font_family: 7,
            front_buttons: 8,
            source_hash: 0xabcd_0000 | book_id,
            source_size: 4096 + book_id,
        }
    }

    fn open(
        book_id: u32,
        chapter: u16,
        page: u16,
        previous: Option<PersistedAppState>,
    ) -> StorageCommand {
        StorageCommand::OpenBook {
            request_id: 7,
            book_id,
            index: ReaderSource::from_book_id(book_id).sd_index().unwrap(),
            chapter,
            catalog_epoch: None,
            target_pages: page,
            type_settings: SETTINGS,
            portrait: false,
            previous,
            resolve_place: false,
        }
    }

    fn book_id_of(command: &StorageCommand) -> u32 {
        match *command {
            StorageCommand::OpenBook { book_id, .. }
            | StorageCommand::ExtendSection { book_id, .. } => book_id,
            _ => unreachable!("only open transactions are driven here"),
        }
    }

    fn extend(book_id: u32, chapter: u16, page: u16) -> StorageCommand {
        StorageCommand::ExtendSection {
            request_id: 7,
            book_id,
            index: ReaderSource::from_book_id(book_id).sd_index().unwrap(),
            chapter,
            target_pages: page,
            type_settings: SETTINGS,
            portrait: false,
        }
    }

    /// A card the sequences can be driven against: one position file per book
    /// plus the single global state slot, with any individual write refusable.
    ///
    /// Modelling the two as separate storage is the point. The whole reason a
    /// book's position lives in its own file is that the global slot names one
    /// book at a time, so a stale global record could hand one book's page to
    /// another; a model that collapsed them could not show that.
    #[derive(Clone, Copy, Debug, Default)]
    struct Card {
        positions: [Option<PersistedAppState>; 8],
        global: Option<PersistedAppState>,
        refuse_position_write: bool,
        refuse_global_write: bool,
    }

    impl Card {
        fn slot(book_id: u32) -> usize {
            ReaderSource::from_book_id(book_id).sd_index().unwrap_or(0) as usize % 8
        }

        fn store_position(&mut self, record: PersistedAppState) -> bool {
            if self.refuse_position_write {
                return false;
            }
            self.positions[Self::slot(record.book_id)] = Some(record);
            true
        }

        fn store_global(&mut self, record: PersistedAppState) -> bool {
            if self.refuse_global_write {
                return false;
            }
            self.global = Some(record);
            true
        }

        fn position_of(&self, book_id: u32) -> Option<(u16, u32)> {
            self.positions[Self::slot(book_id)].map(|record| (record.chapter, record.screen))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        CloseOut(u32),
        Stage(u16),
        LoadSaved(u16),
        LoadSection {
            chapter: u16,
            page: u16,
        },
        StorePointer {
            book_id: u32,
            screen: u32,
        },
        Announce {
            book_id: u32,
            position: Option<u32>,
        },
        /// The `Loaded` event leaving the storage task, and what it says about
        /// the text. Separate from `Announce`, which is only the phase running.
        Loaded {
            book_id: u32,
            text_replaced: bool,
        },
        Refuse(u32),
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct Trace {
        steps: [Option<Step>; 12],
        len: usize,
    }

    impl Trace {
        /// The bound is also the termination guard. Every action but `Done`
        /// pushes exactly one step, so a phase that failed to advance shows up
        /// here as an overflow rather than as a firmware hang — which is what a
        /// missing advance would be on device, inside the loop that owns the
        /// card.
        fn push(&mut self, step: Step) {
            assert!(self.len < self.steps.len(), "trace overflow: {:?}", step);
            self.steps[self.len] = Some(step);
            self.len += 1;
        }

        fn steps(&self) -> impl Iterator<Item = Step> + '_ {
            self.steps[..self.len].iter().filter_map(|step| *step)
        }

        fn contains(&self, step: Step) -> bool {
            self.steps().any(|seen| seen == step)
        }

        fn position_of(&self, wanted: impl Fn(Step) -> bool) -> Option<usize> {
            self.steps().position(wanted)
        }
    }

    /// Runs a whole book-open transaction against the card model, exactly as the
    /// display task drives it, with the loaded section window not covering the
    /// target page. [`run_open_covering`] takes the other half.
    fn run_open(card: &mut Card, command: &StorageCommand, latest_request_id: u32) -> Trace {
        run_open_covering(card, command, latest_request_id, false)
    }

    /// `ram_hit` stands in for the loaded section window already covering the
    /// target page: it changes how the firmware makes the page resident, not the
    /// transaction around it, so the model just accepts — and it is what the
    /// closing event reports about the text, which the trace records.
    fn run_open_covering(
        card: &mut Card,
        command: &StorageCommand,
        latest_request_id: u32,
        ram_hit: bool,
    ) -> Trace {
        run_open_at(card, command, latest_request_id, ram_hit, CATALOG)
    }

    /// The catalog these traces run against. Named so a fenced open can be
    /// given a different one.
    const CATALOG: u32 = 4;

    /// An open of a row resolved in `catalog`, which is what a Library press
    /// produces once the storage task has looked the row up.
    fn row_open(book_id: u32, catalog: u32) -> StorageCommand {
        StorageCommand::OpenBook {
            request_id: 7,
            book_id,
            index: ReaderSource::from_book_id(book_id).sd_index().unwrap(),
            chapter: 0,
            target_pages: 0,
            catalog_epoch: Some(catalog),
            type_settings: TypeSettings::default(),
            portrait: false,
            previous: None,
            resolve_place: false,
        }
    }

    fn run_open_at(
        card: &mut Card,
        command: &StorageCommand,
        latest_request_id: u32,
        ram_hit: bool,
        catalog_epoch: u32,
    ) -> Trace {
        let mut trace = Trace::default();
        // As in the firmware: `None` until a section load is reached at all, so
        // a refused transaction never claims to have read from RAM.
        let mut section_loaded = None;
        let Some(mut sequence) = OpenSequence::begin(command, latest_request_id, catalog_epoch)
        else {
            return trace;
        };
        loop {
            match sequence.next() {
                OpenAction::CloseOutDeparting(previous) => {
                    trace.push(Step::CloseOut(previous.book_id));
                    let stored = card.store_position(previous);
                    sequence.departing_stored(stored);
                }
                OpenAction::Refuse { book_id } => {
                    trace.push(Step::Refuse(book_id));
                    sequence.refused();
                }
                OpenAction::StageBook { index, .. } => {
                    trace.push(Step::Stage(index));
                    sequence.staged();
                }
                OpenAction::LoadSavedPosition { index } => {
                    trace.push(Step::LoadSaved(index));
                    let book_id = ReaderSource::sd(index).book_id();
                    sequence.saved_position(card.position_of(book_id));
                }
                OpenAction::LoadSection { chapter, page, .. } => {
                    trace.push(Step::LoadSection { chapter, page });
                    section_loaded = Some(ram_hit);
                    sequence.section_loaded();
                }
                OpenAction::StorePointer(record) => {
                    trace.push(Step::StorePointer {
                        book_id: record.book_id,
                        screen: record.screen,
                    });
                    let stored = card.store_global(record);
                    sequence.pointer_stored(stored);
                }
                OpenAction::Announce { book_id, position } => {
                    trace.push(Step::Announce { book_id, position });
                    // Unconditional, as in the firmware. `ram_hit` decides what
                    // the event says about the text, never whether it is sent.
                    trace.push(Step::Loaded {
                        book_id,
                        text_replaced: !matches!(section_loaded, Some(true)),
                    });
                    sequence.announced();
                }
                OpenAction::Done => return trace,
            }
        }
    }

    #[test]
    fn a_stale_open_touches_nothing() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 9);
        assert_eq!(trace.len, 0);
        assert!(card.global.is_none());
        assert!(card.positions.iter().all(Option::is_none));
    }

    #[test]
    fn a_reopen_of_the_active_book_never_moves_the_pointer() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(2, 3, 40, None), 7);
        assert!(!trace.contains(Step::CloseOut(2)));
        assert!(trace
            .position_of(|step| matches!(step, Step::StorePointer { .. }))
            .is_none());
        assert!(card.global.is_none());
    }

    // Invariant: every transaction answers the app, RAM hit or not, extend or
    // open. The app clamps its navigation against the counts the event carries
    // and clears its open lock on the event itself, so a transaction that goes
    // quiet is not a saved refresh -- it is a page turn walking into a stale
    // bound, or an open lock nothing will ever lift. What the load did to the
    // text rides in the event instead, and the app spends the refresh or not.
    #[test]
    fn every_transaction_answers_the_app() {
        for (name, command, ram_hit) in [
            // Re-entering Reading on the book still resident: `previous: None`,
            // no relocation, nothing read -- the exact shape of a page turn.
            ("resident reopen", open(2, 3, 40, None), true),
            ("bare reopen", open(2, 0, 0, None), true),
            (
                "switch from RAM",
                open(3, 5, 12, Some(persisted(2, 4, 90))),
                true,
            ),
            (
                "switch from the card",
                open(3, 5, 12, Some(persisted(2, 4, 90))),
                false,
            ),
            ("page turn from RAM", extend(2, 3, 41), true),
            ("page turn from the card", extend(2, 3, 41), false),
        ] {
            let mut card = Card::default();
            let book_id = book_id_of(&command);
            let trace = run_open_covering(&mut card, &command, 7, ram_hit);
            assert!(
                trace.contains(Step::Loaded {
                    book_id,
                    // A RAM hit is the only load that leaves the text alone.
                    text_replaced: !ram_hit,
                }),
                "{name} must answer: {trace:?}"
            );
        }
    }

    // Invariant: switching away preserves the previous book's position.
    #[test]
    fn switching_away_writes_the_departing_book_before_the_slot_moves() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        let close_out = trace
            .position_of(|step| step == Step::CloseOut(2))
            .expect("the departing book is closed out");
        let stage = trace
            .position_of(|step| matches!(step, Step::Stage(_)))
            .expect("the incoming book is staged");
        assert!(
            close_out < stage,
            "the departing write needs its own catalog slot: {:?}",
            trace
        );
        assert_eq!(card.position_of(2), Some((4, 90)));
    }

    // Invariant: a newly opened book becomes active even at page zero.
    #[test]
    fn opening_at_page_zero_still_moves_the_pointer() {
        let mut card = Card::default();
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::StorePointer {
            book_id: 3,
            screen: 0
        }));
        let global = card
            .global
            .expect("the pointer names the newly opened book");
        assert_eq!(global.book_id, 3);
        assert_eq!(global.screen, 0);
    }

    #[test]
    fn moving_the_pointer_carries_the_readers_settings_across_the_switch() {
        let mut card = Card::default();
        let departing = persisted(2, 4, 90);
        run_open(&mut card, &open(3, 0, 0, Some(departing)), 7);
        let global = card.global.expect("the pointer moved");
        // Only the book and where it landed change; the device-wide reader
        // settings are the ones the app was already carrying.
        assert_eq!(
            global,
            PersistedAppState {
                book_id: 3,
                chapter: 0,
                screen: 0,
                ..departing
            }
        );
    }

    #[test]
    fn a_bare_selection_resumes_from_the_books_own_position() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::LoadSection {
            chapter: 11,
            page: 250
        }));
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: Some(250)
        }));
        assert_eq!(
            card.global.map(|record| (record.chapter, record.screen)),
            Some((11, 250))
        );
    }

    #[test]
    fn an_explicit_page_request_is_not_overridden_by_the_saved_position() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &open(3, 2, 30, Some(persisted(2, 4, 90))), 7);
        assert!(!trace.contains(Step::LoadSaved(1)));
        assert!(trace.contains(Step::LoadSection {
            chapter: 2,
            page: 30
        }));
        // The app asked for this page, so it already has it; the event must not
        // hand it back as a landing position and re-render.
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: None
        }));
    }

    #[test]
    fn a_saved_start_of_book_is_not_treated_as_a_resume() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 0, 0)));
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: None
        }));
    }

    // Invariant: opening produces one final render at the restored page.
    #[test]
    fn an_open_announces_exactly_once() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        let announcements = trace
            .steps()
            .filter(|step| matches!(step, Step::Announce { .. }))
            .count();
        assert_eq!(announcements, 1, "{:?}", trace);
    }

    // Invariant: interrupted writes never substitute one book's position for
    // another's.
    #[test]
    fn a_row_open_against_a_rebuilt_catalog_is_refused_rather_than_opened() {
        let mut card = Card::default();

        // The row was resolved in CATALOG and the open arrives after a
        // rebuild, so the number it carries names some other book now.
        let trace = run_open_at(
            &mut card,
            &row_open(3, CATALOG),
            7,
            false,
            CATALOG.wrapping_add(1),
        );

        assert!(trace.contains(Step::Refuse(3)));
        assert!(
            trace
                .position_of(|step| matches!(step, Step::Stage(_)))
                .is_none(),
            "a row number from another catalog may not open anything: {:?}",
            trace
        );
        assert!(trace
            .position_of(|step| matches!(step, Step::Announce { .. }))
            .is_none());
    }

    /// The fence is the catalog the row was resolved in, not any catalog: the
    /// same open against the catalog it named stages and announces as usual.
    #[test]
    fn a_row_open_against_its_own_catalog_opens() {
        let mut card = Card::default();
        let trace = run_open_at(&mut card, &row_open(3, CATALOG), 7, false, CATALOG);
        assert!(trace
            .position_of(|step| matches!(step, Step::Stage(_)))
            .is_some());
        assert!(!trace.contains(Step::Refuse(3)));
    }

    /// Every other open carries no catalog, and a rebuild cannot refuse it.
    /// Fencing those here would refuse a boot restore whose `Scanned` the app
    /// has not folded yet; the index they carry is Milestone 4's problem.
    #[test]
    fn an_unfenced_open_is_indifferent_to_the_catalog() {
        let mut card = Card::default();
        let trace = run_open_at(
            &mut card,
            &open(3, 0, 0, None),
            7,
            false,
            CATALOG.wrapping_add(9),
        );
        assert!(trace
            .position_of(|step| matches!(step, Step::Stage(_)))
            .is_some());
    }

    #[test]
    fn a_refused_departing_write_abandons_the_whole_switch() {
        let mut card = Card {
            refuse_position_write: true,
            ..Card::default()
        };
        assert!(card.store_global(persisted(2, 4, 90)));
        card.refuse_global_write = false;

        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 91))), 7);

        assert!(trace.contains(Step::Refuse(3)));
        assert!(
            trace
                .position_of(|step| matches!(step, Step::Stage(_)))
                .is_none(),
            "nothing may be opened once the departing page is lost: {:?}",
            trace
        );
        assert!(trace
            .position_of(|step| matches!(step, Step::Announce { .. }))
            .is_none());
        // The global slot still names the book the reader is actually in.
        assert_eq!(card.global.map(|record| record.book_id), Some(2));
        assert_eq!(
            book_open_outcome(false, false),
            BookOpenOutcome::KeptBookPositionUnwritten
        );
    }

    #[test]
    fn a_refused_pointer_write_still_opens_and_announces_the_book() {
        let mut card = Card {
            refuse_global_write: true,
            ..Card::default()
        };
        let trace = run_open(&mut card, &open(3, 0, 0, Some(persisted(2, 4, 90))), 7);
        assert!(trace.contains(Step::Announce {
            book_id: 3,
            position: None
        }));
        // The departing page is on the card either way, which is what makes the
        // failure recoverable rather than lossy.
        assert_eq!(card.position_of(2), Some((4, 90)));
        assert!(card.global.is_none());
        assert_eq!(
            book_open_outcome(true, false),
            BookOpenOutcome::OpenedPointerOwed
        );
    }

    #[test]
    fn an_extend_owes_nothing_to_any_other_book() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &extend(3, 2, 60), 7);
        assert!(trace
            .position_of(|step| matches!(step, Step::CloseOut(_)))
            .is_none());
        assert!(trace
            .position_of(|step| matches!(step, Step::LoadSaved(_)))
            .is_none());
        assert!(trace
            .position_of(|step| matches!(step, Step::StorePointer { .. }))
            .is_none());
        assert!(trace.contains(Step::LoadSection {
            chapter: 2,
            page: 60
        }));
        assert!(card.global.is_none());
    }

    #[test]
    fn an_extend_at_chapter_zero_page_zero_does_not_resume() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, 250)));
        let trace = run_open(&mut card, &extend(3, 0, 0), 7);
        assert!(trace.contains(Step::LoadSection {
            chapter: 0,
            page: 0
        }));
    }

    #[test]
    fn a_deep_saved_position_is_clamped_to_the_page_field() {
        let mut card = Card::default();
        assert!(card.store_position(persisted(3, 11, u32::MAX)));
        let mut sequence =
            OpenSequence::begin(&open(3, 0, 0, None), 7, CATALOG).expect("fresh request");
        sequence.staged();
        sequence.saved_position(card.position_of(3));
        assert_eq!(sequence.target_page(), u16::MAX);
        assert_eq!(sequence.target_chapter(), 11);
        assert!(sequence.resumed());
    }

    // The storage queue the pre-sleep drain works against.
    #[derive(Clone, Copy, Debug, Default)]
    struct Queue {
        slots: [Option<StorageCommand>; 4],
        head: usize,
        len: usize,
    }

    impl Queue {
        const CAPACITY: usize = 4;

        fn push(&mut self, command: StorageCommand) -> bool {
            if self.len == Self::CAPACITY {
                return false;
            }
            self.slots[(self.head + self.len) % Self::CAPACITY] = Some(command);
            self.len += 1;
            true
        }

        fn pop(&mut self) -> Option<StorageCommand> {
            let command = self.slots[self.head].take()?;
            self.head = (self.head + 1) % Self::CAPACITY;
            self.len -= 1;
            Some(command)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SleepOutcome {
        Slept,
        Refused(SleepRefusal),
    }

    fn run_sleep(queue: &mut Queue, progress_lands: bool) -> (SleepOutcome, usize) {
        run_sleep_with(queue, progress_lands, None)
    }

    /// Drives a whole sleep transition the way the display task does, returning
    /// what the panel did and how many queued commands were applied on the way.
    ///
    /// `refill` stands in for a producer enqueuing one more command in the window
    /// between the drain taking a command off the queue and putting it back.
    /// That refill is the *only* thing that can make the put-back fail, so it is
    /// the only honest way to reach the lost-upload path: forcing the failure
    /// without it would model a queue that cannot occur — one short of full —
    /// and would validate accounting rather than behaviour.
    fn run_sleep_with(
        queue: &mut Queue,
        progress_lands: bool,
        mut refill: Option<StorageCommand>,
    ) -> (SleepOutcome, usize) {
        let mut sequence = SleepSequence::new(Queue::CAPACITY);
        let mut applied = 0;
        // A phase that fails to advance would spin here exactly as it would in
        // the task; fail the test instead of hanging it.
        for _ in 0..Queue::CAPACITY * 4 {
            match sequence.next() {
                SleepAction::TakeQueued => match queue.pop() {
                    None => sequence.queue_empty(),
                    Some(command) => match sequence.drained(&command) {
                        Drained::Apply => {
                            applied += 1;
                            sequence.applied();
                        }
                        Drained::RequeueAndRefuse => {
                            if let Some(refill) = refill.take() {
                                assert!(queue.push(refill), "the producer wins the freed slot");
                            }
                            let restored = queue.push(command);
                            sequence.requeued(restored);
                        }
                    },
                },
                SleepAction::FlushProgress => sequence.flushed(progress_lands),
                SleepAction::Refuse(refusal) => return (SleepOutcome::Refused(refusal), applied),
                SleepAction::Proceed => return (SleepOutcome::Slept, applied),
            }
        }
        panic!("the sleep sequence never resolved");
    }

    #[test]
    fn an_empty_queue_sleeps_straight_away() {
        let mut queue = Queue::default();
        assert_eq!(run_sleep(&mut queue, true), (SleepOutcome::Slept, 0));
    }

    #[test]
    fn queued_work_is_applied_before_the_panel_goes_down() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, 10))));
        assert!(queue.push(open(4, 0, 0, Some(persisted(3, 1, 10)))));
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Slept);
        assert_eq!(
            applied, 2,
            "deep sleep is terminal; nothing may be left over"
        );
        assert_eq!(queue.len, 0);
    }

    #[test]
    fn a_full_queue_is_drained_within_its_own_depth() {
        let mut queue = Queue::default();
        for page in 0..Queue::CAPACITY as u32 {
            assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, page))));
        }
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Slept);
        assert_eq!(applied, Queue::CAPACITY);
        assert_eq!(queue.len, 0);
    }

    #[test]
    fn a_progress_record_that_will_not_land_keeps_the_panel_up() {
        let mut queue = Queue::default();
        assert_eq!(
            run_sleep(&mut queue, false),
            (SleepOutcome::Refused(SleepRefusal::ProgressUnwritten), 0)
        );
    }

    // Regression: the drain must not answer an upload request. Its arm in the
    // storage handler is a no-op, so applying it here would discard the only
    // signal that starts the writer, leaving the browser waiting on a session
    // that never opens with the upload flag still set.
    #[test]
    fn a_queued_upload_is_put_back_and_the_sleep_refused() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::ReceiveUpload));
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Refused(SleepRefusal::UploadQueued));
        assert_eq!(applied, 0);
        assert_eq!(queue.len, 1, "the request has to survive the refusal");
        assert_eq!(queue.pop(), Some(StorageCommand::ReceiveUpload));
    }

    #[test]
    fn work_queued_ahead_of_an_upload_is_still_applied_before_the_refusal() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, 10))));
        assert!(queue.push(StorageCommand::ReceiveUpload));
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Refused(SleepRefusal::UploadQueued));
        assert_eq!(applied, 1);
        assert_eq!(queue.pop(), Some(StorageCommand::ReceiveUpload));
    }

    /// A full queue at the moment of a refill, with the upload at its head.
    fn queue_refilled_behind_an_upload() -> (Queue, StorageCommand) {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::ReceiveUpload));
        for page in 0..Queue::CAPACITY as u32 - 1 {
            assert!(queue.push(StorageCommand::StoreProgress(persisted(3, 1, page))));
        }
        // What the producer slips into the slot the upload vacates. An open is
        // the costly thing to lose this way: it carries the departing book's
        // only close-out position and nothing reissues it.
        (queue, open(4, 0, 0, Some(persisted(3, 1, 40))))
    }

    // Regression: an upload that cannot go back means a producer refilled the
    // queue behind it, so the channel holds a whole budget's worth of accepted
    // work. Draining on would spend the remaining budget on a queue that grew,
    // and hand what it could not reach to a terminal sleep.
    #[test]
    fn an_upload_that_cannot_be_put_back_leaves_the_refilled_queue_intact() {
        let (mut queue, refill) = queue_refilled_behind_an_upload();
        let (outcome, applied) = run_sleep_with(&mut queue, true, Some(refill));
        assert_eq!(outcome, SleepOutcome::Refused(SleepRefusal::UploadLost));
        assert_eq!(applied, 0);
        assert_eq!(
            queue.len,
            Queue::CAPACITY,
            "every accepted command must survive the refusal"
        );
    }

    #[test]
    fn the_retry_after_a_lost_upload_drains_everything_and_sleeps() {
        let (mut queue, refill) = queue_refilled_behind_an_upload();
        assert_eq!(
            run_sleep_with(&mut queue, true, Some(refill)).0,
            SleepOutcome::Refused(SleepRefusal::UploadLost)
        );
        // The power task's idle clock re-requests sleep. No upload is queued
        // this time — it is the thing that was lost — so nothing defers the
        // drain and the whole backlog lands before the panel goes down.
        let (outcome, applied) = run_sleep(&mut queue, true);
        assert_eq!(outcome, SleepOutcome::Slept);
        assert_eq!(applied, Queue::CAPACITY);
        assert_eq!(queue.len, 0);
    }

    #[test]
    fn a_requeued_upload_is_answered_by_the_loop_on_the_next_pass() {
        let mut queue = Queue::default();
        assert!(queue.push(StorageCommand::ReceiveUpload));
        assert_eq!(
            run_sleep(&mut queue, true),
            (SleepOutcome::Refused(SleepRefusal::UploadQueued), 0)
        );
        // The loop takes it next, and the upload arm — not the handler — is
        // what answers it.
        let command = queue.pop().expect("still queued");
        assert_eq!(
            loop_arm(&command, SyncSession::Loaned),
            LoopArm::UploadSession
        );
    }

    #[test]
    fn an_upload_outside_a_session_is_refused_rather_than_entered() {
        assert_eq!(
            loop_arm(&StorageCommand::ReceiveUpload, SyncSession::Idle),
            LoopArm::RefusedUpload
        );
    }

    const FIRST_OPEN_AT: fn(u32) -> BuildPhase =
        |requested_page| BuildPhase::FirstOpen { requested_page };
    const BACKGROUND: BuildPhase = BuildPhase::Background { slice_ms: 400 };

    // Invariant: the walk the reader waits on stops as soon as it owes nothing.
    #[test]
    fn a_first_open_suspends_once_the_requested_page_is_covered() {
        // Opening at the start: one section of pages is already enough.
        assert!(FIRST_OPEN_AT(0).suspend_here(12, 1, 0, true));
        // Opening deep: page 900 is not covered by 400 pages, and no amount of
        // elapsed time makes it so — the page only exists once it is laid out.
        assert!(!FIRST_OPEN_AT(900).suspend_here(400, 40, 60_000, true));
        assert!(FIRST_OPEN_AT(900).suspend_here(901, 90, 0, true));
    }

    #[test]
    fn a_first_open_with_no_pages_yet_keeps_walking() {
        // A spine item that contributed no section (navigation, empty body)
        // leaves the counters at zero; suspending there would publish a book
        // with nothing in it.
        assert!(!FIRST_OPEN_AT(0).suspend_here(0, 0, 0, true));
    }

    #[test]
    fn a_background_step_suspends_on_its_slice_budget_alone() {
        assert!(!BACKGROUND.suspend_here(1200, 100, 399, true));
        assert!(BACKGROUND.suspend_here(1200, 100, 400, true));
        // The page counters say nothing to a background step: it owes no page.
        assert!(BACKGROUND.suspend_here(0, 0, 400, true));
    }

    // Regression: suspending at the end of the spine would hand the caller a
    // continuation with no work, republish the book, and leave a complete
    // cache flagged partial.
    #[test]
    fn neither_phase_suspends_with_no_spine_left() {
        assert!(!FIRST_OPEN_AT(0).suspend_here(12, 1, 0, false));
        assert!(!BACKGROUND.suspend_here(1200, 100, 60_000, false));
    }

    #[test]
    fn a_complete_index_is_usable_with_or_without_a_walk() {
        assert!(partial_index_is_usable(false, false));
        assert!(partial_index_is_usable(false, true));
    }

    #[test]
    fn an_unfinished_index_is_usable_while_its_own_walk_runs() {
        // The common case during a progressive open: every section crossing
        // arrives as an extend and must not restart the build under itself.
        assert!(partial_index_is_usable(true, true));
    }

    // Invariant: a book is never left capped at a page count nothing will
    // raise. This is the sleep/reboot case — the walk is gone, and the reducer
    // clamps the reader to the advertised count, so no input can provoke the
    // rebuild that would fix it.
    #[test]
    fn an_unfinished_index_nobody_is_building_is_refused() {
        assert!(!partial_index_is_usable(true, false));
    }

    #[test]
    fn a_finished_background_build_always_announces() {
        assert!(background_announce(true, 0, 1200));
    }

    #[test]
    fn a_reader_far_behind_the_frontier_is_not_repainted() {
        // The cost of announcing is a full panel refresh, and this reader
        // gains nothing from it: their next page is already built.
        assert!(!background_announce(false, 3, 400));
    }

    // Invariant: a reader who has caught up with the frontier is told the book
    // grew. The reducer clamps the page to the advertised count, so without
    // this the next-page button silently does nothing.
    #[test]
    fn a_reader_at_the_frontier_is_told_the_book_grew() {
        assert!(background_announce(false, 399, 400));
        // One page short of the frontier still has somewhere to turn to.
        assert!(!background_announce(false, 398, 400));
    }

    // Invariant: the same dead next-page button, reached the other way. The
    // step grew the book to 460 pages and then broke; if the walk simply
    // vanishes at 400, the reader is pinned below pages that exist and are
    // readable, with no input that can dislodge them short of leaving the book.
    #[test]
    fn a_stopped_step_that_grew_the_book_still_frees_the_frontier() {
        assert!(stopped_announce(400, 460, 399));
    }

    // A step that built nothing has nothing to announce — a same-count Loaded
    // would spend a full panel refresh redrawing the same frontier. Which is
    // why silence here is only correct if the walk itself is kept: see
    // `a_step_that_never_began_is_kept_rather_than_announced` below.
    // Invariant: a step always takes at least one item, however large. Without
    // this a book whose every chapter exceeds the slice would never advance.
    #[test]
    fn a_step_always_walks_one_item_before_it_may_stop_short() {
        assert!(!BACKGROUND.suspend_before(0, 5_000_000, 0));
        assert!(!BACKGROUND.suspend_before(9_999, 5_000_000, 0));
    }

    // Invariant: the measured failure. Two small items leave the step under
    // budget, then a 58 KB chapter -- ~2900 ms by the fitted rate -- would
    // overshoot to 3300 ms with no way for a page turn to preempt it.
    #[test]
    fn a_chapter_that_would_blow_the_slice_waits_for_the_next_step() {
        assert!(BACKGROUND.suspend_before(340, 58_245, 2));
    }

    #[test]
    fn a_small_item_still_rides_the_current_step() {
        // 606 B of front matter is ~30 ms; batching these is the whole point
        // of a slice budget, since each suspend costs a zip and OPF re-parse.
        assert!(!BACKGROUND.suspend_before(340, 606, 2));
    }

    // A first open must reach the page it was asked for. Stopping short would
    // publish a book that cannot show the page the reader opened it on.
    #[test]
    fn a_first_open_never_stops_short_of_its_requested_page() {
        let first = BuildPhase::FirstOpen { requested_page: 0 };
        assert!(!first.suspend_before(10_000, 5_000_000, 9));
    }

    #[test]
    fn a_stopped_step_that_built_nothing_stays_silent() {
        assert!(!stopped_announce(400, 400, 399));
    }

    // Invariant: the reader pinned at the frontier is never left with both
    // nothing announced and nothing running. Announcing is useless when no
    // pages were built, so the walk is what has to survive — otherwise Next at
    // the cap issues no command and nothing retries.
    #[test]
    fn a_step_that_never_began_is_kept_rather_than_announced() {
        assert!(!stopped_announce(400, 400, 399));
        assert!(background_retry_delay_ms(1) > 0);
    }

    #[test]
    fn the_retry_backoff_quadruples_to_its_ceiling() {
        assert_eq!(background_retry_delay_ms(1), 250);
        assert_eq!(background_retry_delay_ms(2), 1_000);
        assert_eq!(background_retry_delay_ms(3), 4_000);
        assert_eq!(background_retry_delay_ms(4), 16_000);
        assert_eq!(background_retry_delay_ms(5), BACKGROUND_RETRY_CEILING_MS);
    }

    // Invariant: the walk is never given up on, so the delay has to stay
    // bounded and finite however long the card stays away. u8 saturation is
    // reached after about a day of retrying.
    #[test]
    fn the_retry_backoff_never_runs_away() {
        assert_eq!(
            background_retry_delay_ms(u8::MAX),
            BACKGROUND_RETRY_CEILING_MS
        );
        // A step that ran is not a retry and must not be made to wait.
        assert_eq!(background_retry_delay_ms(0), 0);
    }

    #[test]
    fn a_stopped_step_does_not_repaint_a_reader_mid_book() {
        // Their next page was already built; the growth is theirs to discover
        // by turning pages, not worth a refresh.
        assert!(!stopped_announce(400, 460, 12));
    }

    #[test]
    fn a_reader_past_a_shrinking_frontier_still_announces() {
        // Not reachable today — the frontier only grows — but the comparison
        // is a saturating one either way, so a page beyond the advertised
        // count must not wrap into "far behind" and stay silent.
        assert!(background_announce(false, 500, 400));
        assert!(background_announce(false, u32::MAX, 400));
    }

    #[test]
    fn every_other_command_goes_to_the_storage_handler() {
        for command in [
            StorageCommand::LoadCatalogCache,
            StorageCommand::RefreshCatalog,
            StorageCommand::StoreProgress(persisted(3, 1, 10)),
            open(3, 0, 0, None),
            extend(3, 1, 20),
            StorageCommand::ForgetWifiCredentials,
        ] {
            assert_eq!(loop_arm(&command, SyncSession::Idle), LoopArm::Apply);
            assert_eq!(loop_arm(&command, SyncSession::Loaned), LoopArm::Apply);
        }
    }
}
