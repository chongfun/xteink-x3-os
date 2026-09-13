#![no_std]
#![forbid(unsafe_code)]

use display::font::{FontFamily, FontSize, FontWeight, LineSpacing, TypeSettings};
use display::{epd::RefreshMode, Rect};

/// Where the reader is in the library's folder tree.
pub mod browse;
/// The resistive button ladders, shared by the input task and the boot-time
/// recovery-combo check.
pub mod buttons;
/// The storage/display task's command loop, as sequences a host test can drive.
pub mod storage_loop;

pub const SETTINGS_ITEMS: u8 = 7;
pub const MAX_SD_CHAPTERS: usize = 128;
pub const FIRST_SD_BOOK_ID: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderSource {
    BuiltIn { book_id: u32 },
    Sd { index: u16 },
}

impl ReaderSource {
    pub fn from_book_id(book_id: u32) -> Self {
        if book_id >= FIRST_SD_BOOK_ID {
            Self::Sd {
                index: book_id
                    .saturating_sub(FIRST_SD_BOOK_ID)
                    .min(u16::MAX as u32) as u16,
            }
        } else {
            Self::BuiltIn { book_id }
        }
    }

    pub const fn sd(index: u16) -> Self {
        Self::Sd { index }
    }

    pub const fn book_id(self) -> u32 {
        match self {
            Self::BuiltIn { book_id } => book_id,
            Self::Sd { index } => FIRST_SD_BOOK_ID + index as u32,
        }
    }

    pub const fn sd_index(self) -> Option<u16> {
        match self {
            Self::BuiltIn { .. } => None,
            Self::Sd { index } => Some(index),
        }
    }

    pub const fn is_sd(self) -> bool {
        matches!(self, Self::Sd { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Power,
    Back,
    Confirm,
    Previous,
    Next,
    PagePrevious,
    PageNext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Sample {
        button: Option<Button>,
        aux_raw: u16,
        nav_raw: u16,
        page_raw: u16,
        battery_mv: u16,
        battery_percent: u8,
    },
}

impl InputEvent {
    pub const fn button(button: Button) -> Self {
        Self::Sample {
            button: Some(button),
            aux_raw: 2000,
            nav_raw: 0,
            page_raw: 0,
            battery_mv: 4000,
            battery_percent: 77,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderKind {
    Boot,
    Page,
    Battery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayOrientation {
    LandscapeButtonsBottom,
    LandscapeButtonsTop,
    PortraitButtonsLeft,
    PortraitButtonsRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppView {
    Home,
    Library,
    Reading,
    Chapters,
    Wireless,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HomeAction {
    Read,
    Files,
    Wireless,
    Settings,
}

/// Where the front page-turn pair sits. The front row is two pairs —
/// back/confirm and previous/next. `PagesLeft` exchanges the pairs whole,
/// keeping each pair's internal order, for readers whose thumb rests on
/// the other end of the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontButtons {
    PagesRight,
    PagesLeft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshPolicy {
    FastOnly,
    FullOnWake,
    FullEveryTen,
}

pub const DEFAULT_FULL_REFRESH_INTERVAL: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshPlanner {
    screen_on: bool,
    fast_refreshes: u8,
    last_request: Option<RenderRequest>,
    fast_refresh_enabled: bool,
    full_refresh_interval: u8,
    panel_shows_sleep_screen: bool,
}

impl Default for RefreshPlanner {
    fn default() -> Self {
        Self::new()
    }
}

impl RefreshPlanner {
    pub const fn new() -> Self {
        Self {
            screen_on: false,
            fast_refreshes: 0,
            last_request: None,
            fast_refresh_enabled: true,
            full_refresh_interval: DEFAULT_FULL_REFRESH_INTERVAL,
            panel_shows_sleep_screen: false,
        }
    }

    pub const fn with_fast_refresh_enabled(mut self, enabled: bool) -> Self {
        self.fast_refresh_enabled = enabled;
        self
    }

    /// Seeds a fresh planner with the knowledge that the panel already shows
    /// the sleep screen. Deep sleep is terminal — waking reboots the chip and
    /// builds a new planner — but the only deep-sleep entry path draws the
    /// sleep screen and waits for the panel to settle before cutting power,
    /// so on a deep-sleep wake the panel contents are known by construction
    /// and the first render can take the one-flicker clean instead of the
    /// multi-flash full waveform. Callers must gate the seed strictly on the
    /// deep-sleep wake cause *and* on a persisted record that the sleep
    /// frame actually settled: the sleep path still powers down when its
    /// final flush fails, and any other cold boot (battery pull, crash,
    /// software reset) leaves unknown pixels that only `Full` clears.
    pub const fn with_panel_shows_sleep_screen(mut self, shows_sleep_screen: bool) -> Self {
        self.panel_shows_sleep_screen = shows_sleep_screen;
        self
    }

    pub const fn screen_on(&self) -> bool {
        self.screen_on
    }

    pub const fn last_request(&self) -> Option<RenderRequest> {
        self.last_request
    }

    pub fn mode_for(&self, request: RenderRequest) -> RefreshMode {
        let Some(last) = self.last_request else {
            // Cold boot leaves unknown pixels on the panel; only the deep
            // full waveform reliably clears them. After a display sleep the
            // panel still shows the sleep screen this firmware drew, so the
            // one-flicker clean is enough to wake.
            return if self.fast_refresh_enabled && self.panel_shows_sleep_screen {
                RefreshMode::FastClean
            } else {
                RefreshMode::Full
            };
        };
        if !self.fast_refresh_enabled || !self.screen_on {
            return RefreshMode::Full;
        }
        // Context changes need ghost cleanup, but the panel state is known
        // (the frame just shown), so the one-flicker clean suffices and the
        // multi-flash full waveform stays reserved for boot and sleep.
        if last.kind == RenderKind::Boot
            || request.view != last.view
            || request.book_id != last.book_id
            || request.orientation != last.orientation
            // A type-settings change redraws whole text columns; the clean
            // pass avoids fast-diff ghosting across the page.
            || request.font_size != last.font_size
            || request.line_spacing != last.line_spacing
            || request.font_weight != last.font_weight
            || request.font_family != last.font_family
            || Self::needs_clean_library_refresh(request, last)
        {
            return RefreshMode::FastClean;
        }
        match request.refresh_policy {
            RefreshPolicy::FastOnly | RefreshPolicy::FullOnWake => RefreshMode::Fast,
            RefreshPolicy::FullEveryTen if self.fast_refreshes >= self.full_refresh_interval => {
                RefreshMode::FastClean
            }
            RefreshPolicy::FullEveryTen => RefreshMode::Fast,
        }
    }

    pub fn record_render(&mut self, request: RenderRequest, mode: RefreshMode) {
        self.screen_on = true;
        self.last_request = Some(request);
        self.panel_shows_sleep_screen = false;
        if mode == RefreshMode::Fast {
            self.fast_refreshes = self.fast_refreshes.saturating_add(1);
        } else {
            self.fast_refreshes = 0;
        }
    }

    /// Records a render the flush seam skipped because the frame already
    /// matched the glass.
    ///
    /// Deliberately not a `record_render` with a different mode: no waveform
    /// ran, so `fast_refreshes` must not move, or `FullEveryTen`'s cleans
    /// drift earlier for refreshes nothing drove. `screen_on` is left alone
    /// because the skip predicate already requires it.
    pub fn record_skipped_render(&mut self, request: RenderRequest) {
        self.last_request = Some(request);
        self.panel_shows_sleep_screen = false;
    }

    /// Records the panel powering down at the end of the sleep handshake.
    /// Clearing `last_request` is what makes the next render re-init the
    /// panel, so this must run whenever the panel actually slept — even if
    /// the sleep-frame flush failed. `panel_shows_sleep_screen` carries that
    /// flush outcome: `true` lets the wake render take the one-flicker
    /// clean, `false` (stale pixels under a failed flush) keeps the deep
    /// full waveform that unknown panel contents require.
    pub fn record_sleep(&mut self, panel_shows_sleep_screen: bool) {
        self.screen_on = false;
        self.fast_refreshes = 0;
        self.last_request = None;
        self.panel_shows_sleep_screen = panel_shows_sleep_screen;
    }

    /// Records a failed panel transition (SPI transfer or BUSY handshake).
    /// A flush or sleep that errored may have run partially, so the panel's
    /// RAM, waveform, and power state are all unknown: forget the screen
    /// contents so the next render re-inits the panel and pays the deep
    /// full waveform instead of fast-diffing against a frame that may never
    /// have landed. Configuration (fast-refresh enablement, interval)
    /// describes policy, not panel state, and survives.
    pub fn record_failure(&mut self) {
        self.screen_on = false;
        self.fast_refreshes = 0;
        self.last_request = None;
        self.panel_shows_sleep_screen = false;
    }

    fn needs_clean_library_refresh(request: RenderRequest, last: RenderRequest) -> bool {
        // Only the library list actually redraws when these move; other views
        // repaint identical pixels and can ride the partial.
        if request.view != AppView::Library {
            return false;
        }
        // The actions sheet is a bordered card drawn over the lower rows, and
        // every step of it uncovers or covers text: the card appearing and
        // going away, the key rail relabelling beside it, the footer turning
        // from the position line into the wait and then the note. A fast diff
        // leaves the rows underneath ghosted through it.
        request.library_count != last.library_count || request.library_menu != last.library_menu
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub kind: RenderKind,
    pub view: AppView,
    pub page: u32,
    pub page_count: u32,
    pub chapter: u16,
    pub selection: u16,
    pub book_id: u32,
    pub orientation: DisplayOrientation,
    pub front_buttons: FrontButtons,
    /// Portrait reading's summoned key sheet is up; renderers draw it
    /// over the page's bottom band.
    pub reading_sheet: bool,
    /// The Library per-book actions sheet's progress; renderers draw the
    /// sheet, relabel the key rail, and show the wait or its note.
    pub library_menu: LibraryMenu,
    /// Whether a move through the folder tree is outstanding. The list is
    /// held still until the card answers, and the reducer swallows every
    /// press but Back, so the rail has to stop offering the rest.
    pub library_move_pending: bool,
    pub refresh_policy: RefreshPolicy,
    pub font_size: FontSize,
    pub line_spacing: LineSpacing,
    pub font_weight: FontWeight,
    pub font_family: FontFamily,
    pub last_button: Option<Button>,
    pub aux_raw: u16,
    pub nav_raw: u16,
    pub page_raw: u16,
    pub battery_mv: u16,
    pub battery_percent: u8,
    pub library_count: u16,
    pub sync_status: SyncStatus,
    /// Saved network name for the Wireless screen; len 0 when none.
    pub wifi_ssid: [u8; 32],
    pub wifi_ssid_len: u8,
    pub dirty: Rect,
    /// Device uptime in milliseconds at the instant this request's state was
    /// frozen, or 0 when nothing stamped it.
    ///
    /// `u64` to match `Instant::as_millis()` and every other timestamp in the
    /// telemetry. A `u32` would have been four bytes cheaper and wrapped after
    /// 49.7 days of unbroken uptime, which a reboot or deep sleep normally
    /// resets long before -- but a long soak is exactly where durable
    /// measurement telemetry should not carry an arbitrary horizon, and past
    /// the wrap the pairing would silently stop answering presses.
    ///
    /// This is the boundary the bench pairs button presses against: a press
    /// later than this cannot be reflected in the frame, so crediting it with
    /// the frame's settle time reports a page turn that never happened. It
    /// must be stamped by the *producer*, as the request is built. The display
    /// task dequeues it an unbounded time later -- behind a flush, a prestage,
    /// a storage command or a background build step -- and every press landing
    /// in that window is already too late for this frame.
    ///
    /// `app-core` has no clock, so this stays 0 here and the firmware's single
    /// send site fills it in. 0 reads as "unstamped" downstream; the only
    /// request that could legitimately carry 0 is one frozen in the first
    /// millisecond after boot, which is the boot paint and has no press to
    /// pair with.
    pub requested_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayCommand {
    Render(RenderRequest),
    /// `generation` names this sleep request: the display task echoes it in
    /// `PowerEvent::DisplayAsleep`/`DisplaySleepFailed` so the power task's
    /// handshake can tell its own sleep's acknowledgement from a stale one
    /// left by an earlier sleep it abandoned on `Activity`. The command
    /// channel holds four slots, so two sleep requests can be in flight.
    Sleep {
        generation: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageCommand {
    LoadCatalogCache,
    RefreshCatalog,
    /// Open a book, and — when this is a book change — close out the one being
    /// left in the same command.
    ///
    /// Opening is one transaction the storage task owns end to end: it writes
    /// `previous`'s position file, opens this book, and only then points the
    /// global state file here, at the position the open actually landed on.
    /// Carrying the departing state rather than sending it as a second command
    /// is what makes the pair impossible to separate — there is no interleaving
    /// for a queue to reorder, and no half-finished switch to represent.
    OpenBook {
        request_id: u32,
        book_id: u32,
        index: u16,
        /// The catalog the row was resolved in, when this open came from
        /// resolving one. A catalog index is only a book inside the catalog
        /// that produced it, and this open crosses a queue: the row is
        /// resolved, the answer goes to the app, the app comes back with the
        /// open, and a rebuild in that window leaves a different book sitting
        /// under the same number. The storage task refuses rather than opens
        /// it.
        ///
        /// `None` for every other open, whose index came from the app's own
        /// active book rather than from a row just looked up. Those cross the
        /// same window, and fencing them here would refuse a boot restore
        /// whose `Scanned` the app has not folded yet; the index they carry
        /// is Milestone 4's problem, where a book stops being a row number.
        catalog_epoch: Option<u32>,
        chapter: u16,
        target_pages: u16,
        type_settings: TypeSettings,
        /// Paginate into the portrait page box. Rides beside the type
        /// settings because it changes wrap points the same way.
        portrait: bool,
        /// The departing book's final position, when this open changes books.
        /// `None` re-opens the book already active, which owes nothing.
        previous: Option<PersistedAppState>,
    },
    ExtendSection {
        request_id: u32,
        book_id: u32,
        index: u16,
        chapter: u16,
        target_pages: u16,
        type_settings: TypeSettings,
        portrait: bool,
    },
    /// Load the full chapter list (TOC.BIN) into the reader's section buffer
    /// for the Chapters overview. The reading section reloads on exit.
    LoadChapters {
        request_id: u32,
        book_id: u32,
        index: u16,
    },
    /// Jump to a chapter from the overview. The display task resolves the
    /// chapter's start page from the on-disk TOC (the reducer's chapter-page
    /// map is capped at 128) and loads that section.
    JumpChapter {
        request_id: u32,
        book_id: u32,
        index: u16,
        chapter: u16,
        type_settings: TypeSettings,
        portrait: bool,
    },
    StoreProgress(PersistedAppState),
    /// Hand the EPUB scratch to the wifi task as sync-session heap. One
    /// way: after this the display task refuses scratch-using commands
    /// until the session's software reset reboots the reader.
    LoanSyncMemory,
    /// Persist the credentials captured by the onboarding portal to
    /// /READER/WIFI.BIN. Allowed during a sync session: it is the portal
    /// that sends it.
    StoreWifiCredentials(WifiCredentials),
    /// Record which AP a join actually landed on, keyed to the network it
    /// was for. Fire-and-forget: nothing waits on it and a failed write
    /// only costs the next session a scan.
    StoreWifiApHint {
        ssid: WifiSsid,
        hint: WifiApHint,
    },
    /// Delete /READER/WIFI.BIN. Sent when the user confirms "forget" on
    /// the Wireless screen, which is only reachable before the radio
    /// starts, so it never runs during a sync session.
    ForgetWifiCredentials,
    /// Enter the upload session: the display task parks on the upload
    /// channels and writes browser-sent books to /BOOKS until the
    /// session's reset. Sent by the wifi task at the first upload.
    ReceiveUpload,
    /// Delete one book's rebuildable cache (BOOK/TOC/COVER/CONT.BIN and
    /// SECTIONS/), keeping the position files and the catalog entry. Sent
    /// when the user confirms "Clear cache?" on a Library row.
    ///
    /// A row number alone is not a book. This command can be parked, and the
    /// storage task resolves the row against whatever folder it is standing
    /// in when the command finally runs, which a scan in between may have
    /// taken back to the library root, putting a different book under the
    /// same row. `browse_epoch` is the position the *screen* was listing when
    /// the user picked; the storage task refuses the clear unless it is still
    /// standing there, so a stale row deletes nothing instead of deleting the
    /// wrong book. Identity is then checked a second time against the cache
    /// header, which catches a key collision the generation cannot see.
    ///
    /// `request_id` is what the answering `LibraryEvent::CacheCleared` echoes
    /// back; the epoch guards which book gets deleted, not which wait the
    /// reply belongs to.
    ClearBookCache {
        request_id: u32,
        index: u16,
        browse_epoch: u32,
    },
    /// Say what the Library row at `index` is, and act on it: a folder is
    /// entered and its children listed back, a book is resolved to the
    /// catalog row that opens it.
    ///
    /// The app holds a row count, not a listing, so only the card can tell
    /// the two apart. `browse_epoch` travels because a parked command runs
    /// against whatever folder the storage task is standing in by then, and a
    /// row picked in another one names a different child of a different
    /// place. The catalog's own epoch cannot answer that: a scan whose
    /// recovery is unfinished declines to rebuild the catalog and takes
    /// browsing back to the root anyway.
    ChooseLibraryRow {
        request_id: u32,
        index: u16,
        browse_epoch: u32,
    },
    /// Go up one folder and list the parent, so Back below the library root
    /// zooms out a level rather than leaving for Home.
    ///
    /// Carries no row: the folder being left is the one the storage task is
    /// already in, and naming it again from the app would be the app's copy
    /// of a position storage owns. It carries the generation of that position
    /// for the same reason [`StorageCommand::ChooseLibraryRow`] does.
    LeaveLibraryFolder {
        request_id: u32,
        browse_epoch: u32,
    },
}

/// What the app holds while a dispatched command is in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenHold {
    /// The book whose open is outstanding, and whose answer settles it.
    /// `None` for a command that opens nothing.
    pub opening_book: Option<u32>,
    /// Where to put the reader back if that open is refused rather than
    /// answered. `None` when it cannot be refused.
    pub rollback: Option<BookOpenRollback>,
}

/// What to hold while `command` is in flight, dispatched from `previous`.
///
/// Both halves live here rather than at the dispatch site because the second
/// was got wrong there once: rollback was armed for an open that closes out
/// another book, which stopped covering everything the moment a catalog
/// fence could refuse an open that closes out nobody. Deciding it beside
/// [`StorageCommand::open_may_refuse`] keeps the two from parting again, and
/// lets a test walk the whole sequence.
pub fn open_hold(command: &StorageCommand, previous: &ReaderState) -> OpenHold {
    match command {
        StorageCommand::OpenBook { book_id, .. } => OpenHold {
            opening_book: Some(*book_id),
            rollback: command.open_may_refuse().then(|| previous.open_rollback()),
        },
        _ => OpenHold {
            opening_book: None,
            rollback: None,
        },
    }
}

impl StorageCommand {
    /// Whether this open can end without landing on its book, so its caller
    /// has to keep a way back to where the reader was.
    ///
    /// Two ways an open ends in nothing. It closes out another book, in
    /// which case a refusal leaves the reader between two; or it names the
    /// catalog its row came from, and storage refuses it outright when that
    /// catalog has since been replaced. The second was added later, and the
    /// arming that reads this used to ask only about the first, which left a
    /// row open for the book already being read able to be refused with no
    /// way back. The reader stayed on the reading screen over a row number
    /// that now belongs to a different book.
    pub fn open_may_refuse(&self) -> bool {
        matches!(
            self,
            StorageCommand::OpenBook {
                previous: closing,
                catalog_epoch: fence,
                ..
            } if closing.is_some() || fence.is_some()
        )
    }
}

/// Slots for storage commands the channel could not take yet.
///
/// Two is what a single input transition can produce: one navigation command
/// (open, extend, chapter list, jump) and one deferred write — progress or a
/// credentials forget, never both, since a transition that moves the saved
/// position is not the Wireless-screen confirm.
pub const PARKED_STORAGE_SLOTS: usize = 2;

/// What became of a storage command handed to [`ParkedStorage::dispatch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageDispatch {
    /// Straight into the channel.
    Sent,
    /// Held for the next drain, behind anything already waiting.
    Parked,
    /// Neither, and the caller must cope. Only ever returned for a command
    /// the queue could not make room for without losing something it is not
    /// allowed to lose.
    Rejected,
}

/// Storage commands the channel could not take yet, in arrival order.
///
/// Nothing may overtake what is parked: the storage task applies commands in
/// the order it receives them, so a progress record that slipped past a parked
/// open would land after the new book was opened and point the global state
/// file back at the book the reader had just left.
///
/// Most of what parks here is replaceable — a dropped progress record is
/// reissued by the next page turn. An `OpenBook` is not. It is the only carrier
/// of the departing book's close-out position, and the app arms an input lock
/// waiting for the event it will produce, so silently dropping one strands both
/// the page and the reader. `dispatch` therefore never drops an open: it makes
/// room, or says so.
///
/// RAM: `PARKED_STORAGE_SLOTS` × a 100-byte `Option<StorageCommand>` plus the
/// length, 204 bytes, in the app task's arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParkedStorage {
    queue: [Option<StorageCommand>; PARKED_STORAGE_SLOTS],
    len: usize,
}

impl Default for ParkedStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl ParkedStorage {
    pub const fn new() -> Self {
        Self {
            queue: [None; PARKED_STORAGE_SLOTS],
            len: 0,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    /// Sends `command` if nothing is waiting and the channel takes it, parks it
    /// otherwise, and only reports [`StorageDispatch::Rejected`] when it can do
    /// neither without dropping something irreplaceable.
    pub fn dispatch(
        &mut self,
        command: StorageCommand,
        try_send: impl FnOnce(StorageCommand) -> bool,
    ) -> StorageDispatch {
        if self.is_empty() && try_send(command) {
            return StorageDispatch::Sent;
        }
        if self.push_back(command) {
            return StorageDispatch::Parked;
        }
        // Full. An open may still displace a write it has already subsumed:
        // it carries the departing book's position in `previous`, so a parked
        // `StoreProgress` for that same book is recording something this
        // command will write anyway.
        if let Some(slot) = self.subsumed_by(command) {
            self.remove(slot);
            let _ = self.push_back(command);
            return StorageDispatch::Parked;
        }
        StorageDispatch::Rejected
    }

    /// The parked write `command` makes redundant, if any.
    fn subsumed_by(&self, command: StorageCommand) -> Option<usize> {
        let StorageCommand::OpenBook {
            previous: Some(departing),
            ..
        } = command
        else {
            return None;
        };
        self.queue[..self.len].iter().position(|parked| {
            matches!(
                parked,
                Some(StorageCommand::StoreProgress(record)) if record.book_id == departing.book_id
            )
        })
    }

    pub fn push_back(&mut self, command: StorageCommand) -> bool {
        if self.len == PARKED_STORAGE_SLOTS {
            return false;
        }
        self.queue[self.len] = Some(command);
        self.len += 1;
        true
    }

    /// The command at the front, left where it is.
    ///
    /// Lets a drain offer a command to a queue that may refuse it without
    /// having to put it back afterwards: nothing is taken until it has landed,
    /// so the parked order cannot be disturbed by a refusal.
    pub fn front(&self) -> Option<StorageCommand> {
        self.queue[0]
    }

    pub fn pop_front(&mut self) -> Option<StorageCommand> {
        let command = self.queue[0].take()?;
        self.queue.copy_within(1..self.len, 0);
        self.len -= 1;
        self.queue[self.len] = None;
        Some(command)
    }

    fn remove(&mut self, slot: usize) {
        if slot >= self.len {
            return;
        }
        self.queue.copy_within(slot + 1..self.len, slot);
        self.len -= 1;
        self.queue[self.len] = None;
    }
}

/// Holds a Power press until the app has nothing left to hand the storage task.
///
/// Deep sleep is terminal — waking is a fresh boot — so whatever the app is
/// still holding when the panel sleeps is simply gone. The display task does
/// flush before it sleeps, but only its own coalesced record: it cannot see a
/// command parked in the app, and its command channels are separate, so a
/// `Sleep` is picked up ahead of an `OpenBook` already queued behind it.
///
/// A book open is the costly thing to lose that way. It carries the departing
/// book's only close-out position, and nothing reissues it: the reader has
/// already left that book, and the transaction deliberately suppresses the
/// progress record that used to follow. So the press waits for the open to
/// resolve — either outcome will do — rather than racing it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SleepGate {
    deferred: bool,
}

/// What the app still owes before the panel may sleep.
///
/// All three have to be clear. The first two are durability — work that has not
/// reached the storage task cannot survive a terminal sleep. The third is what
/// the panel keeps showing: the sleep screen is drawn from the last frame that
/// reached it, so sleeping between an open answering and its frame settling
/// leaves the reader looking at a book they are no longer in, for as long as
/// the device stays off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SleepBlockers {
    /// A book open was accepted and has not answered yet.
    pub open_unresolved: bool,
    /// Commands are parked, waiting on the next drain to send them.
    pub parked_storage: bool,
    /// An open answered, but the frame resolving it has not settled.
    pub awaiting_open_frame: bool,
}

impl SleepBlockers {
    pub const fn any(self) -> bool {
        self.open_unresolved || self.parked_storage || self.awaiting_open_frame
    }
}

impl SleepGate {
    pub const fn new() -> Self {
        Self { deferred: false }
    }

    /// A Power press. Returns whether the sleep request goes out now.
    pub fn press(&mut self, blockers: SleepBlockers) -> bool {
        if blockers.any() {
            self.deferred = true;
            return false;
        }
        true
    }

    /// A display cycle ended. Returns whether a press that was held back should
    /// go out now; only ever true once per press.
    pub fn release(&mut self, blockers: SleepBlockers) -> bool {
        if self.deferred && !blockers.any() {
            self.deferred = false;
            return true;
        }
        false
    }

    pub const fn is_deferred(&self) -> bool {
        self.deferred
    }
}

/// Whether a `Loaded` is worth the panel refresh that folding it would cost.
///
/// Every `Loaded` the app renders on costs a full refresh — measured at ~405 ms
/// of panel time on the X3. A page turn used to pay two of them: one when the
/// page changes and the app renders optimistically, and one when storage
/// answers the extend. Measured on device: 31 `view=Reading` renders for 16 page
/// turns, 14.4 s of refresh in a 48.5 s window.
///
/// The second is redundant when the event moves nothing the reader can see. In
/// Reading that is a short list, because the page counter is chapter-relative
/// and drawn from the storage task's own store at render time, not from the
/// numbers in here: the text and the page. `pages`, `chapters`, `chapter_pages`
/// and `current_chapter` are navigation bounds and persistence state — the
/// reducer clamps against them and the chapter overview lists them, and it
/// reloads that list on entry.
///
/// `chapter` is not a visible input to the SD Reading compositor. The firmware
/// selects the body using `request.page`, and the footer derives its
/// chapter-relative counter from `sd_library.chapter_page_position(request.page)`;
/// the SD path bypasses the generic UI model entirely. The
/// [`ReaderState::apply_chapter_cursor`] contract says the same thing:
/// correcting `ReaderState::chapter` is silent because Reading does not display
/// that value. Home, the sleep screen, Chapters, and the persisted position
/// pick the corrected value up when next used.
///
/// The event is *always folded*, and that is the load-bearing half. The
/// background index walk raises the store's page count as it goes and stays
/// deliberately silent about it while the reader is behind its frontier, so the
/// count the app holds is only as fresh as the last event it was handed. Skip
/// the fold and the reader walks to their stale last page, where Next clamps to
/// a no-op: no state change, no command, no repaint, and a walk that will not
/// announce because by its own test the reader is nowhere near *its* frontier.
/// A dead button until the build finishes. So nothing is withheld from the app,
/// and only the repaint is optional.
///
/// Outside Reading the answer is always yes. The saving is a reading-path one,
/// and the other views draw enough of the store (Home's colophon, the chapter
/// overview) that the cost of reasoning about each is worth more than the
/// refresh it would save.
pub const fn loaded_repaints(reading: bool, text_replaced: bool, page_moved: bool) -> bool {
    !reading || text_replaced || page_moved
}

/// Whether the input gate an open took may be lifted now.
///
/// A dispatched `OpenBook` shuts input off until the open has both *answered*
/// and *landed on the panel*, so a press cannot be read against a screen that
/// still shows the book being left. Two things therefore have to be true, and
/// the gate is lifted by whichever of them finishes last:
///
/// - the open answered — `Loaded` or `BookOpenFailed` cleared `opening_book`;
/// - no frame is in flight for it.
///
/// Reading the second one as "a render cycle just ended" is the trap. It holds
/// for every open that repaints, which is nearly all of them, and it is how the
/// gate was lifted for a long time — but `loaded_repaints` can answer an open
/// with no repaint at all (re-entering Reading on the resident book, at the page
/// it was already on), and that event ends no cycle. Nothing else is coming: the
/// open is over, the panel is already correct, and a gate waiting on a frame
/// nobody owes stays shut for good — every press ignored, sleep deferred, until
/// the battery goes. So the test is whether a frame is *in flight*, which an
/// event arriving between cycles can answer as well as a cycle ending can.
pub const fn open_gate_may_lift(gated: bool, open_unresolved: bool, frame_in_flight: bool) -> bool {
    gated && !open_unresolved && !frame_in_flight
}

/// One repaint owed to the reader after a panel transition that failed.
///
/// A failed flush may have run partially, so the panel is showing the old page,
/// a torn one, or nothing — while the app has moved on regardless: the press was
/// reduced, the page advanced, and the progress record went to the card. The
/// display task answers a failure by forgetting its model of the panel so that
/// the *next* render re-inits and takes the full waveform. This is what makes
/// sure there is a next render: without it the next one is whatever the reader
/// presses, and that press advances the page again — so the page they never saw
/// looks like one the device skipped.
///
/// Until the page-turn announce was suppressed, the extend's `Loaded` arrived
/// just behind the failure and re-rendered. That was recovery by accident of
/// event order, and only for turns storage answered with an event; the repaint
/// is owed either way, so the app asks for it here rather than depending on
/// there.
///
/// Bounded at one per frame that reaches the panel. A panel that fails its retry
/// is not coming back this cycle, and repainting into it on every failure would
/// spend the display task and the battery on a reader who can still press their
/// way out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepaintRetry {
    spent: bool,
}

impl RepaintRetry {
    pub const fn new() -> Self {
        Self { spent: false }
    }

    /// A panel transition failed. Returns whether the app repaints current
    /// state now; only ever true once until a frame settles.
    pub fn failed(&mut self) -> bool {
        if self.spent {
            return false;
        }
        self.spent = true;
        true
    }

    /// A frame reached the panel. The next failure is a fresh one, so it gets
    /// its own retry.
    pub fn settled(&mut self) {
        self.spent = false;
    }
}

/// Where the reader sits before a book-open transaction, so an abort can put
/// it back. Held by the app task, which is the only place that still knows.
///
/// Carries the departing book's navigation bounds as well as its position. The
/// Library confirm that starts an open resets those bounds to a one-page book,
/// and no `Loaded` ever arrives to correct them for a switch that did not
/// happen — so restoring the page alone would leave the reader on the right
/// page of a book the reducer believes is one page long, and the next page turn
/// would clamp them to the start and persist it.
///
/// RAM: `sd_chapter_pages` (128 entries) makes this 276 bytes rather than 20,
/// held in one `Option` in the app task's arena while an open is inflight. The
/// alternative — not clobbering the bounds until the open is accepted — would
/// have to move that decision out of the reducer, which is where the shape of a
/// freshly selected book belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookOpenRollback {
    pub book_id: u32,
    pub chapter: u16,
    pub page: u32,
    pub view: AppView,
    pub selection: u16,
    pub sd_page_count: u32,
    pub sd_chapter_count: u16,
    pub sd_chapter_pages: [u16; MAX_SD_CHAPTERS],
}

/// How a book-open transaction ended.
///
/// The policy is deliberately strict, and the strictness is what removes the
/// need to queue partially finished switches: the reader either completes the
/// move or stays wholly on the book it started from, so there is never a
/// half-applied switch for a later command to reconcile.
///
/// A book that will not open is deliberately not an outcome here. By that
/// point the card has been read over and the previous book is no longer
/// resident, so "keep the old book" would mean showing its title with nothing
/// behind it; the reader is better served by the load error on the book it
/// actually asked for. The departing page is safe either way — step one wrote
/// it before the open was attempted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookOpenOutcome {
    /// Departing position written, book opened, global pointer moved.
    Opened,
    /// The departing book's position never reached the card, so the open was
    /// not attempted. The reader stays where it was and owes that position.
    KeptBookPositionUnwritten,
    /// The book is open and readable, but the global pointer still names the
    /// old one, so a reboot before the retry returns there. Recoverable: the
    /// new record is owed to the card and rides the next flush.
    OpenedPointerOwed,
}

/// Resolves a book-open transaction from which of its two writes landed.
///
/// `pointer_stored` is only meaningful once `previous_stored` holds: the
/// transaction stops at the first failure, and the ordering here mirrors that.
pub const fn book_open_outcome(previous_stored: bool, pointer_stored: bool) -> BookOpenOutcome {
    if !previous_stored {
        BookOpenOutcome::KeptBookPositionUnwritten
    } else if !pointer_stored {
        BookOpenOutcome::OpenedPointerOwed
    } else {
        BookOpenOutcome::Opened
    }
}

impl BookOpenOutcome {
    /// Whether the reader ends up on the book the open asked for.
    pub const fn book_changed(self) -> bool {
        matches!(self, Self::Opened | Self::OpenedPointerOwed)
    }
}

/// The storage command a reader-state transition owes, if any.
///
/// Lives here rather than in the firmware task so the dispatch rules are
/// host-testable — above all that a book change produces exactly one command,
/// carrying the departing book's position, rather than an open plus a
/// separate write that a full channel could separate them.
///
/// `request_id` is supplied by the caller because the firmware's counter is a
/// task-owned atomic; the value only has to be the one the storage task will
/// compare against `LATEST_READER_REQUEST_ID`.
pub fn storage_command_for_transition(
    previous: &ReaderState,
    next: &ReaderState,
    request_id: u32,
) -> Option<StorageCommand> {
    let index = ReaderSource::from_book_id(next.book_id).sd_index()?;
    // Entering the overview loads the full chapter list into the section
    // buffer; the reading section reloads on exit.
    if next.view == AppView::Chapters && previous.view != AppView::Chapters {
        return Some(StorageCommand::LoadChapters {
            request_id,
            book_id: next.book_id,
            index,
        });
    }
    if next.view != AppView::Reading {
        return None;
    }

    // Leaving Library for a book is the row-open path, whether or not the row
    // names the book already being read: storage resolved it against a
    // catalog and answered with its number, and a scan between that answer
    // and this open would leave the number naming something else.
    //
    // Read from the diff, which cannot tell a row naming the current book
    // from staying put. Firmware holds the answering event and stamps the
    // epoch from it, overriding this; see `dispatch_transition_storage`.
    let fence = (previous.view == AppView::Library).then_some(next.catalog_epoch);
    if previous.book_id != next.book_id {
        // The one case that closes out another book. Everything the switch
        // owes rides in this command.
        //
        return Some(open_book_command(
            next,
            index,
            request_id,
            Some(previous.persisted()),
            fence,
        ));
    }

    if previous.view != AppView::Reading {
        if previous.view == AppView::Chapters {
            // The buffer held the TOC, so the section always reloads. A new
            // chapter selection resolves its page from the on-disk TOC; a
            // plain back-out just reloads the page we left.
            return if next.chapter != previous.chapter {
                Some(StorageCommand::JumpChapter {
                    request_id,
                    book_id: next.book_id,
                    index,
                    chapter: next.chapter,
                    type_settings: next.type_settings(),
                    portrait: is_portrait(next.orientation),
                })
            } else {
                Some(extend_section_command(next, index, request_id))
            };
        }
        // An unchanged book id no longer proves the store holds its
        // pages: boot restore and the scan default set the active book
        // without loading anything. Entering Reading always requests
        // the section; an already-loaded book answers from RAM without
        // an SD session.
        return Some(open_book_command(next, index, request_id, None, fence));
    }

    if previous.page != next.page || previous.chapter != next.chapter {
        return Some(extend_section_command(next, index, request_id));
    }

    None
}

/// Confirming a row on the Library actions sheet owes storage one command:
/// the transition out of `Sheet` into `Busy` is the pick. Back and the
/// summoning key dismiss to `None`, which owes nothing — the two exits
/// are distinct states, so the transition cannot be mistaken for a cancel.
pub fn library_action_command_for_transition(
    previous: &ReaderState,
    next: &ReaderState,
) -> Option<StorageCommand> {
    let (
        LibraryMenu::Sheet { .. },
        LibraryMenu::Busy {
            action,
            index,
            request_id,
        },
    ) = (previous.library_menu, next.library_menu)
    else {
        return None;
    };
    // Exhaustive over the action on purpose. Every `Busy` is a promise that
    // some command is on its way to settle it, so an action added without a
    // command here would hang the Library list on "…" forever. Naming the
    // variants makes that a build failure instead.
    Some(match action {
        LibraryAction::ClearCache => StorageCommand::ClearBookCache {
            request_id,
            // The row and the listing it was a row *in*, together: neither
            // half means anything without the other by the time the storage
            // task gets to it.
            index,
            browse_epoch: next.library_browse_epoch,
        },
    })
}

/// Moving through the folder tree owes storage one command: the transition
/// out of `Idle` is the press.
///
/// Same shape as [`library_action_command_for_transition`], and for the same
/// reason: a wait the app enters is a promise that some command is on its way
/// to end it, and deriving the command from the transition is what keeps the
/// two from being added apart.
pub fn library_browse_command_for_transition(
    previous: &ReaderState,
    next: &ReaderState,
) -> Option<StorageCommand> {
    if !previous.library_browse.is_idle() {
        return None;
    }
    // Exhaustive on purpose, like the actions sheet's: a wait added without a
    // command here would hold the Library rail forever.
    match next.library_browse {
        LibraryBrowse::Idle => None,
        LibraryBrowse::Choosing {
            index,
            request_id,
            browse_epoch,
        } => Some(StorageCommand::ChooseLibraryRow {
            request_id,
            index,
            browse_epoch,
        }),
        LibraryBrowse::Leaving {
            request_id,
            browse_epoch,
        } => Some(StorageCommand::LeaveLibraryFolder {
            request_id,
            browse_epoch,
        }),
    }
}

/// An open of `state`'s book, closing out `previous` when this changes books.
///
/// `catalog_epoch` names the catalog `index` was resolved in, for an open
/// whose index came from somewhere other than the catalog being opened
/// against. Storage refuses one whose catalog has since been replaced, since
/// the number would name a different book. `None` for an open that resolves
/// its own index against the catalog in hand, which has nothing to be stale
/// against.
pub fn open_book_command(
    state: &ReaderState,
    index: u16,
    request_id: u32,
    previous: Option<PersistedAppState>,
    catalog_epoch: Option<u32>,
) -> StorageCommand {
    StorageCommand::OpenBook {
        request_id,
        book_id: state.book_id,
        index,
        catalog_epoch,
        chapter: state.chapter,
        target_pages: state.page.min(u16::MAX as u32) as u16,
        type_settings: state.type_settings(),
        portrait: is_portrait(state.orientation),
        previous,
    }
}

pub fn extend_section_command(state: &ReaderState, index: u16, request_id: u32) -> StorageCommand {
    StorageCommand::ExtendSection {
        request_id,
        book_id: state.book_id,
        index,
        chapter: state.chapter,
        target_pages: state.page.min(u16::MAX as u32) as u16,
        type_settings: state.type_settings(),
        portrait: is_portrait(state.orientation),
    }
}

/// The sync session's storage-admission rules. Granting the loan is one-way:
/// the EPUB scratch becomes radio heap, so every scratch-using storage command
/// is refused from then on and only the session-ending software reset brings
/// the reader pipeline back. Progress writes stay alive (they are cheap and
/// harmless), the portal stores credentials, and uploads only make sense
/// while the browser shelf is being served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SyncSession {
    /// The reader pipeline owns the scratch; ordinary storage work runs.
    #[default]
    Idle,
    /// The scratch is loaned to the radio until the session-ending reset.
    Loaned,
}

impl SyncSession {
    /// Whether a storage command may run in the current session state.
    pub fn admits(&self, command: &StorageCommand) -> bool {
        match self {
            // Uploads arrive from the browser shelf, which only exists once
            // the session is serving; outside it the command is a stray.
            SyncSession::Idle => !matches!(command, StorageCommand::ReceiveUpload),
            SyncSession::Loaned => matches!(
                command,
                StorageCommand::StoreProgress(_)
                    | StorageCommand::StoreWifiCredentials(_)
                    // Learned at the moment of joining, which is inside the
                    // session; refusing it here would drop every hint.
                    | StorageCommand::StoreWifiApHint { .. }
                    | StorageCommand::ReceiveUpload
            ),
        }
    }

    /// Whether the session is running, i.e. the loan has been granted.
    /// Render-path catalog reads stop once this is true: the browser shelf
    /// may be rewriting the card underneath, and the visible surface is the
    /// Sync screen anyway.
    pub fn active(&self) -> bool {
        matches!(self, SyncSession::Loaned)
    }

    /// One-way transition: the display task has dismantled the scratch and
    /// shipped it to the wifi task.
    pub fn loan_granted(&mut self) {
        *self = SyncSession::Loaned;
    }
}

/// Station credentials as a bounded Copy message: what the onboarding
/// portal captures and what `/READER/WIFI.BIN` stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiCredentials {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub password: [u8; 64],
    pub password_len: u8,
}

impl WifiCredentials {
    pub fn from_strs(ssid: &str, password: &str) -> Option<Self> {
        if ssid.is_empty() || ssid.len() > 32 || password.len() > 64 {
            return None;
        }
        let mut record = Self {
            ssid: [0; 32],
            ssid_len: ssid.len() as u8,
            password: [0; 64],
            password_len: password.len() as u8,
        };
        record.ssid[..ssid.len()].copy_from_slice(ssid.as_bytes());
        record.password[..password.len()].copy_from_slice(password.as_bytes());
        Some(record)
    }

    pub fn ssid(&self) -> &str {
        core::str::from_utf8(&self.ssid[..self.ssid_len.min(32) as usize]).unwrap_or("")
    }

    pub fn password(&self) -> &str {
        core::str::from_utf8(&self.password[..self.password_len.min(64) as usize]).unwrap_or("")
    }

    pub fn ssid_message(&self) -> WifiSsid {
        WifiSsid {
            bytes: self.ssid,
            len: self.ssid_len,
        }
    }
}

/// Which access point the station last associated through, for a directed
/// join that skips the all-channel sweep.
///
/// A hint and nothing more: the join falls back to a full scan whenever it
/// is missing, stale, or simply does not answer. Keyed to an SSID on disk
/// (see `proto::nvm::WifiApHintRecord`), but by the time one reaches here
/// the storage task has already proven it belongs to the network being
/// joined, so this carries only the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiApHint {
    pub bssid: [u8; 6],
    /// 1-14, never zero — the storage side refuses anything else.
    pub channel: u8,
}

/// A network name alone, as a bounded Copy message: what the Wireless
/// screen shows. Events carry this instead of `WifiCredentials` so the
/// password never travels further than the radio and the card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiSsid {
    pub bytes: [u8; 32],
    pub len: u8,
}

impl WifiSsid {
    pub fn new(ssid: &str) -> Option<Self> {
        if ssid.is_empty() || ssid.len() > 32 {
            return None;
        }
        let mut message = Self {
            bytes: [0; 32],
            len: ssid.len() as u8,
        };
        message.bytes[..ssid.len()].copy_from_slice(ssid.as_bytes());
        Some(message)
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len.min(32) as usize]).unwrap_or("")
    }
}

/// The onboarding hotspot's SSID, built from this device's MAC address.
///
/// Per device rather than per board. Two readers running this firmware raise
/// two hotspots, and a name that only said which model it was would leave a
/// pair of the same model indistinguishable in a Wi-Fi list — where some
/// clients collapse identical SSIDs into one row and give no way to pick. The
/// screen names the network the join QR points at, so whatever is on screen
/// has to match exactly one entry in the list.
///
/// Stored as the three MAC bytes it varies by, not as the finished string: this
/// rides `SyncEvent::PortalUp` into a `RenderRequest` that sits four deep in a
/// channel, and ten of the sixteen characters are a prefix that never
/// changes. [`Self::write_into`] spells it out into a caller's buffer, which
/// costs a stack frame rather than `.bss`.
///
/// It travels from the firmware because that is the only layer that can read a
/// MAC. Nothing here is keyed on the board, so a new one inherits it with
/// nothing to remember.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PortalSsid {
    tail: [u8; 3],
}

impl PortalSsid {
    /// `CALENDULA-` plus six hex digits.
    pub const LEN: usize = 16;
    const PREFIX: &'static [u8] = b"CALENDULA-";

    /// Fixed value for the emulators' synthetic portal flow, so golden frames
    /// render deterministically. Never used on hardware, where the tail comes
    /// from the MAC.
    pub const EMULATOR_DEMO: Self = Self {
        tail: [0xDE, 0x11, 0x0A],
    };

    /// The name for a device whose MAC ends in these three bytes.
    ///
    /// Three because that is the widest device-specific part a MAC is sure to
    /// carry: no IEEE allocation block holds more than 2^24 addresses, so
    /// within any one block the low three bytes are distinct. Two readers
    /// whose MACs come from the same block cannot share a name.
    ///
    /// Between blocks they can, and one vendor may hold several — so "same
    /// silicon" does not settle it either. There is no single likelihood to
    /// quote for that: a 24-bit allocation prefix leaves all three bytes
    /// varying, a 36-bit one leaves twelve bits, so how much of the tail is
    /// really free depends on how the blocks were handed out. This is a
    /// discriminator, not an identifier.
    ///
    /// Mixing the whole 48-bit address down to three bytes would buy a
    /// uniform chance instead of that ragged one, at the cost of a suffix
    /// nobody can read off a MAC — which is worth more when two devices are
    /// on the bench than the difference between unlikely and unlikelier.
    pub const fn from_mac_tail(tail: [u8; 3]) -> Self {
        Self { tail }
    }

    /// Spell the name into `buf` and borrow it back.
    pub fn write_into<'a>(&self, buf: &'a mut [u8; Self::LEN]) -> &'a str {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        buf[..Self::PREFIX.len()].copy_from_slice(Self::PREFIX);
        let at = Self::PREFIX.len();
        buf[at] = HEX[usize::from(self.tail[0] >> 4)];
        buf[at + 1] = HEX[usize::from(self.tail[0] & 0x0F)];
        buf[at + 2] = HEX[usize::from(self.tail[1] >> 4)];
        buf[at + 3] = HEX[usize::from(self.tail[1] & 0x0F)];
        buf[at + 4] = HEX[usize::from(self.tail[2] >> 4)];
        buf[at + 5] = HEX[usize::from(self.tail[2] & 0x0F)];
        // Prefix and hex digits alike are ASCII by construction.
        core::str::from_utf8(buf).unwrap_or("")
    }
}

/// The onboarding hotspot's WPA2 PSK, minted fresh from the hardware RNG
/// each time the portal starts. It rides `SyncEvent::PortalUp` into
/// `SyncStatus` so the Wireless screen can render the join QR and the
/// manual-join password text — the display is the only channel that
/// carries it, so nothing secret lives in the repo or the release binary.
/// Always exactly [`PortalPsk::LEN`] ASCII characters from
/// [`PSK_ALPHABET`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PortalPsk {
    bytes: [u8; PortalPsk::LEN],
}

impl core::fmt::Debug for PortalPsk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PortalPsk")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

/// Alphabet for the per-session portal PSK: ASCII alphanumerics minus
/// the hand-typing-ambiguous 0/O/1/I/l/i/o (phones that cannot scan
/// type it from the screen) and nothing the `WIFI:` QR payload needs
/// escaped (`\ ; , : "`). 55 characters. Lives here rather than in the
/// firmware's minting code so [`PortalPsk::EMULATOR_DEMO`] is
/// host-testable against it.
pub const PSK_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz";

impl PortalPsk {
    pub const LEN: usize = 16;

    /// Fixed value for the emulators' synthetic portal flow, so golden
    /// frames render deterministically. Sixteen characters from the same
    /// unambiguous alphabet the firmware mints from; never used on
    /// hardware.
    pub const EMULATOR_DEMO: Self = Self {
        bytes: *b"emudemqpsk234567",
    };

    /// Constructs a PSK, refusing any byte outside [`PSK_ALPHABET`] —
    /// which also rules out non-ASCII bytes and the characters the
    /// `WIFI:` QR payload would need escaped.
    pub fn new(bytes: [u8; Self::LEN]) -> Option<Self> {
        if bytes.iter().all(|b| PSK_ALPHABET.contains(b)) {
            Some(Self { bytes })
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        // PSK_ALPHABET is pure ASCII, so validated bytes are always UTF-8.
        core::str::from_utf8(&self.bytes).unwrap_or("")
    }

    pub const fn bytes(&self) -> [u8; Self::LEN] {
        self.bytes
    }
}

/// The reader's true chapter for the page just shown.
///
/// The reducer's page map is capped at [`MAX_SD_CHAPTERS`], so past that cap
/// its own idea of the chapter goes stale; only the loaded SD reader has the
/// uncapped map. The display task reads the real one off the page it just
/// rendered and sends it back with the acknowledgement.
///
/// It names the page as well as the book because it is an answer about one
/// particular frame, and the reader need not still be on that frame when the
/// answer lands: input is applied while a render is in flight, and only the
/// repaint waits. Applied without the page check, a correction for the page
/// left behind would pair that chapter with the page moved to — and
/// [`extend_section_command`] reads the two together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChapterCursor {
    pub book_id: u32,
    pub page: u32,
    pub current_chapter: u16,
}

// Bounded Copy messages by design: chapter_pages rides inside the event
// because firmware has no heap to box large variants into.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayEvent {
    /// The frame reached the panel and the render cycle is over.
    ///
    /// Carries the chapter correction for the page just shown, when there is
    /// one. It rides here rather than travelling as its own event because the
    /// app must apply it *before* it clears the render lock — clearing the
    /// lock is what lets the next navigation read `chapter` — and no pair of
    /// separate messages can promise that. They were separate once: the
    /// correction went down whichever channel had room, and the two could be
    /// split across channels the app selects on independently, or the
    /// correction dropped outright while the acknowledgement survived. One
    /// message cannot arrive in the wrong order or half-arrive.
    Settled {
        chapter_cursor: Option<ChapterCursor>,
    },
    /// The panel completed a sleep transition. Informational for the app:
    /// like `SleepFailed`, it does not end a render cycle, and the sleep's
    /// handshake may already have been abandoned with render/open work
    /// queued behind the Sleep command — the app must not reset any
    /// bookkeeping on it. Renders are acknowledged individually by
    /// `Settled`/`RefreshFailed` regardless of interleaved sleeps.
    Asleep,
    /// A panel refresh (render flush or wake init) did not complete: the
    /// SPI transfer failed or the BUSY handshake never finished. The
    /// panel's contents are unknown and the frame must not be treated as
    /// shown; this ends the render cycle, so the app clears its render
    /// lock and runs the failed-render recovery.
    RefreshFailed,
    /// A sleep transition failed (progress flush or panel handshake).
    /// Distinct from `RefreshFailed` because it does not end a render
    /// cycle: a render queued behind the sleep command is still pending,
    /// so the app must not clear its render lock over it.
    SleepFailed,
    Library(LibraryEvent),
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryEvent {
    Scanned {
        count: u16,
        /// Bumped by the storage task every time it replaces its catalog.
        /// Row numbers only mean something within one epoch, so a command
        /// that names a row carries the epoch it was picked in; see
        /// [`StorageCommand::ClearBookCache`].
        catalog_epoch: u32,
    },
    /// A section load finished: the book's shape, and where the reader now is.
    ///
    /// This is the only event an open produces. It used to be followed by a
    /// second `Restored` whenever the storage task resumed the book somewhere
    /// other than the requested page, which meant the app rendered the wrong
    /// page first and then corrected it — two panel refreshes and a visible
    /// flash. The landing position rides here instead, so one event settles
    /// the open and one render draws it.
    Loaded {
        book_id: u32,
        pages: u32,
        chapters: u16,
        /// The chapter the reading page currently sits in, computed by the
        /// firmware over the whole book. Unlike `chapter_pages` (capped at
        /// `MAX_SD_CHAPTERS`), this tracks position into a long book so the
        /// colophon and chapter cursor do not stick past the cap.
        current_chapter: u16,
        chapter_pages: [u16; MAX_SD_CHAPTERS],
        /// The page the storage task landed on, when it chose it rather than
        /// the app: a per-book resume, or a chapter jump resolved from the
        /// on-disk TOC. `None` means the load answered the page that was
        /// asked for, and the app's own page stands — adopting a page from
        /// every load would rubber-band the reader back onto an in-flight
        /// request during quick page turns.
        position: Option<u32>,
        /// Whether this load put different text under the reader.
        ///
        /// The one thing in here the app cannot work out for itself. Every
        /// other field it can compare against what it already holds, but the
        /// text is drawn straight from the storage task's own buffer at render
        /// time, so a load that replaced it leaves the panel showing a frame
        /// that no state of the app's disagrees with. `false` is the page turn
        /// answered out of the resident section window: no card session, no new
        /// text, and — when nothing else moved either — no repaint.
        ///
        /// Note what this is deliberately *not*: "the announced fields
        /// changed". A section really read from the card can leave every field
        /// identical and still have replaced the text, which is what a chapter
        /// spanning two cache sections does — both halves report the same
        /// chapter. See [`loaded_repaints`].
        text_replaced: bool,
    },
    /// A book-open transaction refused to complete, so the book was never
    /// opened and the reader must go back to the one it was reading.
    ///
    /// Sent when the departing book's position could not be written: opening
    /// on top of that failure would strand a page that nothing else will
    /// rewrite, because the reader has already left the book that owns it.
    BookOpenFailed {
        book_id: u32,
    },
    ChapterPage {
        book_id: u32,
        chapter: u16,
        page: u32,
    },
    CustomFont {
        available: bool,
    },
    Restored {
        book_id: u32,
        chapter: u16,
        page: u32,
        /// The book's total page count, read from the cache index header at
        /// restore so the Home progress bar has a denominator before the book
        /// is opened. 0 when unavailable (the bar keeps its fallback).
        page_count: u32,
        reading_orientation: u8,
        refresh_policy: u8,
        font_size: u8,
        line_spacing: u8,
        font_weight: u8,
        font_family: u8,
        front_buttons: u8,
    },
    /// A `ClearBookCache` settled. `ok` is false when the row was stale (the
    /// catalog changed under it), could not be resolved, its identity did not
    /// match the cache on card, or something rebuildable survived the delete.
    ///
    /// `request_id` echoes the command this answers, and is the only thing
    /// that identifies it. The row cannot serve: leaving Library drops the
    /// wait but not the command, so the same row can be cleared twice with
    /// both in flight, and then a row match would let the first answer settle
    /// the second wait — showing a stale-epoch refusal as the outcome of a
    /// clear that is still running. The reducer shows the note only if the
    /// user is still waiting on the Library screen for this exact request.
    CacheCleared {
        request_id: u32,
        ok: bool,
    },
    /// A folder was listed: entered through `ChooseLibraryRow`, or arrived at
    /// by `LeaveLibraryFolder`. Carries everything the list is drawn from, so
    /// one event settles a move.
    ///
    /// `selection` is where the cursor lands, which the storage task decides:
    /// going in starts at the top, and coming out returns to the row the
    /// folder was entered from, found by name.
    FolderListed {
        /// The move this answers, or `None` for a listing nobody asked for:
        /// a scan replaces the catalog and relists wherever the reader is,
        /// with no press behind it.
        request_id: Option<u32>,
        /// The position generation this listing was taken in. An unsolicited
        /// listing outranks a move issued in an older one, because the scan
        /// has already taken the storage task back to the root and the move
        /// can only come back refused, or worse land on a row number that now
        /// names a different child of a different place. Without this the two
        /// would end up browsing different folders, with the screen describing
        /// one and every later command landing on the other.
        browse_epoch: u32,
        depth: u8,
        count: u16,
        /// How many of `count` are books; the rest are folders, below them.
        books: u16,
        selection: u16,
    },
    /// The chosen row was a book, and it is the catalog's row `index` in the
    /// catalog named by `catalog_epoch`.
    ///
    /// Storage answers and stops there. Opening is the app's, because an open
    /// is more than the command: it commits the reader request id the storage
    /// task checks a later open against, arms the gate that keeps input off
    /// the panel until the book lands, and keeps the rollback that puts the
    /// reader back on the previous book if the command is refused. A storage
    /// task opening on its own initiative has none of that, and its command
    /// carries an id from the wrong counter besides.
    RowIsBook {
        request_id: u32,
        index: u16,
        catalog_epoch: u32,
    },
    /// The chosen row could not be acted on: gone since the listing, deeper
    /// than a locator can name, or absent from the catalog because the card
    /// changed under the scan. Nothing moves, and the wait ends.
    RowFailed {
        request_id: u32,
    },
    /// The library could not be listed at all: the card answered the scan and
    /// then stopped answering for the rows. Carries the position generation
    /// the failed relist moved to, for the reason
    /// [`LibraryEvent::FolderListed`] does.
    ///
    /// Distinct from a listing of zero rows, which is a card that answered and
    /// had nothing to show. The reader is at the root either way, and with
    /// nothing loaded either way; what differs is whether the screen offers to
    /// add books or says the library is unavailable, and that is a lie the
    /// count alone cannot avoid telling.
    LibraryUnreadable {
        browse_epoch: u32,
    },
}

/// Slots in the library-event channel. Lives here because the eviction walk
/// below is written against it and the two must not drift; the firmware sizes
/// the channel from this.
pub const LIBRARY_EVENT_SLOTS: usize = 8;

/// Making room in a full library-event channel for an event that must be
/// delivered, without discarding another one that must be delivered.
///
/// The channel is a ring the sender cannot look into: the only way to see
/// what is queued is to take it off the front. So making room is a walk —
/// take the head, and either drop it (a refresh, which the next event or the
/// next render makes good) or put it back behind the others and look at the
/// next one. Requeuing moves that event behind whatever was after it, which
/// is the price of not losing it; a full channel is already a degraded moment
/// and order among refreshes is not what the app is waiting on.
///
/// The walk stops once it has been all the way round. Every slot holding a
/// settling event means there is none that can be freed honestly, so the walk
/// ends with nothing spent and the newcomer still in the caller's hands — to
/// hold and retry, not to drop, since it is awaited exactly like the eight it
/// could not displace. Stopping *before* the repeat rather than after it is
/// what leaves the ring exactly as the walk found it: each slot has been
/// requeued once, which is a full rotation.
///
/// RAM: two usizes, beside the one `LibraryEvent` the caller already holds
/// while it decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvictionWalk {
    inspected: usize,
    capacity: usize,
}

/// What the caller does with the event it just took off the front.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictionStep {
    /// Only a refresh: drop it, and the slot it frees is the newcomer's.
    Discard,
    /// It settles a wait too. Put it back and take the next one.
    Requeue,
}

impl EvictionWalk {
    pub const fn new(capacity: usize) -> Self {
        Self {
            inspected: 0,
            capacity,
        }
    }

    /// Whether the walk has been all the way round. Checked before taking
    /// another event, so a walk that finds nothing to spend never disturbs
    /// the ring it gave up on.
    pub const fn exhausted(&self) -> bool {
        self.inspected >= self.capacity
    }

    /// Decide what to do with `head`, the event just taken off the front.
    pub fn inspect(&mut self, head: &LibraryEvent) -> EvictionStep {
        self.inspected += 1;
        if head.must_be_delivered() {
            EvictionStep::Requeue
        } else {
            EvictionStep::Discard
        }
    }
}

/// Slots in the display-event channel. Lives here so the firmware and the
/// model test below size the same channel.
pub const DISPLAY_EVENT_SLOTS: usize = 8;

/// The render acknowledgement that had nowhere to go, waiting for room.
///
/// The display-event channel used to be made room in by walking it, the way
/// [`EvictionWalk`] walks the library channel. There is nothing here worth
/// spending: the informational events take the lossy path, and everything
/// else is either an acknowledgement the app is waiting on or a library event
/// carrying its own rule. A walk would also have to put back what it may not
/// spend, at the tail, behind everything it has not inspected — and this queue
/// is the one the app reads its render acknowledgements from in order.
///
/// So the queue is left alone and the acknowledgement waits instead, placed by
/// a branch of the display task's select once the app drains a slot — the same
/// answer as [`LibraryEventHolder`], for the same reason.
///
/// Unlike that one, this holder gates nothing. The producer of
/// acknowledgements is the render arm of the display loop, and refusing
/// renders while one is held would deadlock: the app blocks on handing over a
/// render, and servicing renders is exactly what frees the app to drain the
/// channel this event is waiting for.
///
/// RAM: one `Option<DisplayEvent>` — 280 bytes, sized by the `Library`
/// variant. Sits in the firmware's `.bss`, not on a stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayEventHolder {
    held: Option<DisplayEvent>,
}

/// What [`DisplayEventHolder::hold`] did with the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayHoldOutcome {
    /// Taken. The caller's placing branch delivers it once there is room.
    Held,
    /// Not an acknowledgement the app is waiting on. Send it the lossy way.
    NotRequired,
    /// Occupied by an acknowledgement that has been waiting longer. Both end
    /// the render cycle and the app clears its render lock on either, so the
    /// one already waiting is enough and the newcomer may be the loss.
    Occupied,
}

impl DisplayEventHolder {
    pub const fn new() -> Self {
        Self { held: None }
    }

    /// The acknowledgement awaiting a slot, if any. Peeking, not taking: it
    /// stays held until [`Self::placed`] says it landed, so a cancelled send
    /// cannot lose it.
    pub const fn pending(&self) -> Option<DisplayEvent> {
        self.held
    }

    /// Take responsibility for an acknowledgement the channel refused.
    pub fn hold(&mut self, event: &DisplayEvent) -> DisplayHoldOutcome {
        if !event.must_be_delivered() {
            return DisplayHoldOutcome::NotRequired;
        }
        if self.held.is_some() {
            return DisplayHoldOutcome::Occupied;
        }
        self.held = Some(*event);
        DisplayHoldOutcome::Held
    }

    /// The held event reached the channel.
    pub fn placed(&mut self) -> Option<DisplayEvent> {
        self.held.take()
    }
}

impl DisplayEvent {
    /// Whether dropping this would strand the app rather than cost it a log
    /// line.
    ///
    /// `Settled` and `RefreshFailed` end the render cycle: the app clears its
    /// render lock on them, drains its parked storage and releases a deferred
    /// sleep. Nothing reissues them, so a dropped one leaves every later state
    /// change merely pending, with no render left to be acknowledged.
    ///
    /// `Asleep` and `SleepFailed` are notifications. The handshake the power
    /// task actually waits on travels over its own channel and is sent beside
    /// each of these; the app only logs them. Dropping one costs the log line.
    ///
    /// `Library` carries the library event's own answer, so an event that
    /// settles a wait is protected whichever channel it is travelling on.
    pub const fn must_be_delivered(&self) -> bool {
        match self {
            Self::Settled { .. } | Self::RefreshFailed => true,
            Self::Asleep | Self::SleepFailed => false,
            Self::Library(event) => event.must_be_delivered(),
        }
    }
}

/// The one settling event that had nowhere to go, and the standing orders the
/// display task owes it while it waits.
///
/// When [`EvictionWalk`] finds nothing it may honestly spend, the event is
/// neither queued nor dropped — it is held here until the app drains a slot.
/// That single occupied slot then constrains the whole task, because the way
/// it empties is the app receiving from a channel the display task is not
/// currently feeding:
///
/// - **Storage stands down.** Storage is where settling events come from, and
///   there is one slot; applying another command could produce a second with
///   nowhere to go.
/// - **Sleep waits.** Sleep is terminal here — waking is a fresh boot — so
///   going down with an event held would take it along and strand the wait it
///   was going to settle.
/// - **Library events stay in the display-event channel.** Moving one across
///   to make room is only free while the holder can catch it; occupied, the
///   move would arrive with nowhere to go.
///
/// Those are three readings of one rule, which is why they live together: they
/// were three separate `if`s in the task before this type, and two of them had
/// already drifted out of agreement by the time a reviewer noticed. The fourth
/// rule is [`Self::hold`]'s own — it accepts only what it is for.
///
/// RAM: one `Option<LibraryEvent>` — 276 bytes, sized by the `Loaded`
/// variant's chapter-page map. Sits in the firmware's `.bss`, not on a stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibraryEventHolder {
    held: Option<LibraryEvent>,
}

/// What [`LibraryEventHolder::hold`] did with the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldOutcome {
    /// Taken. The caller's placing branch delivers it once there is room.
    Held,
    /// A refresh, which the next event or the next render makes good. It was
    /// never the holder's to protect, and letting one in would close every
    /// gate above on an event nothing is waiting for. Send it the lossy way.
    NotSettling,
    /// Occupied by an event that has been waiting longer. Every producer is
    /// gated on the holder, so reaching here means one storage command
    /// produced two settling events. The newcomer gets a last try at the
    /// channel and is the loss if that fails — the older wait survives.
    Occupied,
}

impl LibraryEventHolder {
    pub const fn new() -> Self {
        Self { held: None }
    }

    /// The event awaiting a slot, if any. Peeking, not taking: it stays held
    /// until [`Self::placed`] says it actually landed, so a cancelled send
    /// cannot lose it.
    pub const fn pending(&self) -> Option<LibraryEvent> {
        self.held
    }

    /// May the task apply another storage command?
    pub const fn storage_may_run(&self) -> bool {
        self.held.is_none()
    }

    /// May the task sleep, or keep draining storage on the way down?
    pub const fn sleep_may_proceed(&self) -> bool {
        self.held.is_none()
    }

    /// May a library event be moved out of the display-event channel to make
    /// room there?
    pub const fn library_event_may_move(&self) -> bool {
        self.held.is_none()
    }

    /// Take responsibility for an event the library channel refused.
    pub fn hold(&mut self, event: &LibraryEvent) -> HoldOutcome {
        if !event.must_be_delivered() {
            return HoldOutcome::NotSettling;
        }
        if self.held.is_some() {
            return HoldOutcome::Occupied;
        }
        self.held = Some(*event);
        HoldOutcome::Held
    }

    /// The held event reached the channel. Returns it so a caller can log
    /// what it placed; the holder is empty either way.
    pub fn placed(&mut self) -> Option<LibraryEvent> {
        self.held.take()
    }
}

impl LibraryEvent {
    /// Whether dropping this event would strand the app rather than cost it a
    /// repaint.
    ///
    /// The library-event channel is small and lossy on purpose: most of what
    /// crosses it is a refresh, and a dropped one is made good by the next
    /// event or the next render. These three are not refreshes. Each one
    /// releases a lock the app took when it handed the work over, and nothing
    /// reissues them — the storage task has already done the work and moved
    /// on, so a dropped one leaves the app waiting for the rest of the visit:
    ///
    /// - `Loaded` and `BookOpenFailed` are the two ways an open ends, and the
    ///   app suppresses input until one of them arrives.
    /// - `CacheCleared` settles a per-book action's `LibraryMenu::Busy`, which
    ///   holds the whole Library list still while it waits.
    /// - `Restored` is what the boot render waits for before drawing.
    ///
    /// The senders route on this, so an event that settles something is
    /// protected by naming it here rather than by every call site
    /// remembering which function to reach for.
    pub const fn must_be_delivered(&self) -> bool {
        matches!(
            self,
            Self::Loaded { .. }
                | Self::BookOpenFailed { .. }
                | Self::CacheCleared { .. }
                | Self::Restored { .. }
                // The three that settle a `LibraryBrowse`. Dropping one
                // leaves the Library rail waiting on a move that already
                // happened, with no second press able to start another.
                | Self::FolderListed { .. }
                | Self::RowIsBook { .. }
                | Self::RowFailed { .. }
                | Self::LibraryUnreadable { .. }
        )
    }
}

/// The Library's per-book actions sheet and its follow-through, one step
/// at a time: the side browse key opens the sheet on the selected row,
/// the browse keys move its cursor, Confirm executes the action, and the
/// settled result lingers as a note until the next press. Picking a row
/// on the sheet is itself the deliberate step, so recoverable actions run
/// on that one Confirm; an irreversible action (deletion, when it joins)
/// adds its own per-action confirm stage. One state, because at most one
/// of these is ever on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LibraryMenu {
    #[default]
    None,
    /// The actions sheet is up; `row` is its cursor into [`LIBRARY_ACTIONS`].
    Sheet { row: u8 },
    /// `action`'s storage command is in flight for catalog row `index`; the
    /// `LibraryEvent` echoing `request_id` settles it.
    ///
    /// The request id is what matches the answer to the question. Leaving
    /// Library drops `Busy` but not the command it launched, so a second
    /// action can be started — on the same row as the first, even — with both
    /// still in flight; only an id distinguishes them. `index` rides along
    /// because the note is about a book, not because it identifies anything.
    /// While `Busy` is up the Library list is frozen (see `apply_input`), so
    /// at most one action per visit can be launched.
    Busy {
        action: LibraryAction,
        index: u16,
        request_id: u32,
    },
    /// Transient result note; the next press dismisses it.
    Done { action: LibraryAction, ok: bool },
}

/// Per-book actions the Library sheet offers. Slice 1 of the
/// file-management PRD ships cache-clearing; delete and move join here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryAction {
    ClearCache,
}

/// A Library row press waiting on the card.
///
/// A row is a book or a folder, and only the card knows which: the app holds
/// a count, not a listing. So a press asks, and the answer decides what
/// happens. Both waits look the same from the app's side, which is why they
/// are one type: something is in flight, the rail says so, and no second
/// press starts another one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryBrowse {
    /// Nothing in flight.
    Idle,
    /// This row was chosen, and storage is resolving it.
    Choosing {
        index: u16,
        request_id: u32,
        /// The position the rows were counted in when the press happened. A
        /// scan in between takes the storage task back to the library root,
        /// and a listing from the newer position outranks a move from the
        /// older one, whose row number now means something else.
        browse_epoch: u32,
    },
    /// Back was pressed below the root, and storage is listing the parent.
    Leaving { request_id: u32, browse_epoch: u32 },
}

impl LibraryBrowse {
    /// The id of the command this is waiting on, if any.
    pub const fn request_id(self) -> Option<u32> {
        match self {
            Self::Idle => None,
            Self::Choosing { request_id, .. } | Self::Leaving { request_id, .. } => {
                Some(request_id)
            }
        }
    }

    /// The position generation this move was issued against, if any.
    pub const fn browse_epoch(self) -> Option<u32> {
        match self {
            Self::Idle => None,
            Self::Choosing { browse_epoch, .. } | Self::Leaving { browse_epoch, .. } => {
                Some(browse_epoch)
            }
        }
    }

    pub const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// The sheet's rows, top to bottom.
pub const LIBRARY_ACTIONS: &[LibraryAction] = &[LibraryAction::ClearCache];

/// Wi-Fi session lifecycle as shown on the Wireless screen. The wifi task
/// owns the radio and reports transitions back as `SyncEvent`s; the reducer
/// only records what the screen should say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    /// No Wi-Fi network is saved; Confirm starts the onboarding hotspot.
    NotConfigured,
    /// A network is saved and the radio is untouched; Confirm connects.
    Idle,
    /// "Forget this network" awaits its confirmation: Confirm deletes the
    /// saved credentials, Back cancels. Only reachable from Idle, so the
    /// radio is still untouched.
    ForgetPending,
    /// Confirm was pressed: the app shell must emit `SyncCommand::Start`.
    Starting,
    Connecting,
    /// Joined and DHCP-configured with this IPv4 address.
    Connected([u8; 4]),
    /// The onboarding hotspot is up; the screen renders the join QR and
    /// manual-join password from this session's PSK.
    PortalUp(PortalPsk, PortalSsid),
    /// Connected and the book server answers at this address until the
    /// session ends.
    Serving([u8; 4]),
    /// The portal captured and stored credentials; a fresh session will
    /// use them after the reset.
    CredentialsSaved,
    Error(SyncError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncError {
    RadioInit,
    Join,
    Dhcp,
    /// The pre-loan flush of the coalesced reading position failed, so the
    /// display task refused to dismantle the reader scratch over an unsaved
    /// position. Nothing was loaned; Confirm retries the session.
    Storage,
}

/// wifi task -> app task progress reports for the Wireless screen. The
/// display task also sends `NetworkSaved` once at boot, after reading
/// /READER/WIFI.BIN, so the screen can name the saved network before any
/// session starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    /// A saved network exists (on the card or compiled in); the screen
    /// shows its name and offers connect/forget.
    NetworkSaved(WifiSsid),
    Connecting,
    Connected([u8; 4]),
    /// The onboarding hotspot is up, secured with this session's PSK.
    PortalUp(PortalPsk, PortalSsid),
    Serving([u8; 4]),
    CredentialsSaved(WifiSsid),
    Failed(SyncError),
}

/// app task -> wifi task session control. Starting a session loans reader
/// memory to the radio irrevocably; Exit therefore maps to a software reset
/// on hardware once a session has started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncCommand {
    Start,
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    /// User input landed; carries the view the input left the app in so the
    /// power task can tier its idle timeout (long leash while Reading,
    /// short on the shell views).
    Activity(AppView),
    DisplaySettled,
    /// The panel completed the sleep handshake for the identified
    /// `DisplayCommand::Sleep` generation; only the matching handshake may
    /// cut power on it.
    DisplayAsleep(u32),
    /// A panel refresh (render flush or wake init) did not complete. A
    /// refresh failure can belong to a render queued ahead of a Sleep
    /// command, and the sleep handshake must not be abandoned over
    /// someone else's frame.
    DisplayRefreshFailed,
    /// The display task could not complete the identified sleep request
    /// (progress flush or panel handshake failed); the power task must
    /// stay awake — never cut power behind a failed sleep handshake. The
    /// generation lets a later handshake ignore a stale failure from a
    /// sleep it abandoned on `Activity`.
    DisplaySleepFailed(u32),
    SleepNow,
}

/// Keep the display and power acknowledgements for a panel refresh paired.
/// A failed transfer must never advance the app render queue or authorize a
/// later power transition as though the panel had settled.
pub const fn display_refresh_outcome(
    success: bool,
    chapter_cursor: Option<ChapterCursor>,
) -> (DisplayEvent, PowerEvent) {
    if success {
        (
            DisplayEvent::Settled { chapter_cursor },
            PowerEvent::DisplaySettled,
        )
    } else {
        // A frame that never landed corrects nothing: the cursor is read from
        // the page that was shown.
        (
            DisplayEvent::RefreshFailed,
            PowerEvent::DisplayRefreshFailed,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersistedAppState {
    pub book_id: u32,
    pub chapter: u16,
    pub screen: u32,
    pub shell_orientation: u8,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
    pub font_size: u8,
    pub line_spacing: u8,
    pub font_weight: u8,
    pub font_family: u8,
    pub front_buttons: u8,
    pub source_hash: u32,
    pub source_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReducerContext {
    pub builtin_book_count: u8,
    pub builtin_chapter_count: u8,
}

impl ReducerContext {
    pub const fn new(builtin_book_count: u8, builtin_chapter_count: u8) -> Self {
        Self {
            builtin_book_count,
            builtin_chapter_count,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderState {
    pub view: AppView,
    pub page: u32,
    pub selection: u16,
    pub chapter: u16,
    pub book_id: u32,
    pub orientation: DisplayOrientation,
    pub front_buttons: FrontButtons,
    pub refresh_policy: RefreshPolicy,
    pub font_size: FontSize,
    pub line_spacing: LineSpacing,
    pub font_weight: FontWeight,
    pub font_family: FontFamily,
    pub custom_font_available: bool,
    pub last_button: Option<Button>,
    pub aux_raw: u16,
    pub nav_raw: u16,
    pub page_raw: u16,
    pub battery_mv: u16,
    pub battery_percent: u8,
    pub library_count: u16,
    /// How many of `library_count` are books. The Library list shows a
    /// folder's books above its folders, so this one number says which kind
    /// any row is, without the app holding a row-by-row copy of a listing
    /// that slides as the reader scrolls.
    pub library_books: u16,
    /// How far below the library root the list is. Zero is the root, where
    /// Back leaves for Home rather than going up.
    pub library_depth: u8,
    /// A Library row press waiting on storage to say what it was.
    pub library_browse: LibraryBrowse,
    /// The position the resident rows were counted in, as reported by the
    /// last listing. Row-addressed browse commands carry it so the storage
    /// task can tell a row picked where it is standing from one picked
    /// somewhere it has since left.
    pub library_browse_epoch: u32,
    /// The catalog the Library list is currently drawn from, as reported by
    /// the last [`LibraryEvent::Scanned`]. Row-addressed storage commands
    /// carry it so the storage task can tell a live row from a stale one.
    pub catalog_epoch: u32,
    pub sd_page_count: u32,
    pub sd_chapter_count: u16,
    pub sd_chapter_pages: [u16; MAX_SD_CHAPTERS],
    pub read_request_pending: bool,
    /// Portrait reading's summoned key sheet is up: the next named-key
    /// press acts on the label it revealed instead of summoning again.
    pub reading_sheet: bool,
    /// The Library per-book actions sheet's progress on the selected row.
    pub library_menu: LibraryMenu,
    /// Ids handed to per-book actions, one per pick. Lives beside the menu
    /// rather than inside it because it has to outlive the wait it named:
    /// `Busy` is dropped whenever the reader leaves Library, and the next
    /// pick must still not reuse the id of a command that is out there
    /// unanswered.
    pub library_request_seq: u32,
    pub sync_status: SyncStatus,
    /// The saved Wi-Fi network's name; len 0 means none is saved. Fed by
    /// `SyncEvent::NetworkSaved` at boot and `CredentialsSaved` from the
    /// portal, cleared by the forget flow.
    pub wifi_ssid: [u8; 32],
    pub wifi_ssid_len: u8,
    pub dirty: Rect,
}

impl ReaderState {
    pub const fn boot() -> Self {
        Self {
            view: AppView::Home,
            page: 0,
            selection: 0,
            chapter: 0,
            book_id: 1,
            // Calendula boots into the portrait hold. It is the same
            // `PortraitButtonsLeft` the settings cycle offers, not the
            // format-only buttons-above variant, so the first Change press
            // leaves for landscape and the cycle can return here.
            orientation: DisplayOrientation::PortraitButtonsLeft,
            front_buttons: FrontButtons::PagesRight,
            refresh_policy: RefreshPolicy::FullOnWake,
            font_size: FontSize::Medium,
            line_spacing: LineSpacing::Normal,
            font_weight: FontWeight::Normal,
            font_family: FontFamily::Literata,
            custom_font_available: false,
            last_button: None,
            aux_raw: 0,
            nav_raw: 0,
            page_raw: 0,
            battery_mv: 0,
            battery_percent: 100,
            library_count: 0,
            library_books: 0,
            library_depth: 0,
            library_browse: LibraryBrowse::Idle,
            library_browse_epoch: 0,
            catalog_epoch: 0,
            sd_page_count: 1,
            sd_chapter_count: 1,
            sd_chapter_pages: [0; MAX_SD_CHAPTERS],
            read_request_pending: false,
            reading_sheet: false,
            library_menu: LibraryMenu::None,
            library_request_seq: 0,
            sync_status: SyncStatus::NotConfigured,
            wifi_ssid: [0; 32],
            wifi_ssid_len: 0,
            dirty: Rect::FULL,
        }
    }

    /// The saved network's name; empty when none is saved.
    pub fn wifi_ssid(&self) -> &str {
        core::str::from_utf8(&self.wifi_ssid[..self.wifi_ssid_len.min(32) as usize]).unwrap_or("")
    }

    pub fn wifi_network_saved(&self) -> bool {
        self.wifi_ssid_len > 0
    }

    pub fn apply_input(self, ctx: ReducerContext, event: InputEvent) -> Self {
        let InputEvent::Sample {
            button: raw_button,
            aux_raw,
            nav_raw,
            page_raw,
            battery_mv,
            battery_percent,
        } = event;
        let button = orient_button(
            self.orientation,
            swap_front_pairs(self.front_buttons, raw_button),
        );
        // Home is positional, not grammatical: its four actions direct-map
        // the physical key column, with the ordering itself ranked (continue
        // second from the top). The front-pair swap moves roles to the
        // resting thumb, but Home has no roles to move -- riding the swap
        // would demote continue to the far end and put wireless and settings
        // on the comfortable pair. So Home maps from the un-swapped keys and
        // reads the same for every user.
        let home_button = orient_button(self.orientation, raw_button);
        let mut next = self;
        next.last_button = button;
        next.aux_raw = aux_raw;
        next.nav_raw = nav_raw;
        next.page_raw = page_raw;
        next.battery_mv = battery_mv;
        next.battery_percent = battery_percent;
        next.dirty = Rect::FULL;

        // Portrait reading is full-bleed: the first named-key press summons
        // the key sheet above the buttons (the margin appears when called
        // for); the second press acts on the label it revealed. Page turns
        // never wait on the sheet -- reading momentum would make a turn a
        // second press -- so the browse pair acts at once, dismissing it.
        // Landscape keeps its direct mapping.
        if self.view == AppView::Reading && is_portrait(self.orientation) {
            match button {
                Some(Button::Confirm | Button::Back) if !self.reading_sheet => {
                    next.reading_sheet = true;
                    return next;
                }
                Some(Button::Power) | None => {}
                Some(_) => next.reading_sheet = false,
            }
        }

        // A settled per-book-action note lingers until the next press
        // acknowledges it; the press itself still acts normally.
        if matches!(self.library_menu, LibraryMenu::Done { .. }) {
            next.library_menu = LibraryMenu::None;
        }

        // The Library actions sheet and the wait that follows a pick run
        // their own grammar: while either is up, no press may fall through
        // to move the cursor or open a book — the same press must never
        // both answer the sheet and act on the list beneath it, and none
        // may disturb a row the storage task is working on.
        if self.view == AppView::Library {
            if let LibraryMenu::Sheet { row } = self.library_menu {
                match button {
                    // Opening the sheet and confirming a labeled row is the
                    // deliberate two-step; the pick executes. Irreversible
                    // actions add their own confirm stage when they join.
                    Some(Button::Confirm) if self.selection < self.library_count => {
                        // A sheet with no actions has nothing to pick, and
                        // the modulo and the index would both abort on the
                        // way to finding that out. Not reachable while
                        // LIBRARY_ACTIONS is a fixed non-empty const, but
                        // this is a release build that aborts on panic.
                        let Some(&action) =
                            LIBRARY_ACTIONS.get(row as usize % LIBRARY_ACTIONS.len().max(1))
                        else {
                            return next;
                        };
                        // A fresh id per pick, never reused, so an answer to
                        // an abandoned clear cannot settle this one.
                        next.library_request_seq = self.library_request_seq.wrapping_add(1);
                        next.library_menu = LibraryMenu::Busy {
                            action,
                            index: self.selection,
                            request_id: next.library_request_seq,
                        };
                        return next;
                    }
                    Some(Button::Next | Button::PageNext) => {
                        let row = wrap_next(row as u16, LIBRARY_ACTIONS.len().max(1) as u16) as u8;
                        next.library_menu = LibraryMenu::Sheet { row };
                        return next;
                    }
                    Some(Button::Previous) => {
                        let row = wrap_prev(row as u16, LIBRARY_ACTIONS.len().max(1) as u16) as u8;
                        next.library_menu = LibraryMenu::Sheet { row };
                        return next;
                    }
                    Some(Button::Power) | None => {}
                    // Back and the summoning side key both dismiss.
                    Some(_) => {
                        next.library_menu = LibraryMenu::None;
                        return next;
                    }
                }
            }
            // A picked action holds the list still until it settles. The
            // storage task is mid-operation on the selected row: a press
            // that fell through here could move the cursor off the row the
            // note will name, open a book whose cache is being deleted, or
            // pick a second action while the first is unanswered. Back is
            // the deliberate exception — leaving Library is always allowed,
            // and it drops the claim to the answer along with the state.
            if matches!(self.library_menu, LibraryMenu::Busy { .. }) {
                match button {
                    Some(Button::Back) | Some(Button::Power) | None => {}
                    Some(_) => return next,
                }
            }
            // A move through the tree holds the list still for the same
            // reason: the rows are about to be replaced, so a press against
            // the ones on screen would act on a listing that is already
            // gone. Back is the same deliberate exception, and it leaves
            // Library outright rather than stacking a second move: the
            // `library_browse` reset below drops the claim to the answer.
            if !self.library_browse.is_idle() {
                match button {
                    Some(Button::Back) => {
                        next.library_browse = LibraryBrowse::Idle;
                        next.view = AppView::Home;
                        next.selection = 0;
                        next.read_request_pending = false;
                        return next;
                    }
                    Some(Button::Power) | None => {}
                    Some(_) => return next,
                }
            }
        }

        match (self.view, button) {
            (_, None) => {}
            (_, Some(Button::Power)) => {}
            (AppView::Home, Some(_)) => {
                if let Some(home_button) = home_button {
                    next = apply_home_action(next, home_action_for_button(home_button));
                }
            }
            (AppView::Library, Some(Button::Next | Button::PageNext)) => {
                next.selection = wrap_next(self.selection, self.library_item_count(ctx));
            }
            (AppView::Library, Some(Button::Previous)) => {
                next.selection = wrap_prev(self.selection, self.library_item_count(ctx));
            }
            // The side browse key is a navigation alias everywhere else; in
            // Library it opens the per-book actions sheet instead, the same
            // key that arms "forget" on the Wireless screen. Only a real
            // catalog row has actions.
            (AppView::Library, Some(Button::PagePrevious)) => {
                // Books have actions; a folder is a place, not a book, and
                // "clear cache" means nothing on it.
                if self.selection < self.library_books {
                    next.library_menu = LibraryMenu::Sheet { row: 0 };
                }
            }
            // Imprint key grammar: Back always zooms out one level,
            // Confirm always affirms the screen's primary action.
            (AppView::Library, Some(Button::Confirm)) => {
                if self.selection < self.library_count {
                    // The app holds a row count, not a listing, so the card
                    // decides what this row is and acts on it. A fresh id per
                    // press, never reused, so an answer to a press the reader
                    // walked away from cannot settle this one.
                    next.library_request_seq = self.library_request_seq.wrapping_add(1);
                    next.library_browse = LibraryBrowse::Choosing {
                        index: self.selection,
                        request_id: next.library_request_seq,
                        browse_epoch: self.library_browse_epoch,
                    };
                }
            }
            (AppView::Library, Some(Button::Back)) => {
                // A picked action holds every other press, and Back is the
                // one way out of that wait. Inside a folder it would
                // otherwise spend itself on the folder and leave the reader
                // held with nothing that answers.
                let held = matches!(self.library_menu, LibraryMenu::Busy { .. });
                if self.library_depth > 0 && !held {
                    // Back zooms out one level, and below the root a level is
                    // a folder rather than the whole screen.
                    next.library_request_seq = self.library_request_seq.wrapping_add(1);
                    next.library_browse = LibraryBrowse::Leaving {
                        request_id: next.library_request_seq,
                        browse_epoch: self.library_browse_epoch,
                    };
                } else {
                    next.view = AppView::Home;
                    next.selection = 0;
                    next.read_request_pending = false;
                }
            }
            (AppView::Reading, Some(Button::Next | Button::PageNext)) => {
                if ReaderSource::from_book_id(self.book_id).is_sd() {
                    if self.page + 1 < self.sd_page_count {
                        next.page = self.page + 1;
                    } else {
                        next.page = self.sd_page_count.saturating_sub(1);
                    }
                    next.chapter = next.sd_chapter_for_page(next.page);
                    next.selection = next.chapter;
                } else {
                    next.chapter =
                        wrap_next(self.chapter, (ctx.builtin_chapter_count as u16).max(1));
                    next.selection = next.chapter;
                    next.page = 0;
                }
            }
            (AppView::Reading, Some(Button::Previous | Button::PagePrevious)) => {
                if ReaderSource::from_book_id(self.book_id).is_sd() {
                    if self.page > 0 {
                        next.page = self.page - 1;
                    }
                    next.chapter = next.sd_chapter_for_page(next.page);
                    next.selection = next.chapter;
                } else {
                    next.chapter =
                        wrap_prev(self.chapter, (ctx.builtin_chapter_count as u16).max(1));
                    next.selection = next.chapter;
                    next.page = 0;
                }
            }
            (AppView::Reading, Some(Button::Confirm)) => {
                next.view = AppView::Chapters;
                // `chapter` already tracks the reading position (kept current
                // by the firmware's Loaded event, un-capped); opening the list
                // lands the cursor there rather than on the saturated guess.
                next.selection = self.chapter;
            }
            (AppView::Reading, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 0;
            }
            (AppView::Chapters, Some(Button::Next | Button::PageNext)) => {
                next.selection = wrap_next(self.selection, self.chapter_item_count(ctx));
            }
            (AppView::Chapters, Some(Button::Previous | Button::PagePrevious)) => {
                next.selection = wrap_prev(self.selection, self.chapter_item_count(ctx));
            }
            (AppView::Chapters, Some(Button::Confirm)) => {
                next.chapter = self.selection;
                next.page = if ReaderSource::from_book_id(self.book_id).is_sd() {
                    u32::from(
                        self.sd_chapter_pages
                            .get(self.selection as usize)
                            .copied()
                            .unwrap_or(0),
                    )
                } else {
                    0
                };
                next.view = AppView::Reading;
            }
            (AppView::Chapters, Some(Button::Back)) => {
                next.view = AppView::Reading;
            }
            (AppView::Wireless, Some(Button::Confirm)) => match self.sync_status {
                // NotConfigured starts too: with no stored or built-in
                // credentials the wifi task answers with the onboarding
                // portal instead of a station join.
                SyncStatus::NotConfigured | SyncStatus::Idle | SyncStatus::Error(_) => {
                    next.sync_status = SyncStatus::Starting;
                }
                // Confirm affirms the forget: the app shell deletes
                // /READER/WIFI.BIN on this transition.
                SyncStatus::ForgetPending => {
                    next.wifi_ssid = [0; 32];
                    next.wifi_ssid_len = 0;
                    next.sync_status = SyncStatus::NotConfigured;
                }
                SyncStatus::CredentialsSaved | SyncStatus::Serving(_) => {
                    next.view = AppView::Home;
                    next.selection = 0;
                    next.sync_status = next.wireless_entry_status();
                }
                // An in-flight session ignores Confirm until it lands in
                // Serving, CredentialsSaved, or Error.
                _ => {}
            },
            (AppView::Wireless, Some(Button::Back)) => {
                // Back zooms out one level: a pending forget falls back to
                // the idle screen rather than leaving the view.
                if self.sync_status == SyncStatus::ForgetPending {
                    next.sync_status = SyncStatus::Idle;
                } else {
                    // Leaving after the radio started maps to
                    // SyncCommand::Exit in the app shell, which resets the
                    // device; the reducer still returns Home so the
                    // emulator stays navigable.
                    next.view = AppView::Home;
                    next.selection = 0;
                    next.sync_status = next.wireless_entry_status();
                }
            }
            (AppView::Wireless, Some(Button::Previous | Button::PagePrevious)) => {
                // The browse key doubles as "forget" while idle; the
                // destructive step still needs its Confirm.
                if self.sync_status == SyncStatus::Idle {
                    next.sync_status = SyncStatus::ForgetPending;
                }
            }
            (AppView::Wireless, Some(Button::Next | Button::PageNext)) => {}
            (AppView::Settings, Some(Button::Next | Button::PageNext)) => {
                next.selection = wrap_next(self.selection, SETTINGS_ITEMS as u16);
            }
            (AppView::Settings, Some(Button::Previous | Button::PagePrevious)) => {
                next.selection = wrap_prev(self.selection, SETTINGS_ITEMS as u16);
            }
            (AppView::Settings, Some(Button::Confirm)) => {
                next = apply_setting(next);
            }
            (AppView::Settings, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 0;
            }
        }

        // The sheet is a reading-surface state; leaving the page (or the
        // posture that summons it) always drops it.
        if next.view != AppView::Reading || !is_portrait(next.orientation) {
            next.reading_sheet = false;
        }
        // The actions sheet is a Library-surface state; leaving the screen
        // drops it (an in-flight Busy settles silently — the note only
        // shows to someone still looking at Library).
        if next.view != AppView::Library {
            next.library_menu = LibraryMenu::None;
            // And so is a move through the tree. The command is still out
            // there and its answer still comes back; dropping the wait here
            // is what makes that answer land on nobody, which is what the
            // request id in every reply is for.
            next.library_browse = LibraryBrowse::Idle;
        }

        next
    }

    /// Adopt the firmware's uncapped chapter for the page just shown.
    ///
    /// Silent: the Reading view shows page-within-chapter, not the chapter
    /// itself, so no repaint is owed — Home, the sleep screen, Chapters and
    /// the persisted position pick the corrected value up when next used.
    /// That is also why it can ride with the acknowledgement instead of
    /// forcing a render of its own.
    ///
    /// Applied before the render lock is cleared, since clearing it is what
    /// lets the next navigation read [`Self::chapter`].
    ///
    /// Only onto the frame it describes. A press lands while its render is
    /// still in flight — the reducer runs and only the repaint is held back —
    /// so an acknowledgement can arrive for a page the reader has already left,
    /// and adopting its chapter there would pair one page's number with
    /// another's chapter. Nothing is lost by declining: the page moved to has
    /// its own render coming, with its own correction.
    pub fn apply_chapter_cursor(mut self, cursor: ChapterCursor) -> Self {
        if self.book_id == cursor.book_id && self.page == cursor.page {
            self.chapter = cursor.current_chapter;
        }
        self
    }

    pub fn apply_library_event(mut self, ctx: ReducerContext, event: LibraryEvent) -> Self {
        match event {
            LibraryEvent::Scanned {
                count,
                catalog_epoch,
            } => {
                self.library_count = count;
                // Row numbers are only meaningful inside one epoch; adopt it
                // so anything the user picks from this list is tagged with
                // the catalog the list was drawn from.
                self.catalog_epoch = catalog_epoch;
                // Boot points at the built-in demo book until the scan
                // proves the card has real books; the title page then
                // adopts the first catalog entry instead of the
                // placeholder. Saved progress (Restored) arrives after
                // and overrides this default, and a demo book that is
                // actually open stays put.
                if count > 0
                    && !ReaderSource::from_book_id(self.book_id).is_sd()
                    && !matches!(self.view, AppView::Reading | AppView::Chapters)
                {
                    self.book_id = ReaderSource::sd(0).book_id();
                    self.chapter = 0;
                    self.page = 0;
                    self.dirty = Rect::FULL;
                }
                if self.view == AppView::Library {
                    if count == 0 {
                        self.selection = 0;
                    } else if self.selection >= count {
                        self.selection = count - 1;
                    }
                    self.dirty = Rect::FULL;
                    if self.read_request_pending {
                        self.read_request_pending = false;
                    }
                }
                // A fresh scan can reorder rows; an open sheet must not
                // carry over to whatever now sits at the cursor.
                if matches!(self.library_menu, LibraryMenu::Sheet { .. }) {
                    self.library_menu = LibraryMenu::None;
                }
            }
            LibraryEvent::Loaded {
                book_id,
                pages,
                chapters,
                current_chapter,
                chapter_pages,
                position,
                // Routing only: whether the panel owes a repaint is the app
                // task's question (see `loaded_repaints`), and every one of
                // these is folded either way.
                text_replaced: _,
            } => {
                if self.book_id == book_id {
                    self.sd_page_count = pages.max(1);
                    self.sd_chapter_count = chapters.max(1);
                    self.sd_chapter_pages = chapter_pages;
                    // A landing page the storage task chose (resume, chapter
                    // jump) replaces the page that was asked for; otherwise
                    // the app's own page stands and is only clamped to the
                    // book it now knows the length of.
                    self.page = position
                        .unwrap_or(self.page)
                        .min(self.sd_page_count.saturating_sub(1));
                    // The firmware owns the true current chapter over the whole
                    // book; adopt it so the cursor tracks past the cap that the
                    // page-turn recompute (sd_chapter_for_page) saturates at.
                    self.chapter = current_chapter;
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::BookOpenFailed { .. } => {
                // The app task owns the rollback: it is the only place that
                // still remembers which book the reader was on before the
                // open, so it applies `restore_after_failed_open` itself.
            }
            LibraryEvent::ChapterPage {
                book_id,
                chapter,
                page,
            } => {
                if self.book_id == book_id {
                    if let Some(slot) = self.sd_chapter_pages.get_mut(chapter as usize) {
                        *slot = page.min(u16::MAX as u32) as u16;
                    }
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::CustomFont { available } => {
                self.custom_font_available = available;
                if !available && self.font_family == FontFamily::Custom {
                    self.font_family = FontFamily::Literata;
                }
                self.dirty = Rect::FULL;
            }
            LibraryEvent::CacheCleared { request_id, ok } => {
                // Only someone still waiting on the Library screen sees the
                // note; leaving the screen dropped `Busy` and with it any
                // claim to the answer. The id has to match too: a clear
                // started, walked away from, and started again leaves an
                // older answer still in flight, and it reports on a command
                // this wait never issued — possibly one the storage task
                // refused as stale, which would read here as "cache not
                // cleared" for a clear that is still running.
                if let LibraryMenu::Busy {
                    action,
                    request_id: outstanding,
                    ..
                } = self.library_menu
                {
                    // Exhaustive for the same reason the command side is: an
                    // action added without a settle arm here would match no
                    // pattern and leave its wait up for good. Every action
                    // this event can answer has to be named.
                    let settles = match action {
                        LibraryAction::ClearCache => true,
                    };
                    if settles && outstanding == request_id {
                        self.library_menu = LibraryMenu::Done { action, ok };
                        self.dirty = Rect::FULL;
                    }
                }
            }
            // The three answers to a move. Each checks the id for the reason
            // `CacheCleared` does: a move walked away from leaves an older
            // answer in flight, and it describes a folder this wait never
            // asked about.
            LibraryEvent::FolderListed {
                request_id,
                browse_epoch,
                depth,
                count,
                books,
                selection,
            } => {
                let mine = match request_id {
                    Some(id) => self.library_browse.request_id() == Some(id),
                    // Adopted when nothing is in flight, and over a move issued
                    // from a position this listing has left: that move is
                    // already doomed, and holding the old rows until its
                    // refusal arrives would leave the screen naming a folder
                    // the storage task is not in.
                    None => match self.library_browse.browse_epoch() {
                        None => true,
                        Some(issued_in) => issued_in != browse_epoch,
                    },
                };
                if mine {
                    self.library_browse = LibraryBrowse::Idle;
                    self.library_browse_epoch = browse_epoch;
                    self.library_depth = depth;
                    self.library_count = count;
                    self.library_books = books.min(count);
                    // Where the storage task put the cursor: the top on the
                    // way in, and on the way out the row the folder was
                    // entered from, found by name.
                    self.selection = selection.min(count.saturating_sub(1));
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::RowIsBook {
                request_id,
                index,
                catalog_epoch,
            } => {
                // The row was resolved against a catalog the app may since
                // have been told was replaced, in which case the number it
                // carries names some other book. The open the transition
                // below owes is fenced too, because this event can arrive
                // before the `Scanned` that would have caught it here.
                let fresh = catalog_epoch == self.catalog_epoch;
                // Both readings answer one request, so both check that it is
                // the one still being waited on. A stale answer to a press
                // the reader has already moved past would otherwise end a
                // newer move, and the response to that move arrives to find
                // nothing waiting for it.
                let mine = self.library_browse.request_id() == Some(request_id);
                if !fresh && mine {
                    self.library_browse = LibraryBrowse::Idle;
                    self.dirty = Rect::FULL;
                }
                if fresh && mine {
                    self.library_browse = LibraryBrowse::Idle;
                    // What Confirm used to do the moment it was pressed, now
                    // that the row has a catalog number. The transition into
                    // Reading is what owes the open, and the caller dispatches
                    // it the same way it dispatches one from a keypress.
                    self.book_id = ReaderSource::sd(index).book_id();
                    self.view = AppView::Reading;
                    self.chapter = 0;
                    self.selection = 0;
                    self.page = 0;
                    self.sd_page_count = 1;
                    self.sd_chapter_count = 1;
                    self.sd_chapter_pages = [0; MAX_SD_CHAPTERS];
                    self.read_request_pending = false;
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::LibraryUnreadable { browse_epoch } => {
                // Nobody pressed anything, so this is read the way an
                // unsolicited listing is: adopted when nothing is in flight,
                // and over a move issued from the position this scan left.
                let mine = match self.library_browse.browse_epoch() {
                    None => true,
                    Some(issued_in) => issued_in != browse_epoch,
                };
                if mine {
                    self.library_browse = LibraryBrowse::Idle;
                    self.library_browse_epoch = browse_epoch;
                    self.library_depth = 0;
                    self.library_count = 0;
                    self.library_books = 0;
                    self.selection = 0;
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::RowFailed { request_id } => {
                if self.library_browse.request_id() == Some(request_id) {
                    // Nothing moved. The rows on screen are the rows that are
                    // there, and the repaint puts the rail back.
                    self.library_browse = LibraryBrowse::Idle;
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::Restored {
                book_id,
                chapter,
                page,
                page_count,
                reading_orientation,
                refresh_policy,
                font_size,
                line_spacing,
                font_weight,
                font_family,
                front_buttons,
            } => {
                self.book_id = book_id;
                self.chapter = chapter;
                self.page = page;
                // Give the Home progress bar a real denominator on wake, before
                // the book opens; the Loaded event refreshes it once read.
                if page_count > 0 {
                    self.sd_page_count = page_count;
                }
                if self.read_request_pending {
                    self.view = AppView::Reading;
                    self.selection = chapter;
                } else if self.view == AppView::Library {
                    let restored_index =
                        ReaderSource::from_book_id(book_id).sd_index().unwrap_or(0);
                    self.selection = restored_index.min(self.library_count.saturating_sub(1));
                } else if self.view == AppView::Chapters {
                    // Home/Settings keep their own key selection; only the
                    // chapter list tracks the restored chapter cursor.
                    self.selection = chapter;
                }
                self.read_request_pending = false;
                if let Some(orientation) = display_orientation_from_u8(reading_orientation) {
                    self.orientation = orientation;
                }
                if let Some(policy) = refresh_policy_from_u8(refresh_policy) {
                    self.refresh_policy = policy;
                }
                if let Some(size) = FontSize::from_u8(font_size) {
                    self.font_size = size;
                }
                if let Some(spacing) = LineSpacing::from_u8(line_spacing) {
                    self.line_spacing = spacing;
                }
                if let Some(weight) = FontWeight::from_u8(font_weight) {
                    self.font_weight = weight;
                }
                if let Some(family) = FontFamily::from_u8(font_family) {
                    self.font_family =
                        if family == FontFamily::Custom && !self.custom_font_available {
                            FontFamily::Literata
                        } else {
                            family
                        };
                }
                if let Some(front) = front_buttons_from_u8(front_buttons) {
                    self.front_buttons = front;
                }
                self.dirty = Rect::FULL;
            }
        }
        if self.view == AppView::Library {
            self.selection = self
                .selection
                .min(self.library_item_count(ctx).saturating_sub(1));
            self.dirty = Rect::FULL;
        }
        self
    }

    pub fn apply_sync_event(mut self, event: SyncEvent) -> Self {
        self.sync_status = match event {
            // The boot-time probe names the saved network without touching
            // the session; it only upgrades an untouched screen.
            SyncEvent::NetworkSaved(ssid) => {
                self.wifi_ssid = ssid.bytes;
                self.wifi_ssid_len = ssid.len;
                match self.sync_status {
                    SyncStatus::NotConfigured => SyncStatus::Idle,
                    status => status,
                }
            }
            SyncEvent::Connecting => SyncStatus::Connecting,
            SyncEvent::Connected(ip) => SyncStatus::Connected(ip),
            SyncEvent::PortalUp(psk, ssid) => SyncStatus::PortalUp(psk, ssid),
            SyncEvent::Serving(ip) => SyncStatus::Serving(ip),
            SyncEvent::CredentialsSaved(ssid) => {
                self.wifi_ssid = ssid.bytes;
                self.wifi_ssid_len = ssid.len;
                SyncStatus::CredentialsSaved
            }
            SyncEvent::Failed(error) => SyncStatus::Error(error),
        };
        self.dirty = Rect::FULL;
        self
    }

    /// What the Wireless screen shows on entry, before any session: the
    /// connect offer when a network is saved, the set-up offer otherwise.
    pub fn wireless_entry_status(&self) -> SyncStatus {
        if self.wifi_network_saved() {
            SyncStatus::Idle
        } else {
            SyncStatus::NotConfigured
        }
    }

    pub fn render_request(self, kind: RenderKind) -> RenderRequest {
        RenderRequest {
            kind,
            view: self.view,
            page: self.page,
            page_count: self.sd_page_count,
            chapter: self.chapter,
            selection: self.selection,
            book_id: self.book_id,
            orientation: self.orientation,
            front_buttons: self.front_buttons,
            reading_sheet: self.reading_sheet,
            library_menu: self.library_menu,
            library_move_pending: !self.library_browse.is_idle(),
            refresh_policy: self.refresh_policy,
            font_size: self.font_size,
            line_spacing: self.line_spacing,
            font_weight: self.font_weight,
            font_family: self.font_family,
            // The producer stamps this at the send site; app-core has no clock.
            requested_at_ms: 0,
            last_button: self.last_button,
            aux_raw: self.aux_raw,
            nav_raw: self.nav_raw,
            page_raw: self.page_raw,
            battery_mv: self.battery_mv,
            battery_percent: self.battery_percent,
            library_count: self.library_count,
            sync_status: self.sync_status,
            wifi_ssid: self.wifi_ssid,
            wifi_ssid_len: self.wifi_ssid_len,
            dirty: self.dirty,
        }
    }

    pub fn persisted(self) -> PersistedAppState {
        PersistedAppState {
            book_id: self.book_id,
            chapter: self.chapter,
            screen: self.page,
            shell_orientation: DisplayOrientation::PortraitButtonsLeft as u8,
            reading_orientation: self.orientation as u8,
            refresh_policy: self.refresh_policy as u8,
            font_size: self.font_size as u8,
            line_spacing: self.line_spacing as u8,
            font_weight: self.font_weight as u8,
            font_family: self.font_family as u8,
            front_buttons: self.front_buttons as u8,
            source_hash: 0,
            source_size: 0,
        }
    }

    pub fn type_settings(self) -> TypeSettings {
        TypeSettings {
            size: self.font_size,
            spacing: self.line_spacing,
            weight: self.font_weight,
            family: self.font_family,
        }
    }

    /// Where the reader sits before an open, so an aborted transaction can put
    /// it back. Taken before the state that requests the open, never after.
    pub fn open_rollback(self) -> BookOpenRollback {
        BookOpenRollback {
            book_id: self.book_id,
            chapter: self.chapter,
            page: self.page,
            view: self.view,
            selection: self.selection,
            sd_page_count: self.sd_page_count,
            sd_chapter_count: self.sd_chapter_count,
            sd_chapter_pages: self.sd_chapter_pages,
        }
    }

    /// Puts the reader back on the book it was reading when a book-open
    /// transaction aborted.
    ///
    /// Position and navigation bounds together. Nothing reloads the old book
    /// after a switch that never happened, so this is the only chance to undo
    /// what selecting the new one overwrote; see [`BookOpenRollback`].
    pub fn restore_after_failed_open(mut self, rollback: BookOpenRollback) -> Self {
        self.book_id = rollback.book_id;
        self.chapter = rollback.chapter;
        self.page = rollback.page;
        self.view = rollback.view;
        self.selection = rollback.selection;
        self.sd_page_count = rollback.sd_page_count;
        self.sd_chapter_count = rollback.sd_chapter_count;
        self.sd_chapter_pages = rollback.sd_chapter_pages;
        self.dirty = Rect::FULL;
        self
    }

    /// Settles a move through the folder tree whose command never left the
    /// app, as the standstill it is.
    ///
    /// A non-idle `library_browse` holds the list still while it waits, and
    /// the wait is a promise that some command is on its way to end it. A
    /// command the queue refused breaks that promise, and only the caller
    /// that saw the refusal knows. Nothing moved, so nothing changes but the
    /// wait ending and the rail coming back.
    pub fn library_browse_rejected(mut self) -> Self {
        if !self.library_browse.is_idle() {
            self.library_browse = LibraryBrowse::Idle;
            self.dirty = Rect::FULL;
        }
        self
    }

    /// Settles a picked Library action whose storage command never left the
    /// app, as the failure it is.
    ///
    /// `Busy` waits on an event that only the storage task can send, so a
    /// command the queue refused would leave the screen saying "clearing…"
    /// with nothing on its way to answer — until the battery ran out. The
    /// caller that saw the refusal is the only one who knows, so it says so
    /// here.
    pub fn library_action_rejected(mut self) -> Self {
        if let LibraryMenu::Busy { action, .. } = self.library_menu {
            self.library_menu = LibraryMenu::Done { action, ok: false };
            self.dirty = Rect::FULL;
        }
        self
    }

    pub fn library_item_count(self, ctx: ReducerContext) -> u16 {
        self.library_count.max(ctx.builtin_book_count as u16).max(1)
    }

    pub fn chapter_item_count(self, ctx: ReducerContext) -> u16 {
        if ReaderSource::from_book_id(self.book_id).is_sd() {
            self.sd_chapter_count.max(1)
        } else {
            u16::from(ctx.builtin_chapter_count.max(1))
        }
    }

    pub fn sd_chapter_for_page(self, page: u32) -> u16 {
        let mut selected = 0u16;
        for index in 0..self.sd_chapter_count.min(MAX_SD_CHAPTERS as u16) {
            if u32::from(self.sd_chapter_pages[index as usize]) <= page {
                selected = index;
            } else {
                break;
            }
        }
        selected
    }
}

/// Whether an orientation stands the panel's long axis upright. The two
/// portrait variants share one page geometry, so reading layout keys off
/// this rather than the exact variant.
pub fn is_portrait(orientation: DisplayOrientation) -> bool {
    matches!(
        orientation,
        DisplayOrientation::PortraitButtonsLeft | DisplayOrientation::PortraitButtonsRight
    )
}

pub fn display_orientation_from_u8(value: u8) -> Option<DisplayOrientation> {
    match value {
        0 => Some(DisplayOrientation::LandscapeButtonsBottom),
        1 => Some(DisplayOrientation::LandscapeButtonsTop),
        2 => Some(DisplayOrientation::PortraitButtonsLeft),
        3 => Some(DisplayOrientation::PortraitButtonsRight),
        _ => None,
    }
}

pub fn front_buttons_from_u8(value: u8) -> Option<FrontButtons> {
    match value {
        0 => Some(FrontButtons::PagesRight),
        1 => Some(FrontButtons::PagesLeft),
        _ => None,
    }
}

pub fn refresh_policy_from_u8(value: u8) -> Option<RefreshPolicy> {
    match value {
        0 => Some(RefreshPolicy::FastOnly),
        1 => Some(RefreshPolicy::FullOnWake),
        2 => Some(RefreshPolicy::FullEveryTen),
        _ => None,
    }
}

fn wrap_next(value: u16, len: u16) -> u16 {
    if value + 1 >= len {
        0
    } else {
        value + 1
    }
}

fn wrap_prev(value: u16, len: u16) -> u16 {
    if value == 0 {
        len - 1
    } else {
        value - 1
    }
}

fn home_action_for_button(button: Button) -> HomeAction {
    match button {
        // Home direct-maps the left-edge key column (top to bottom:
        // Back, Confirm, Previous, Next). Back zooms out of the book
        // onto the shelf; Confirm affirms continuing to read.
        Button::Back => HomeAction::Files,
        Button::Confirm => HomeAction::Read,
        Button::Previous | Button::PagePrevious => HomeAction::Wireless,
        Button::Next | Button::PageNext | Button::Power => HomeAction::Settings,
    }
}

fn apply_home_action(mut state: ReaderState, action: HomeAction) -> ReaderState {
    state.selection = 0;
    state.read_request_pending = false;
    match action {
        HomeAction::Read => {
            if ReaderSource::from_book_id(state.book_id).is_sd() {
                state.view = AppView::Reading;
                state.selection = state.chapter;
            } else if state.library_count > 0 {
                state.view = AppView::Library;
            } else {
                state.view = AppView::Reading;
                state.book_id = 1;
            }
        }
        HomeAction::Files => {
            state.view = AppView::Library;
        }
        HomeAction::Wireless => {
            state.view = AppView::Wireless;
            state.sync_status = state.wireless_entry_status();
        }
        HomeAction::Settings => {
            state.view = AppView::Settings;
        }
    }
    state
}

/// Positional button semantics: after the device rotates, the button
/// sitting where Back used to be still acts as Back. The 180-degree flip
/// reverses both the front column and the side pair. The quarter turn to
/// portrait keeps everything: the front column reads left-to-right along
/// the bottom bezel in its natural order, and the side pair stands on end
/// with the forward key already at its natural end (hardware-walked
/// July 9 2026 — a swap here came out inverted on the device).
/// `PagesLeft` exchanges the two front pairs whole (back/confirm with
/// previous/next) before the orientation map, so the swap is a fact about
/// the physical buttons rather than the current hold. The side page rail
/// is untouched.
fn swap_front_pairs(front_buttons: FrontButtons, button: Option<Button>) -> Option<Button> {
    if front_buttons == FrontButtons::PagesRight {
        return button;
    }
    Some(match button? {
        Button::Back => Button::Previous,
        Button::Confirm => Button::Next,
        Button::Previous => Button::Back,
        Button::Next => Button::Confirm,
        other => other,
    })
}

/// The physical key that reaches `action` under these settings.
///
/// `apply_input` runs a raw key through the front-pair swap and then the
/// orientation before the reducer sees it, so the key a hand presses and the
/// action it performs are only the same thing on the default settings.
/// `PagesLeft` turns a raw `Next` into `Confirm`, and `LandscapeButtonsTop`
/// turns it into `Back`. Anything that synthesises input rather than reading
/// the ADC has to send the key that produces the action it means, or it does
/// whatever the reader last saved in Settings.
///
/// Brute force over the seven keys rather than a hand-written inverse: both
/// maps are small, `orient_button`'s is not an involution, and a second table
/// would drift from the ones it inverts without anything failing loudly.
///
/// `view` matters because Home skips the front-pair swap. See [`arrives_as`].
///
/// `None` means no key reaches that action here, which no current mapping
/// produces (both are permutations) but is the honest answer if one stops
/// being one.
pub fn physical_key_for(
    view: AppView,
    orientation: DisplayOrientation,
    front_buttons: FrontButtons,
    action: Button,
) -> Option<Button> {
    const KEYS: [Button; 7] = [
        Button::Power,
        Button::Back,
        Button::Confirm,
        Button::Previous,
        Button::Next,
        Button::PagePrevious,
        Button::PageNext,
    ];
    KEYS.into_iter()
        .find(|&raw| arrives_as(view, orientation, front_buttons, raw) == Some(action))
}

/// What a raw key arrives as in `view`, mirroring `apply_input`'s two maps.
///
/// The view is part of the question because Home is positional rather than
/// grammatical: it direct-maps the physical key column and deliberately
/// skips the front-pair swap, so the same raw key means one thing at Home
/// and another everywhere else. Inverting both maps regardless put the
/// selftest on the wrong Home row under `PagesLeft`, asking for Confirm
/// (continue reading) and landing on Settings.
fn arrives_as(
    view: AppView,
    orientation: DisplayOrientation,
    front_buttons: FrontButtons,
    raw: Button,
) -> Option<Button> {
    if view == AppView::Home {
        orient_button(orientation, Some(raw))
    } else {
        orient_button(orientation, swap_front_pairs(front_buttons, Some(raw)))
    }
}

fn orient_button(orientation: DisplayOrientation, button: Option<Button>) -> Option<Button> {
    let button = button?;
    Some(match orientation {
        DisplayOrientation::LandscapeButtonsTop => match button {
            Button::Power => Button::Power,
            Button::Back => Button::Next,
            Button::Confirm => Button::Previous,
            Button::Previous => Button::Confirm,
            Button::Next => Button::Back,
            Button::PagePrevious => Button::PageNext,
            Button::PageNext => Button::PagePrevious,
        },
        _ => button,
    })
}

/// Settings rows, top to bottom: the type block first (typeface, then its
/// size, weight, and spacing — broadest choice to finest adjustment), then
/// the set-and-forget display rows.
fn apply_setting(mut state: ReaderState) -> ReaderState {
    match state.selection {
        0 => {
            state.font_family = next_font_family(state.font_family, state.custom_font_available);
        }
        1 => {
            state.font_size = match state.font_size {
                FontSize::Small => FontSize::Medium,
                FontSize::Medium => FontSize::Large,
                FontSize::Large => FontSize::Small,
            };
        }
        2 => {
            state.font_weight = match state.font_weight {
                FontWeight::Normal => FontWeight::Heavy,
                FontWeight::Heavy => FontWeight::Normal,
            };
        }
        3 => {
            state.line_spacing = match state.line_spacing {
                LineSpacing::Compact => LineSpacing::Normal,
                LineSpacing::Normal => LineSpacing::Relaxed,
                LineSpacing::Relaxed => LineSpacing::Compact,
            };
        }
        4 => {
            state.refresh_policy = match state.refresh_policy {
                RefreshPolicy::FastOnly => RefreshPolicy::FullOnWake,
                RefreshPolicy::FullOnWake => RefreshPolicy::FullEveryTen,
                RefreshPolicy::FullEveryTen => RefreshPolicy::FastOnly,
            };
        }
        5 => {
            // Three holds are offered: the two landscapes and the one
            // portrait (front buttons below the screen). The buttons-above
            // portrait variant stays in the enum for the persistence format
            // but has no use case, so the cycle skips it.
            state.orientation = match state.orientation {
                DisplayOrientation::LandscapeButtonsBottom => {
                    DisplayOrientation::LandscapeButtonsTop
                }
                DisplayOrientation::LandscapeButtonsTop => DisplayOrientation::PortraitButtonsLeft,
                _ => DisplayOrientation::LandscapeButtonsBottom,
            };
        }
        6 => {
            state.front_buttons = match state.front_buttons {
                FrontButtons::PagesRight => FrontButtons::PagesLeft,
                FrontButtons::PagesLeft => FrontButtons::PagesRight,
            };
        }
        _ => {}
    }
    state
}

fn next_font_family(family: FontFamily, custom_available: bool) -> FontFamily {
    match (family, custom_available) {
        (FontFamily::Literata, _) => FontFamily::Merriweather,
        (FontFamily::Merriweather, true) => FontFamily::Custom,
        (FontFamily::Merriweather, false) => FontFamily::Literata,
        (FontFamily::Custom, _) => FontFamily::Literata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTX: ReducerContext = ReducerContext::new(1, 3);

    #[test]
    fn emulator_demo_psk_stays_within_the_mintable_alphabet() {
        let bytes = PortalPsk::EMULATOR_DEMO.bytes();
        assert_eq!(bytes.len(), PortalPsk::LEN);
        for b in bytes {
            assert!(
                PSK_ALPHABET.contains(&b),
                "EMULATOR_DEMO byte {:?} is outside PSK_ALPHABET",
                b as char
            );
        }
    }

    #[test]
    fn portal_psk_construction_refuses_bytes_outside_the_alphabet() {
        let valid = PortalPsk::EMULATOR_DEMO.bytes();
        assert_eq!(PortalPsk::new(valid), Some(PortalPsk::EMULATOR_DEMO));
        for bad in [b'0', b';', 0xFF] {
            let mut bytes = valid;
            bytes[0] = bad;
            assert_eq!(
                PortalPsk::new(bytes),
                None,
                "byte {bad:#04x} must be refused"
            );
        }
    }

    fn press(state: ReaderState, button: Button) -> ReaderState {
        state.apply_input(CTX, InputEvent::button(button))
    }

    /// A reader parked in a book, as if it had been opened and read into.
    fn reading(book_index: u16, chapter: u16, page: u32) -> ReaderState {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = ReaderSource::sd(book_index).book_id();
        state.chapter = chapter;
        state.page = page;
        state.library_count = 4;
        state.sd_page_count = 500;
        state.sd_chapter_count = 20;
        state
    }

    fn loaded(book_id: u32, position: Option<u32>) -> LibraryEvent {
        LibraryEvent::Loaded {
            book_id,
            pages: 500,
            chapters: 20,
            current_chapter: 3,
            chapter_pages: [0; MAX_SD_CHAPTERS],
            position,
            text_replaced: true,
        }
    }

    /// What a page turn served out of the resident section window answers with:
    /// no new text, no landing position, and whatever shape the store has
    /// reached by now — which a background index walk may have grown since the
    /// app was last told anything.
    fn extend_answered_from_ram(book_id: u32, pages: u32, current_chapter: u16) -> LibraryEvent {
        LibraryEvent::Loaded {
            book_id,
            pages,
            chapters: 20,
            current_chapter,
            chapter_pages: [0; MAX_SD_CHAPTERS],
            position: None,
            text_replaced: false,
        }
    }

    /// A storage channel that is already full, so every send fails and the
    /// parking slots are the only thing standing between a command and the bin.
    fn full_channel(_: StorageCommand) -> bool {
        false
    }

    fn progress_for(book_index: u16, page: u32) -> StorageCommand {
        StorageCommand::StoreProgress(reading(book_index, 0, page).persisted())
    }

    fn open_closing(previous: &ReaderState, next: &ReaderState) -> StorageCommand {
        storage_command_for_transition(previous, next, 1).expect("a book change owes an open")
    }

    /// The app owes nothing: no open outstanding, nothing parked, and the
    /// frame that resolves the last open has settled.
    const NOTHING_OWED: SleepBlockers = SleepBlockers {
        open_unresolved: false,
        parked_storage: false,
        awaiting_open_frame: false,
    };

    /// An open is parked or inflight.
    const OPEN_UNRESOLVED: SleepBlockers = SleepBlockers {
        open_unresolved: true,
        parked_storage: false,
        awaiting_open_frame: true,
    };

    /// The open answered, but the frame it produced has not reached the panel.
    const AWAITING_FRAME: SleepBlockers = SleepBlockers {
        open_unresolved: false,
        parked_storage: false,
        awaiting_open_frame: true,
    };

    #[test]
    fn power_sleeps_immediately_when_nothing_is_owed() {
        let mut gate = SleepGate::new();
        assert!(gate.press(NOTHING_OWED));
        assert!(!gate.is_deferred());
        // Nothing was held back, so a later resting point sends nothing.
        assert!(!gate.release(NOTHING_OWED));
    }

    #[test]
    fn a_deferred_press_waits_for_the_frame_that_resolves_the_open() {
        // Power pressed while opening Book B, which then loads at its restored
        // page. The library event clears the open, but the frame showing that
        // page has not been drawn: the sleep screen is taken from whatever last
        // reached the panel, so sleeping here would leave Book B's provisional
        // page frozen on it for as long as the reader is away.
        let mut gate = SleepGate::new();
        assert!(!gate.press(OPEN_UNRESOLVED));

        // Loaded arrives. Not a resting point — the render is still to come.
        assert!(
            !gate.release(AWAITING_FRAME),
            "the library event alone must not release the press"
        );
        assert!(gate.is_deferred());

        // The render settles and the suppression lifts.
        assert!(gate.release(NOTHING_OWED));
        assert!(!gate.is_deferred());
    }

    #[test]
    fn a_deferred_press_waits_for_the_rollback_frame_too() {
        // Same, for the open that fails: BookOpenFailed puts the reader back on
        // Book A, and sleeping before that frame lands would strand the panel
        // showing Book B — a book the reader is not in and did not open.
        let mut gate = SleepGate::new();
        assert!(!gate.press(OPEN_UNRESOLVED));
        assert!(
            !gate.release(AWAITING_FRAME),
            "the rollback event alone must not release the press"
        );
        assert!(gate.release(NOTHING_OWED));
    }

    #[test]
    fn power_during_a_parked_open_waits_for_the_drain_and_the_frame() {
        // The open has not reached the storage task at all: it sits in the
        // app's parking slots until a render settles.
        let mut gate = SleepGate::new();
        let parked = SleepBlockers {
            open_unresolved: false,
            parked_storage: true,
            awaiting_open_frame: true,
        };
        assert!(!gate.press(parked));
        assert!(!gate.release(parked));

        // The drain sends it; now it is inflight, still not a resting point.
        assert!(!gate.release(OPEN_UNRESOLVED));
        assert!(gate.release(NOTHING_OWED));
    }

    #[test]
    fn a_deferred_sleep_is_sent_once() {
        // release() runs at every display cycle, so it has to be idempotent or
        // the power task would field a burst of requests.
        let mut gate = SleepGate::new();
        assert!(!gate.press(OPEN_UNRESOLVED));
        assert!(gate.release(NOTHING_OWED));
        assert!(!gate.release(NOTHING_OWED));
        assert!(!gate.release(OPEN_UNRESOLVED));
    }

    /// The ordering that strands the reader if the gate waits on a render
    /// cycle. The open is dispatched with its optimistic render; the display
    /// task takes render commands ahead of storage work, so that render settles
    /// while the open is still queued -- too early to lift anything. The open is
    /// then answered out of the resident window at the page the reader was
    /// already on, which repaints nothing and ends no cycle. If that event does
    /// not lift the gate, nothing ever will.
    #[test]
    fn an_open_answered_between_cycles_still_lifts_the_gate() {
        // The optimistic render settles first: the open has not answered, so
        // there is nothing to lift yet.
        assert!(!open_gate_may_lift(true, true, false));

        // Its `Loaded` arrives with no repaint owed and no frame in flight.
        assert!(open_gate_may_lift(true, false, false));
    }

    #[test]
    fn a_frame_in_flight_keeps_the_gate_shut() {
        // The open answered while its render was still on the panel's way. That
        // frame's own acknowledgement lifts the gate; lifting it here would let
        // a press -- or a deferred sleep -- overtake the frame it is waiting on.
        assert!(!open_gate_may_lift(true, false, true));
    }

    #[test]
    fn an_ungated_app_lifts_nothing() {
        // Every library event reaches this test, and all but an open's are
        // arriving with the gate already open.
        assert!(!open_gate_may_lift(false, false, false));
    }

    /// The regression this was written for. A book is opened while its index is
    /// still being built: the app is told the shape reached so far, the walk
    /// keeps growing the store, and it stays silent about that growth because
    /// by its own test the reader is nowhere near *its* frontier. Every page
    /// turn in between is answered out of the resident window.
    ///
    /// The reader walks to the last page the app knows about. If those answers
    /// were withheld to save the refresh, this is where the device looks
    /// broken: Next clamps to a no-op, so no state changes, no command goes
    /// out, and nothing repaints -- until the whole build finishes.
    #[test]
    fn a_book_growing_in_the_background_never_traps_the_reader() {
        let mut state = reading(0, 0, 0);
        let book_id = state.book_id;
        // What the first publish told the app: one section's worth of pages.
        state = state.apply_library_event(
            CTX,
            LibraryEvent::Loaded {
                book_id,
                pages: 20,
                chapters: 1,
                current_chapter: 0,
                chapter_pages: [0; MAX_SD_CHAPTERS],
                position: None,
                text_replaced: true,
            },
        );
        assert_eq!(state.sd_page_count, 20);

        // Read to the last page the app knows about. Every turn is a RAM hit,
        // and the walk has been growing the book the whole way.
        let mut grown = 20;
        while state.page < 19 {
            let before = state.page;
            state = press(state, Button::Next);
            assert_eq!(state.page, before + 1, "the turn moves inside the window");
            grown += 40;
            state = state
                .apply_library_event(CTX, extend_answered_from_ram(book_id, grown, state.chapter));
        }

        assert_eq!(state.sd_page_count, grown, "the bound tracks the store");

        let next = press(state, Button::Next);
        assert_eq!(next.page, 20, "Next still moves at the old frontier");
    }

    /// The saving, at the same time: those answers cost no panel refresh. The
    /// counter on screen is chapter-relative and drawn from the store, so a
    /// grown page count moves nothing the reader can see.
    #[test]
    fn a_page_turn_answered_from_ram_does_not_repaint_twice() {
        let state = reading(0, 3, 120);
        let folded =
            state.apply_library_event(CTX, extend_answered_from_ram(state.book_id, 900, 3));

        assert_ne!(folded.sd_page_count, state.sd_page_count, "the bound moved");
        assert!(
            !loaded_repaints(
                state.view == AppView::Reading,
                false,
                folded.page != state.page,
            ),
            "nothing the reader can see changed"
        );
    }

    /// A section really read from the card replaced the text under the reader,
    /// and can leave every announced field identical while doing it -- a chapter
    /// spanning two cache sections reports the same chapter from both halves.
    #[test]
    fn a_section_read_from_the_card_always_repaints() {
        assert!(loaded_repaints(true, true, false));
    }

    /// A landing position the storage task resolved moves the reader; that is
    /// on screen.
    #[test]
    fn a_load_that_moves_the_page_repaints() {
        assert!(loaded_repaints(true, false, true));
    }

    /// A chapter-only correction is invisible in Reading — the compositor
    /// derives its chapter-relative counter from the store, not from
    /// `ReaderState::chapter` — so it owes no refresh. The corrected chapter
    /// is still folded for persistence, Home, sleep, and the chapter list.
    #[test]
    fn a_chapter_only_correction_does_not_repaint() {
        assert!(!loaded_repaints(true, false, false));
    }

    /// Outside Reading the saving is not claimed: the other views draw enough of
    /// the store that reasoning about each costs more than the refresh saves.
    #[test]
    fn a_load_outside_reading_always_repaints() {
        assert!(loaded_repaints(false, false, false));
    }

    /// The sequence this exists for: a page turn is reduced and rendered, the
    /// flush fails, and the extend behind it is answered from RAM -- which since
    /// `announce_is_owed` sends no `Loaded`, so no event repaints the frame. The
    /// repaint has to come from the failure itself, with no second press.
    #[test]
    fn a_failed_page_turn_repaints_without_another_press() {
        let mut retry = RepaintRetry::new();
        // The turn before it settled normally, which is the ordinary state a
        // reader arrives in.
        retry.settled();

        assert!(
            retry.failed(),
            "the reader is looking at the page before this one; nothing else redraws it"
        );
    }

    #[test]
    fn a_failed_repaint_is_not_retried_again() {
        // A panel that fails the retry is not coming back this cycle. Repainting
        // into it on every failure would spend the display task and the battery
        // in a loop the reader cannot interrupt.
        let mut retry = RepaintRetry::new();
        assert!(retry.failed());
        assert!(!retry.failed());
        assert!(!retry.failed());
    }

    #[test]
    fn a_frame_that_reaches_the_panel_restores_the_retry() {
        // The budget is per display cycle, not per boot: a failure a hundred
        // page turns later is a fresh one and owes the reader the same repaint.
        let mut retry = RepaintRetry::new();
        assert!(retry.failed());
        assert!(!retry.failed());

        retry.settled();

        assert!(
            retry.failed(),
            "the next cycle's failure gets its own retry"
        );
    }

    #[test]
    fn every_blocker_holds_the_press_on_its_own() {
        for blocker in [
            SleepBlockers {
                open_unresolved: true,
                ..NOTHING_OWED
            },
            SleepBlockers {
                parked_storage: true,
                ..NOTHING_OWED
            },
            SleepBlockers {
                awaiting_open_frame: true,
                ..NOTHING_OWED
            },
        ] {
            let mut gate = SleepGate::new();
            assert!(!gate.press(blocker), "{blocker:?} must hold the press");
            assert!(!gate.release(blocker));
            assert!(gate.release(NOTHING_OWED));
        }
    }

    #[test]
    fn a_saturated_queue_still_takes_the_open() {
        // Both slots spoken for: a navigation command and a progress record
        // for the book being read, which is the state a busy storage task
        // leaves behind after a page turn against a full channel.
        let mut parked = ParkedStorage::new();
        let reader = reading(0, 4, 120);
        assert_eq!(
            parked.dispatch(extend_section_command(&reader, 0, 1), full_channel),
            StorageDispatch::Parked
        );
        assert_eq!(
            parked.dispatch(progress_for(0, 120), full_channel),
            StorageDispatch::Parked
        );
        assert_eq!(parked.len(), PARKED_STORAGE_SLOTS);

        // Selecting another book must not be the thing that gets dropped: it
        // carries the departing book's only close-out record, and the app arms
        // an input lock waiting on the event it produces.
        let open = open_closing(&reader, &reading(1, 0, 0));
        assert_eq!(parked.dispatch(open, full_channel), StorageDispatch::Parked);

        // The progress record gave up its slot, not the extend that came first
        // and not the open. Nothing is lost: the open carries book 0's page.
        let drained = [parked.pop_front(), parked.pop_front(), parked.pop_front()];
        assert!(
            matches!(drained[0], Some(StorageCommand::ExtendSection { .. })),
            "arrival order must hold, got {:?}",
            drained[0]
        );
        assert_eq!(drained[1], Some(open));
        assert_eq!(drained[2], None);
    }

    #[test]
    fn an_open_with_nothing_to_displace_is_refused_not_dropped() {
        // Two navigation commands, neither of them redundant. There is no safe
        // room to make, so the queue has to say so rather than quietly bin the
        // open and leave the app waiting for an event that cannot come.
        let mut parked = ParkedStorage::new();
        let reader = reading(0, 4, 120);
        for _ in 0..PARKED_STORAGE_SLOTS {
            assert_eq!(
                parked.dispatch(extend_section_command(&reader, 0, 1), full_channel),
                StorageDispatch::Parked
            );
        }

        let open = open_closing(&reader, &reading(1, 0, 0));
        assert_eq!(
            parked.dispatch(open, full_channel),
            StorageDispatch::Rejected
        );
        assert_eq!(parked.len(), PARKED_STORAGE_SLOTS);
    }

    #[test]
    fn a_refused_open_leaves_the_reader_where_it_was() {
        // What the app task does with a Rejected open: the state has already
        // moved to the new book, and putting it back is what keeps input alive
        // instead of locked behind an open that never happened.
        let before = reading(0, 4, 120);
        let committed = reading(1, 0, 0);
        let recovered = committed.restore_after_failed_open(before.open_rollback());

        assert_eq!(recovered.book_id, before.book_id);
        assert_eq!(recovered.page, 120);
        // Nothing left to persist, so no progress record names the book the
        // reader never reached.
        assert_eq!(recovered.persisted(), before.persisted());
    }

    #[test]
    fn a_progress_record_for_another_book_is_never_displaced() {
        // Only the departing book's own record is subsumed by the open. A
        // record for any other book is still the only copy of that page.
        let mut parked = ParkedStorage::new();
        let reader = reading(0, 4, 120);
        assert_eq!(
            parked.dispatch(progress_for(2, 77), full_channel),
            StorageDispatch::Parked
        );
        assert_eq!(
            parked.dispatch(extend_section_command(&reader, 0, 1), full_channel),
            StorageDispatch::Parked
        );

        let open = open_closing(&reader, &reading(1, 0, 0));
        assert_eq!(
            parked.dispatch(open, full_channel),
            StorageDispatch::Rejected
        );
        assert_eq!(parked.pop_front(), Some(progress_for(2, 77)));
    }

    #[test]
    fn an_empty_queue_sends_straight_through() {
        let mut parked = ParkedStorage::new();
        let reader = reading(0, 4, 120);
        assert_eq!(
            parked.dispatch(extend_section_command(&reader, 0, 1), |_| true),
            StorageDispatch::Sent
        );
        assert!(parked.is_empty());
    }

    #[test]
    fn nothing_overtakes_a_parked_command() {
        // The channel frees up while something is parked. The next command
        // still queues behind it: the storage task applies what it receives in
        // order, and a progress record that landed after a later open would
        // point the global state file back at the book just left.
        let mut parked = ParkedStorage::new();
        let reader = reading(0, 4, 120);
        let extend = extend_section_command(&reader, 0, 1);
        assert_eq!(
            parked.dispatch(extend, full_channel),
            StorageDispatch::Parked
        );
        assert_eq!(
            parked.dispatch(progress_for(0, 121), |_| true),
            StorageDispatch::Parked
        );
        assert_eq!(parked.pop_front(), Some(extend));
    }

    #[test]
    fn a_book_change_carries_the_departing_position_in_its_open() {
        let previous = reading(0, 4, 120);
        let next = reading(1, 0, 0);

        let command = storage_command_for_transition(&previous, &next, 7)
            .expect("a book change owes an open");

        match command {
            StorageCommand::OpenBook {
                book_id,
                index,
                previous: Some(departing),
                ..
            } => {
                assert_eq!(book_id, ReaderSource::sd(1).book_id());
                assert_eq!(index, 1);
                // The page the reader actually left, not the one it is going
                // to: writing the arriving book's state here is what used to
                // erase its saved position.
                assert_eq!(departing.book_id, ReaderSource::sd(0).book_id());
                assert_eq!(departing.chapter, 4);
                assert_eq!(departing.screen, 120);
            }
            other => panic!("expected an open closing out the old book, got {other:?}"),
        }
    }

    #[test]
    fn staying_in_one_book_owes_no_departing_position() {
        let previous = reading(0, 4, 120);
        let mut next = previous;
        next.page = 121;

        let command = storage_command_for_transition(&previous, &next, 7);
        assert!(
            matches!(command, Some(StorageCommand::ExtendSection { .. })),
            "a page turn extends the loaded section, got {command:?}"
        );

        // Re-entering a book that never changed opens without closing anything
        // out, so nothing else is written on the way in.
        let mut from_home = reading(0, 4, 120);
        from_home.view = AppView::Home;
        assert!(matches!(
            storage_command_for_transition(&from_home, &previous, 7),
            Some(StorageCommand::OpenBook { previous: None, .. })
        ));
    }

    #[test]
    fn a_resumed_open_lands_the_reader_in_one_event() {
        // The book was chosen from the shelf, so the app starts it at page 0
        // and the storage task resolves the real position from the card.
        let state = reading(1, 0, 0);
        let landed = state.apply_library_event(CTX, loaded(state.book_id, Some(184)));

        assert_eq!(landed.page, 184);
        assert_eq!(landed.chapter, 3);
        // One event carried both the book's shape and where to be in it, so
        // there is no second update for the app to render a stale page from.
        assert_eq!(landed.sd_page_count, 500);
    }

    #[test]
    fn a_load_without_a_position_leaves_the_reader_where_it_is() {
        // An extend answering an in-flight request must not drag the reader
        // back to the page that request was issued for.
        let state = reading(1, 3, 260);
        let landed = state.apply_library_event(CTX, loaded(state.book_id, None));

        assert_eq!(landed.page, 260);
    }

    #[test]
    fn a_load_position_is_clamped_to_the_book_it_describes() {
        let state = reading(1, 0, 0);
        let landed = state.apply_library_event(CTX, loaded(state.book_id, Some(9_000)));

        assert_eq!(landed.page, 499);
    }

    #[test]
    fn an_aborted_open_puts_the_reader_back_on_the_book_it_left() {
        let before = reading(0, 4, 120);
        let rollback = before.open_rollback();

        // The app has already moved to the new book by the time the storage
        // task refuses the switch.
        let committed = reading(1, 0, 0);
        let recovered = committed.restore_after_failed_open(rollback);

        assert_eq!(recovered.book_id, ReaderSource::sd(0).book_id());
        assert_eq!(recovered.chapter, 4);
        assert_eq!(recovered.page, 120);
        assert_eq!(recovered.view, AppView::Reading);
    }

    #[test]
    fn an_aborted_open_leaves_the_reader_able_to_turn_the_page() {
        // Selecting a book from the shelf resets the reader's idea of how long
        // the book is, because the new one has not been measured yet. Nothing
        // reloads the old book when the switch is refused, so a rollback that
        // restored only the page would leave the reader on page 120 of a book
        // the reducer thinks is one page long -- and the next page turn clamps
        // to sd_page_count - 1, putting them at the start and persisting it.
        let before = reading(0, 4, 120);
        let rollback = before.open_rollback();

        let mut committed = ReaderState::boot();
        committed.view = AppView::Library;
        committed.library_count = 4;
        committed.selection = 1;
        let committed = press(committed, Button::Confirm);
        assert_eq!(committed.sd_page_count, 1, "the shelf resets the bounds");

        let recovered = committed.restore_after_failed_open(rollback);
        assert_eq!(recovered.sd_page_count, before.sd_page_count);
        assert_eq!(recovered.sd_chapter_count, before.sd_chapter_count);

        let turned = press(recovered, Button::Next);
        assert_eq!(turned.page, 121, "the reader must keep reading forward");
    }

    #[test]
    fn the_open_transaction_never_leaves_a_book_half_switched() {
        // Losing the departing page means the switch does not happen at all,
        // so that page is still the reader's and still owed to the card.
        assert_eq!(
            book_open_outcome(false, false),
            BookOpenOutcome::KeptBookPositionUnwritten
        );
        assert!(!book_open_outcome(false, false).book_changed());

        // Open and readable, pointer not yet moved. The reader is in the new
        // book; only a reboot before the retry goes back to the old one.
        assert_eq!(
            book_open_outcome(true, false),
            BookOpenOutcome::OpenedPointerOwed
        );
        assert!(book_open_outcome(true, false).book_changed());

        assert_eq!(book_open_outcome(true, true), BookOpenOutcome::Opened);
        assert!(book_open_outcome(true, true).book_changed());
    }

    #[test]
    fn closing_out_a_book_does_not_widen_the_storage_command() {
        // `OpenBook` now carries a `PersistedAppState`, but the enum is sized
        // by `StoreWifiCredentials` (a 32-byte SSID and a 64-byte password),
        // which is still far wider. Folding the departing state into the open
        // therefore costs the four-deep command channel nothing at all — the
        // separate command it replaced was the same 100 bytes.
        assert_eq!(core::mem::size_of::<StorageCommand>(), 100);
    }

    #[test]
    fn stamping_the_render_request_costs_one_word() {
        // `requested_at_ms` is what lets the bench pair a press against the
        // frame that could reflect it. It is a `u64` to match every other
        // logged timestamp; measured, that is 108 -> 112 bytes with no
        // alignment padding, so the four-deep DISPLAY_COMMANDS channel pays
        // 16 bytes and the planner's stored request four. Recorded because
        // `.bss` trades one-for-one against the main stack region on this
        // target, so a struct in a channel is never free.
        //
        // 112 -> 120 when the portal SSID joined the PSK in
        // `SyncStatus::PortalUp`: three bytes of payload landing either side
        // of an alignment boundary, so the channel pays 32 and the planner 8.
        // Storing the three MAC bytes rather than the sixteen-character name
        // is what keeps it to that -- the finished string would have cost 16
        // and 64 (see `PortalSsid`).
        assert_eq!(core::mem::size_of::<RenderRequest>(), 120);
        assert!(
            core::mem::size_of::<PersistedAppState>() < core::mem::size_of::<WifiCredentials>(),
            "the departing state has outgrown the credentials variant",
        );
    }

    #[test]
    fn home_navigation_opens_primary_views() {
        assert_eq!(
            press(ReaderState::boot(), Button::Confirm).view,
            AppView::Reading
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Back).view,
            AppView::Library
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Previous).view,
            AppView::Wireless
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Next).view,
            AppView::Settings
        );
    }

    fn with_saved_network(state: ReaderState) -> ReaderState {
        state.apply_sync_event(SyncEvent::NetworkSaved(
            WifiSsid::new("latent.space").unwrap(),
        ))
    }

    #[test]
    fn wireless_without_saved_network_starts_the_portal_flow() {
        let state = press(ReaderState::boot(), Button::Previous);
        assert_eq!(state.view, AppView::Wireless);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        let state = press(state, Button::Confirm);
        assert_eq!(state.sync_status, SyncStatus::Starting);
        let state = state.apply_sync_event(SyncEvent::PortalUp(
            PortalPsk::EMULATOR_DEMO,
            PortalSsid::EMULATOR_DEMO,
        ));
        assert_eq!(
            state.sync_status,
            SyncStatus::PortalUp(PortalPsk::EMULATOR_DEMO, PortalSsid::EMULATOR_DEMO)
        );
        // Confirm is inert while the portal serves.
        let state = press(state, Button::Confirm);
        assert_eq!(
            state.sync_status,
            SyncStatus::PortalUp(PortalPsk::EMULATOR_DEMO, PortalSsid::EMULATOR_DEMO)
        );
        let state = state.apply_sync_event(SyncEvent::CredentialsSaved(
            WifiSsid::new("latent.space").unwrap(),
        ));
        assert_eq!(state.sync_status, SyncStatus::CredentialsSaved);
        // The portal's capture names the network for the rest of the boot.
        assert_eq!(state.wifi_ssid(), "latent.space");
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn sync_serving_state_follows_connect_and_back_exits() {
        let state = with_saved_network(ReaderState::boot());
        let state = press(press(state, Button::Previous), Button::Confirm)
            .apply_sync_event(SyncEvent::Connected([192, 168, 0, 233]))
            .apply_sync_event(SyncEvent::Serving([192, 168, 0, 233]));
        assert_eq!(state.sync_status, SyncStatus::Serving([192, 168, 0, 233]));
        // The screen labels Confirm "done" while serving, so it must exit
        // exactly like Back does (the wifi task defers the reset past any
        // in-flight transfer either way).
        let confirmed = press(state, Button::Confirm);
        assert_eq!(confirmed.view, AppView::Home);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn boot_network_probe_names_the_saved_network() {
        let state = with_saved_network(ReaderState::boot());
        assert_eq!(state.wifi_ssid(), "latent.space");
        let state = press(state, Button::Previous);
        assert_eq!(state.view, AppView::Wireless);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn boot_network_probe_upgrades_an_open_wireless_screen() {
        // The probe races screen entry only when the user opens Wireless
        // within the first seconds of boot; the screen upgrades in place.
        let state = press(ReaderState::boot(), Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        let state = with_saved_network(state);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn forget_needs_its_confirm_and_clears_the_network() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        let state = press(state, Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::ForgetPending);
        // Back cancels without leaving the screen or the network.
        let cancelled = press(state, Button::Back);
        assert_eq!(cancelled.view, AppView::Wireless);
        assert_eq!(cancelled.sync_status, SyncStatus::Idle);
        assert!(cancelled.wifi_network_saved());
        // Confirm forgets: the screen falls back to the set-up offer.
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Wireless);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        assert!(!state.wifi_network_saved());
    }

    /// A reader parked on a Library row, as if it had scanned real books.
    /// At the library root with `count` rows, all of them books: the shape a
    /// card with no folders on it has, and the one every test here predates
    /// folders by assuming.
    fn in_library(selection: u16, count: u16) -> ReaderState {
        let mut state = ReaderState::boot();
        state.view = AppView::Library;
        state.library_count = count;
        state.library_books = count;
        state.catalog_epoch = EPOCH;
        state.library_browse_epoch = EPOCH;
        state.selection = selection;
        state
    }

    /// The same, one folder down, with `books` of the rows being books and
    /// the rest folders below them.
    fn in_folder(selection: u16, books: u16, folders: u16) -> ReaderState {
        let mut state = in_library(selection, books + folders);
        state.library_books = books;
        state.library_depth = 1;
        state
    }

    const CLEAR: LibraryAction = LibraryAction::ClearCache;
    /// A catalog epoch that is deliberately not the boot default, so a
    /// command that forgot to carry one shows up as a mismatch.
    const EPOCH: u32 = 7;

    /// The id of the action `state` is waiting on.
    fn outstanding(state: ReaderState) -> u32 {
        match state.library_menu {
            LibraryMenu::Busy { request_id, .. } => request_id,
            other => panic!("no action in flight: {other:?}"),
        }
    }

    #[test]
    fn library_sheet_opens_picks_and_settles() {
        let state = press(in_library(1, 4), Button::PagePrevious);
        assert_eq!(state.library_menu, LibraryMenu::Sheet { row: 0 });
        assert_eq!(state.selection, 1, "opening must not move the cursor");

        // Confirm on the labeled row executes — the sheet was the
        // deliberate step; recoverable actions ask no second question.
        let previous = state;
        let state = press(state, Button::Confirm);
        assert_eq!(
            state.library_menu,
            LibraryMenu::Busy {
                action: CLEAR,
                index: 1,
                request_id: 1
            }
        );
        assert_eq!(
            state.view,
            AppView::Library,
            "confirm must not open the book"
        );
        assert_eq!(
            library_action_command_for_transition(&previous, &state),
            Some(StorageCommand::ClearBookCache {
                request_id: 1,
                index: 1,
                browse_epoch: EPOCH
            })
        );

        let state = state.apply_library_event(
            CTX,
            LibraryEvent::CacheCleared {
                request_id: 1,
                ok: true,
            },
        );
        assert_eq!(
            state.library_menu,
            LibraryMenu::Done {
                action: CLEAR,
                ok: true
            }
        );
        // The next press dismisses the note and still acts.
        let state = press(state, Button::Next);
        assert_eq!(state.library_menu, LibraryMenu::None);
        assert_eq!(state.selection, 2);
    }

    #[test]
    fn library_sheet_browse_moves_its_cursor_not_the_list() {
        let sheet = press(in_library(1, 4), Button::PagePrevious);
        for button in [Button::Next, Button::Previous, Button::PageNext] {
            let moved = press(sheet, button);
            assert!(
                matches!(moved.library_menu, LibraryMenu::Sheet { .. }),
                "{button:?} must stay on the sheet"
            );
            assert_eq!(moved.selection, 1, "{button:?} must not move the list");
        }
        // Back and the summoning key dismiss without acting.
        for button in [Button::Back, Button::PagePrevious] {
            let dismissed = press(sheet, button);
            assert_eq!(dismissed.library_menu, LibraryMenu::None);
            assert_eq!(
                dismissed.view,
                AppView::Library,
                "{button:?} must only dismiss"
            );
            assert_eq!(dismissed.selection, 1);
        }
    }

    #[test]
    fn library_sheet_dismissal_emits_nothing() {
        let sheet = press(in_library(1, 4), Button::PagePrevious);
        for button in [Button::Back, Button::PagePrevious] {
            let dismissed = press(sheet, button);
            assert_eq!(dismissed.library_menu, LibraryMenu::None);
            assert_eq!(
                library_action_command_for_transition(&sheet, &dismissed),
                None,
                "{button:?} must not owe storage anything"
            );
        }
        // Moving the sheet cursor owes nothing either.
        let moved = press(sheet, Button::Next);
        assert_eq!(library_action_command_for_transition(&sheet, &moved), None);
    }

    #[test]
    fn library_sheet_never_opens_off_the_catalog() {
        // The built-in fallback row has no per-book actions.
        let state = press(in_library(0, 0), Button::PagePrevious);
        assert_eq!(state.library_menu, LibraryMenu::None);
    }

    #[test]
    fn library_sheet_does_not_survive_scans_or_leaving() {
        let sheet = press(in_library(1, 4), Button::PagePrevious);
        let state = sheet.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 4,
                catalog_epoch: EPOCH + 1,
            },
        );
        assert_eq!(
            state.library_menu,
            LibraryMenu::None,
            "a rescan may reorder rows; the sheet must not carry over"
        );
        assert_eq!(
            state.catalog_epoch,
            EPOCH + 1,
            "the list now belongs to the new catalog"
        );

        // Leaving the screen while the clear is in flight drops the claim to
        // the answer: a late settle event shows no note.
        let busy = press(sheet, Button::Confirm);
        assert_eq!(
            busy.library_menu,
            LibraryMenu::Busy {
                action: CLEAR,
                index: 1,
                request_id: 1
            }
        );
        let away = press(busy, Button::Back);
        assert_ne!(away.view, AppView::Library);
        assert_eq!(away.library_menu, LibraryMenu::None);
        let settled = away.apply_library_event(
            CTX,
            LibraryEvent::CacheCleared {
                request_id: 1,
                ok: true,
            },
        );
        assert_eq!(settled.library_menu, LibraryMenu::None);
    }

    #[test]
    fn clear_cache_failure_shows_the_failed_note() {
        let busy = press(
            press(in_library(2, 4), Button::PagePrevious),
            Button::Confirm,
        );
        let state = busy.apply_library_event(
            CTX,
            LibraryEvent::CacheCleared {
                request_id: outstanding(busy),
                ok: false,
            },
        );
        assert_eq!(
            state.library_menu,
            LibraryMenu::Done {
                action: CLEAR,
                ok: false
            }
        );
    }

    /// A rescan dismisses the sheet but not the wait, and the asymmetry is
    /// deliberate: the sheet is a question that a reordered list invalidates,
    /// while the wait belongs to a command already handed over. Dropping it
    /// would leave the answer with nothing to settle. The answer is a refusal
    /// here — the storage task compares the command's epoch against the
    /// catalog it now holds and will not delete against a stale row — and the
    /// note has to report that, not silently vanish.
    #[test]
    fn a_rescan_keeps_the_wait_it_cannot_cancel() {
        let busy = press(
            press(in_library(1, 4), Button::PagePrevious),
            Button::Confirm,
        );
        let request_id = outstanding(busy);

        let rescanned = busy.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 4,
                catalog_epoch: EPOCH + 1,
            },
        );
        assert_eq!(
            rescanned.library_menu, busy.library_menu,
            "the command is already out; the wait must survive to receive it"
        );
        assert_eq!(rescanned.catalog_epoch, EPOCH + 1);

        // The storage task refused the now-stale row, and the note says so.
        let settled = rescanned.apply_library_event(
            CTX,
            LibraryEvent::CacheCleared {
                request_id,
                ok: false,
            },
        );
        assert_eq!(
            settled.library_menu,
            LibraryMenu::Done {
                action: CLEAR,
                ok: false
            }
        );
    }

    /// While the storage task is deleting a row's cache, the list under it is
    /// frozen: nothing may start a second action, open the book being worked
    /// on, or walk the cursor off the row the note is about.
    #[test]
    fn library_busy_freezes_the_list() {
        let busy = press(
            press(in_library(1, 4), Button::PagePrevious),
            Button::Confirm,
        );
        for button in [
            Button::Confirm,
            Button::Next,
            Button::Previous,
            Button::PageNext,
            Button::PagePrevious,
        ] {
            let pressed = press(busy, button);
            assert_eq!(
                pressed.library_menu, busy.library_menu,
                "{button:?} must not disturb the action in flight"
            );
            assert_eq!(pressed.selection, 1, "{button:?} must not move the list");
            assert_eq!(pressed.view, AppView::Library, "{button:?} must not leave");
            assert_eq!(
                library_action_command_for_transition(&busy, &pressed),
                None,
                "{button:?} must not start a second action"
            );
        }
        // Back is the way out, and leaving drops the wait.
        let away = press(busy, Button::Back);
        assert_eq!(away.view, AppView::Home);
        assert_eq!(away.library_menu, LibraryMenu::None);
    }

    /// Two clears can only ever be in flight across a visit to another screen —
    /// and the second may well be the *same row* the reader gave up on, so the
    /// row cannot tell the answers apart. Only the request id can.
    #[test]
    fn clear_cache_settles_only_its_own_request() {
        let first = press(
            press(in_library(3, 4), Button::PagePrevious),
            Button::Confirm,
        );
        // Walk away mid-clear, come back, and clear the very same row again.
        // Home's Back key is the way back onto the shelf, which lands the
        // cursor at the top; three Next presses return it to row 3.
        let returned = press(press(first, Button::Back), Button::Back);
        assert_eq!(returned.view, AppView::Library);
        let returned = press(
            press(press(returned, Button::Next), Button::Next),
            Button::Next,
        );
        assert_eq!(returned.selection, 3);
        let second = press(press(returned, Button::PagePrevious), Button::Confirm);
        assert_ne!(
            outstanding(second),
            outstanding(first),
            "a second pick must not reuse an outstanding id"
        );
        assert_eq!(
            second.library_menu,
            LibraryMenu::Busy {
                action: CLEAR,
                index: 3,
                request_id: outstanding(second)
            }
        );

        // The abandoned first clear lands now — a stale-epoch refusal, say.
        // It answers a command this wait never issued.
        let intruder = second.apply_library_event(
            CTX,
            LibraryEvent::CacheCleared {
                request_id: outstanding(first),
                ok: false,
            },
        );
        assert_eq!(
            intruder.library_menu, second.library_menu,
            "an abandoned clear's outcome is not this clear's answer"
        );

        let mine = intruder.apply_library_event(
            CTX,
            LibraryEvent::CacheCleared {
                request_id: outstanding(second),
                ok: true,
            },
        );
        assert_eq!(
            mine.library_menu,
            LibraryMenu::Done {
                action: CLEAR,
                ok: true
            }
        );
    }

    /// A command the storage queue refused produces no event, so the app has
    /// to settle the wait itself rather than sit on "clearing…" forever.
    #[test]
    fn rejected_library_action_settles_as_failed() {
        let busy = press(
            press(in_library(1, 4), Button::PagePrevious),
            Button::Confirm,
        );
        let settled = busy.library_action_rejected();
        assert_eq!(
            settled.library_menu,
            LibraryMenu::Done {
                action: CLEAR,
                ok: false
            }
        );
        // Nothing in flight, nothing to settle.
        let resting = in_library(1, 4);
        assert_eq!(resting.library_action_rejected(), resting);
    }

    /// The library-event channel drops events when it is full, and a dropped
    /// `CacheCleared` would leave the list frozen on "clearing…" for the rest
    /// of the visit — the work is done, so nothing sends it again. Every event
    /// that settles a wait the app is holding has to say so, or the firmware's
    /// sender routes it out the lossy path.
    /// Every event that releases a lock the app took when it handed work over.
    /// Listed once, so the routing test and the holder tests below cannot
    /// disagree about which events are which.
    fn settling_events() -> [LibraryEvent; 8] {
        [
            LibraryEvent::CacheCleared {
                request_id: 1,
                ok: true,
            },
            LibraryEvent::BookOpenFailed { book_id: 2 },
            LibraryEvent::Loaded {
                book_id: 2,
                pages: 1,
                chapters: 1,
                current_chapter: 0,
                chapter_pages: [0; MAX_SD_CHAPTERS],
                position: None,
                text_replaced: true,
            },
            LibraryEvent::Restored {
                book_id: 2,
                chapter: 0,
                page: 0,
                page_count: 0,
                reading_orientation: 0,
                refresh_policy: 0,
                font_size: 0,
                line_spacing: 0,
                font_weight: 0,
                font_family: 0,
                front_buttons: 0,
            },
            // The browse answers. Each settles a `LibraryBrowse` wait, and a
            // dropped one leaves the Library rail held on a press nothing
            // will ever answer.
            LibraryEvent::FolderListed {
                request_id: Some(1),
                browse_epoch: EPOCH,
                depth: 1,
                count: 3,
                books: 2,
                selection: 0,
            },
            LibraryEvent::RowIsBook {
                request_id: 1,
                index: 0,
                catalog_epoch: EPOCH,
            },
            LibraryEvent::RowFailed { request_id: 1 },
            LibraryEvent::LibraryUnreadable {
                browse_epoch: EPOCH,
            },
        ]
    }

    /// The rest: the next event or the next render makes a dropped one good.
    fn refresh_events() -> [LibraryEvent; 3] {
        [
            LibraryEvent::Scanned {
                count: 4,
                catalog_epoch: EPOCH,
            },
            LibraryEvent::CustomFont { available: true },
            LibraryEvent::ChapterPage {
                book_id: 2,
                chapter: 0,
                page: 0,
            },
        ]
    }

    #[test]
    fn events_that_settle_a_wait_are_not_droppable() {
        for event in settling_events() {
            assert!(
                event.must_be_delivered(),
                "{event:?} releases a lock the app is holding"
            );
        }
        for event in refresh_events() {
            assert!(!event.must_be_delivered(), "{event:?} is only a refresh");
        }
    }

    /// One of the task's channels, as far as a sender can see it: push to the
    /// back, take from the front, no looking inside. Only this plumbing is
    /// modelled — the walks driven over it below are the ones the firmware
    /// runs.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Ring<T: Copy + PartialEq, const N: usize> {
        slots: [Option<T>; N],
        head: usize,
        len: usize,
    }

    type LibraryRing = Ring<LibraryEvent, LIBRARY_EVENT_SLOTS>;
    type DisplayRing = Ring<DisplayEvent, DISPLAY_EVENT_SLOTS>;

    impl<T: Copy + PartialEq, const N: usize> Ring<T, N> {
        fn new() -> Self {
            Self {
                slots: [None; N],
                head: 0,
                len: 0,
            }
        }

        fn try_send(&mut self, event: T) -> bool {
            if self.len == N {
                return false;
            }
            self.slots[(self.head + self.len) % N] = Some(event);
            self.len += 1;
            true
        }

        fn try_receive(&mut self) -> Option<T> {
            let event = self.slots[self.head].take()?;
            self.head = (self.head + 1) % N;
            self.len -= 1;
            Some(event)
        }

        /// Everything queued, front to back — the order the app would see.
        fn drained(mut self) -> [Option<T>; N] {
            let mut out = [None; N];
            for slot in out.iter_mut() {
                *slot = self.try_receive();
            }
            out
        }

        fn holds(self, event: T) -> bool {
            self.drained().contains(&Some(event))
        }
    }

    impl LibraryRing {
        fn refreshes(self) -> usize {
            self.drained()
                .iter()
                .filter(|queued| queued.is_some_and(|event| !event.must_be_delivered()))
                .count()
        }
    }

    /// The firmware's `send_required_library_event`, over the modelled ring:
    /// try the channel, then the walk, then the holder — in that order, which
    /// is the order the task uses.
    fn send_required(
        queue: &mut LibraryRing,
        holder: &mut LibraryEventHolder,
        event: LibraryEvent,
    ) {
        if queue.try_send(event) {
            return;
        }
        let mut walk = EvictionWalk::new(LIBRARY_EVENT_SLOTS);
        while !walk.exhausted() {
            let Some(head) = queue.try_receive() else {
                break;
            };
            match walk.inspect(&head) {
                EvictionStep::Discard => {
                    queue.try_send(event);
                    return;
                }
                EvictionStep::Requeue => {
                    queue.try_send(head);
                }
            }
        }
        if holder.hold(&event) != HoldOutcome::Held {
            // The firmware's last try at a slot that may have freed since,
            // and then the drop it reports.
            queue.try_send(event);
        }
    }

    /// The main loop's placing branch, which runs once the app has drained a
    /// slot. Only ever called where the model has made room, matching the
    /// firmware's `LIBRARY_EVENTS.send().await`.
    fn place_held(queue: &mut LibraryRing, holder: &mut LibraryEventHolder) {
        let Some(event) = holder.pending() else {
            return;
        };
        assert!(queue.try_send(event), "the placing branch waits for room");
        assert_eq!(holder.placed(), Some(event));
    }

    /// The firmware's `send_display_event`, over the modelled channel: the
    /// queue, then the holder for an acknowledgement, and the lossy path for
    /// anything else. Returns whether the event was queued outright.
    fn send_display(
        queue: &mut DisplayRing,
        holder: &mut DisplayEventHolder,
        event: DisplayEvent,
    ) -> bool {
        if queue.try_send(event) {
            return true;
        }
        if holder.hold(&event) != DisplayHoldOutcome::Held {
            // The firmware's last try at a slot that may have freed since,
            // and then the drop it reports.
            queue.try_send(event);
        }
        false
    }

    /// The main loop's placing branch for a held acknowledgement.
    fn place_held_ack(queue: &mut DisplayRing, holder: &mut DisplayEventHolder) {
        let Some(event) = holder.pending() else {
            return;
        };
        assert!(queue.try_send(event), "the placing branch waits for room");
        assert_eq!(holder.placed(), Some(event));
    }

    fn display_filled(events: impl IntoIterator<Item = DisplayEvent>) -> DisplayRing {
        let mut ring = DisplayRing::new();
        for event in events {
            assert!(ring.try_send(event));
        }
        assert_eq!(ring.len, DISPLAY_EVENT_SLOTS, "the channel starts full");
        ring
    }

    /// The three questions the display task asks the holder, from three
    /// unrelated places: the main loop's storage branch, the pre-sleep drain,
    /// and the display-event sender.
    fn gates(holder: &LibraryEventHolder) -> [bool; 3] {
        [
            holder.storage_may_run(),
            holder.sleep_may_proceed(),
            holder.library_event_may_move(),
        ]
    }

    /// A plain acknowledgement with no chapter correction riding along.
    fn settled() -> DisplayEvent {
        DisplayEvent::Settled {
            chapter_cursor: None,
        }
    }

    fn cleared(request_id: u32) -> LibraryEvent {
        LibraryEvent::CacheCleared {
            request_id,
            ok: true,
        }
    }

    fn filled(events: impl IntoIterator<Item = LibraryEvent>) -> LibraryRing {
        let mut ring = LibraryRing::new();
        for event in events {
            assert!(ring.try_send(event));
        }
        assert_eq!(ring.len, LIBRARY_EVENT_SLOTS, "the channel starts full");
        ring
    }

    /// Making room for one settling event must never be paid for with
    /// another. An abandoned clear leaves its completion in flight while a
    /// second clear runs, so two of these really can be queued at once — and
    /// evicting the head unread would drop the first, stranding the visit it
    /// belongs to on "clearing…" with nothing left to settle it.
    #[test]
    fn making_room_never_spends_an_awaited_event() {
        // The completion is at the front, exactly where the old sender took
        // from, with refreshes behind it.
        let mut queue =
            filled(
                core::iter::once(cleared(1)).chain((0..7).map(|count| LibraryEvent::Scanned {
                    count,
                    catalog_epoch: EPOCH,
                })),
            );

        send_required(&mut queue, &mut LibraryEventHolder::new(), cleared(2));

        assert!(
            queue.holds(cleared(1)),
            "the queued completion must survive the newcomer"
        );
        assert!(queue.holds(cleared(2)), "the newcomer must land");
        assert_eq!(queue.len, LIBRARY_EVENT_SLOTS);
        assert_eq!(
            queue.refreshes(),
            6,
            "exactly one refresh paid for the slot"
        );
    }

    /// Every slot already awaited: nothing may be spent, so the walk places
    /// nothing and leaves the ring exactly as it found it. The newcomer is not
    /// lost either — it goes to the holder, and reaches the app once the
    /// consumer has drained a slot.
    #[test]
    fn a_channel_of_awaited_events_refuses_the_newcomer() {
        let before = filled((0..LIBRARY_EVENT_SLOTS as u32).map(cleared));
        let mut queue = before;
        let mut holder = LibraryEventHolder::new();

        send_required(&mut queue, &mut holder, cleared(99));

        assert!(
            !queue.holds(cleared(99)),
            "nothing could be spent, so the walk placed nothing"
        );
        assert_eq!(
            queue.drained(),
            before.drained(),
            "a refused walk disturbs nothing, order included"
        );
        assert_eq!(
            holder.pending(),
            Some(cleared(99)),
            "the newcomer is held, not dropped"
        );

        // The app takes one, which is what the placing branch waits for.
        queue.try_receive();
        place_held(&mut queue, &mut holder);
        assert!(queue.holds(cleared(99)), "the held event still arrives");
        assert_eq!(
            gates(&holder),
            [true; 3],
            "placing it lifts the standing orders"
        );
    }

    /// The correction used to travel as its own event down whichever channel
    /// had room. It could arrive after the acknowledgement it was supposed to
    /// precede -- the app selects the two channels independently -- or be
    /// dropped as a refresh while the acknowledgement it belonged to survived.
    /// Riding inside `Settled` makes both unrepresentable: one message cannot
    /// arrive in the wrong order or half-arrive, and the acknowledgement's own
    /// protection now covers the correction.
    #[test]
    fn the_chapter_correction_rides_with_the_acknowledgement() {
        let cursor = ChapterCursor {
            book_id: 2,
            page: 120,
            current_chapter: 900,
        };

        let (event, power) = display_refresh_outcome(true, Some(cursor));

        assert_eq!(
            event,
            DisplayEvent::Settled {
                chapter_cursor: Some(cursor)
            }
        );
        assert_eq!(power, PowerEvent::DisplaySettled);
        assert!(
            event.must_be_delivered(),
            "a carried correction is protected exactly as its acknowledgement is"
        );
        let mut holder = DisplayEventHolder::new();
        assert_eq!(holder.hold(&event), DisplayHoldOutcome::Held);
        assert_eq!(
            holder.pending(),
            Some(event),
            "it waits for room with its correction intact"
        );

        // A frame that never reached the panel corrects nothing: the cursor is
        // read off the page that was shown.
        assert_eq!(
            display_refresh_outcome(false, Some(cursor)).0,
            DisplayEvent::RefreshFailed
        );
    }

    /// Past `MAX_SD_CHAPTERS` the reducer's own page map saturates, which is
    /// what the correction is for. It names its book, because an
    /// acknowledgement can outlive the book it was rendered for.
    #[test]
    fn the_chapter_correction_applies_only_to_the_book_still_open() {
        let state = ReaderState::boot();
        let book_id = state.book_id;
        let page = state.page;

        let corrected = state.apply_chapter_cursor(ChapterCursor {
            book_id,
            page,
            current_chapter: 900,
        });

        assert_eq!(
            corrected.chapter, 900,
            "the uncapped chapter is adopted past the reducer's own map"
        );
        assert_eq!(
            corrected.dirty, state.dirty,
            "Reading shows page-within-chapter, so the correction owes no repaint"
        );

        let elsewhere = corrected.apply_chapter_cursor(ChapterCursor {
            book_id: book_id.wrapping_add(1),
            page,
            current_chapter: 5,
        });

        assert_eq!(
            elsewhere.chapter, 900,
            "a correction for another book is not this book's"
        );
    }

    /// A press is applied while its render is still in flight -- the reducer
    /// runs and only the repaint waits -- so the acknowledgement for the page
    /// left behind arrives after the reader has moved on. Adopting its chapter
    /// then would pair one page's number with another's chapter, and
    /// `extend_section_command` reads the two together.
    #[test]
    fn a_late_correction_does_not_land_on_the_page_moved_to() {
        let state = reading(0, 3, 120);
        let rendering_page = state.page;
        let chapter_of_that_page = state.chapter;

        // Page P is on the panel; the press for Q lands before its
        // acknowledgement does.
        let moved_on = press(state, Button::Next);
        assert_ne!(moved_on.page, rendering_page, "the reader turned the page");

        let settled = moved_on.apply_chapter_cursor(ChapterCursor {
            book_id: moved_on.book_id,
            page: rendering_page,
            current_chapter: chapter_of_that_page.wrapping_add(7),
        });

        assert_eq!(
            settled.chapter, moved_on.chapter,
            "the correction belongs to the page that was rendered, not this one"
        );

        // Q's own render answers for Q.
        let settled = settled.apply_chapter_cursor(ChapterCursor {
            book_id: settled.book_id,
            page: settled.page,
            current_chapter: 900,
        });

        assert_eq!(settled.chapter, 900, "the matching correction still lands");
    }

    /// The channel is never made room in: a full queue leaves the
    /// acknowledgement waiting rather than rearranging what is already
    /// queued, so every event in this list stays exactly where it was.
    #[test]
    fn a_full_queue_holds_the_acknowledgement_and_keeps_its_order() {
        let before = display_filled([
            settled(),
            DisplayEvent::Asleep,
            DisplayEvent::Library(cleared(3)),
            settled(),
            DisplayEvent::Library(cleared(7)),
            DisplayEvent::SleepFailed,
            settled(),
            DisplayEvent::Asleep,
        ]);
        let mut queue = before;
        let mut holder = DisplayEventHolder::new();

        let queued = send_display(&mut queue, &mut holder, DisplayEvent::RefreshFailed);

        assert!(!queued, "a full channel takes nothing");
        assert_eq!(
            queue.drained(),
            before.drained(),
            "the queue is left alone, order included"
        );
        assert_eq!(
            holder.pending(),
            Some(DisplayEvent::RefreshFailed),
            "the acknowledgement is held, not dropped"
        );

        // The app takes one, which is what the placing branch waits for.
        queue.try_receive();
        place_held_ack(&mut queue, &mut holder);
        assert!(queue.holds(DisplayEvent::RefreshFailed));
        assert_eq!(holder.pending(), None);
    }

    /// The two sleep notifications are informational -- the handshake the
    /// power task waits on travels beside them over its own channel, and the
    /// app only logs these. Protecting them the way an acknowledgement is
    /// protected would get the priority backwards: they would occupy the
    /// holder that the event ending the app's render cycle needs.
    #[test]
    fn only_a_render_acknowledgement_may_take_the_holder() {
        for event in [settled(), DisplayEvent::RefreshFailed] {
            let mut holder = DisplayEventHolder::new();
            assert!(event.must_be_delivered(), "{event:?} ends the render cycle");
            assert_eq!(holder.hold(&event), DisplayHoldOutcome::Held);
            assert_eq!(holder.pending(), Some(event));
        }
        for event in [DisplayEvent::Asleep, DisplayEvent::SleepFailed] {
            let mut holder = DisplayEventHolder::new();
            assert!(
                !event.must_be_delivered(),
                "{event:?} is only a notification"
            );
            assert_eq!(holder.hold(&event), DisplayHoldOutcome::NotRequired);
            assert_eq!(holder.pending(), None);
        }
    }

    /// A library event travelling on this channel keeps its own answer, so an
    /// event that settles a wait is protected whichever channel it is on.
    #[test]
    fn a_library_event_on_the_display_channel_keeps_its_own_rule() {
        for event in settling_events() {
            assert!(DisplayEvent::Library(event).must_be_delivered());
        }
        for event in refresh_events() {
            assert!(!DisplayEvent::Library(event).must_be_delivered());
        }
    }

    /// Two acknowledgements with the app draining neither. Both end the render
    /// cycle and the app clears its lock on either, so the one already waiting
    /// answers for the newcomer too -- but it must be the newcomer that gives
    /// way, not the event that has been waiting longer.
    #[test]
    fn an_occupied_acknowledgement_holder_keeps_the_older_one() {
        let mut holder = DisplayEventHolder::new();
        assert_eq!(holder.hold(&settled()), DisplayHoldOutcome::Held);

        assert_eq!(
            holder.hold(&DisplayEvent::RefreshFailed),
            DisplayHoldOutcome::Occupied
        );

        assert_eq!(holder.pending(), Some(settled()));
    }

    /// The gates are one rule read from three unrelated places, and the
    /// failure they prevent needs all three to agree. Two of them had already
    /// drifted apart when they were three separate `if`s in the task, so
    /// assert they move together.
    #[test]
    fn a_held_event_closes_every_gate_until_it_is_placed() {
        let mut holder = LibraryEventHolder::new();
        assert_eq!(
            gates(&holder),
            [true; 3],
            "an empty holder constrains nothing"
        );

        assert_eq!(holder.hold(&cleared(1)), HoldOutcome::Held);
        assert_eq!(
            gates(&holder),
            [false; 3],
            "storage, sleep and the display-event sender all stand down together"
        );

        assert_eq!(holder.placed(), Some(cleared(1)));
        assert_eq!(holder.pending(), None);
        assert_eq!(gates(&holder), [true; 3]);
    }

    /// The holder protects settling events; a refresh taking it would shut
    /// storage, sleep and the display-event sender down for an event nothing
    /// is waiting on — and the display-event sender standing down is exactly
    /// what strands the acknowledgement behind it. One caller really did route
    /// refreshes here, so the refusal belongs in the holder rather than in
    /// each sender.
    #[test]
    fn only_a_settling_event_may_take_the_holder() {
        for event in refresh_events() {
            let mut holder = LibraryEventHolder::new();
            assert_eq!(
                holder.hold(&event),
                HoldOutcome::NotSettling,
                "{event:?} is a refresh and belongs on the lossy path"
            );
            assert_eq!(holder.pending(), None);
            assert_eq!(gates(&holder), [true; 3], "{event:?} constrained the task");
        }
        for event in settling_events() {
            let mut holder = LibraryEventHolder::new();
            assert_eq!(holder.hold(&event), HoldOutcome::Held);
            assert_eq!(holder.pending(), Some(event));
        }
    }

    /// Two settling events out of one storage command is the only way to
    /// reach an occupied holder, every producer being gated on it. The one
    /// already waiting has waited longer and has the older wait behind it, so
    /// it is the one that survives.
    #[test]
    fn an_occupied_holder_keeps_the_older_wait() {
        let mut holder = LibraryEventHolder::new();
        assert_eq!(holder.hold(&cleared(1)), HoldOutcome::Held);

        assert_eq!(holder.hold(&cleared(2)), HoldOutcome::Occupied);

        assert_eq!(
            holder.pending(),
            Some(cleared(1)),
            "the newcomer must not displace the event already waiting"
        );
    }

    /// End to end, at the worst moment the firmware can reach: the channel is
    /// full of settling events and one more arrives. It must still be the
    /// case that nothing awaited is lost.
    #[test]
    fn a_settling_event_survives_a_channel_with_nothing_to_spend() {
        let mut queue = filled((0..LIBRARY_EVENT_SLOTS as u32).map(cleared));
        let mut holder = LibraryEventHolder::new();

        send_required(&mut queue, &mut holder, cleared(99));

        let mut delivered = [None; LIBRARY_EVENT_SLOTS + 1];
        let mut count = 0;
        // The app drains the channel, which is what the placing branch is
        // waiting for; then the branch runs and the held event goes in.
        drain_ids_into(&mut queue, &mut delivered, &mut count);
        place_held(&mut queue, &mut holder);
        drain_ids_into(&mut queue, &mut delivered, &mut count);

        let mut expected = [None; LIBRARY_EVENT_SLOTS + 1];
        for (slot, id) in expected.iter_mut().zip(0..LIBRARY_EVENT_SLOTS as u32) {
            *slot = Some(id);
        }
        expected[LIBRARY_EVENT_SLOTS] = Some(99);
        assert_eq!(
            delivered, expected,
            "every settling event reaches the app, oldest first"
        );
    }

    /// Empties `queue`, appending the request id of every `CacheCleared` it
    /// yields — the order the app would see them in.
    fn drain_ids_into(queue: &mut LibraryRing, out: &mut [Option<u32>], next: &mut usize) {
        while let Some(event) = queue.try_receive() {
            if let LibraryEvent::CacheCleared { request_id, .. } = event {
                out[*next] = Some(request_id);
                *next += 1;
            }
        }
    }

    #[test]
    fn forget_is_unreachable_without_a_saved_network() {
        let state = press(ReaderState::boot(), Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
        let state = press(state, Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::NotConfigured);
    }

    #[test]
    fn wifi_credentials_round_trip_strs() {
        let creds = WifiCredentials::from_strs("latent.space", "a&b c/9").unwrap();
        assert_eq!(creds.ssid(), "latent.space");
        assert_eq!(creds.password(), "a&b c/9");
        assert!(WifiCredentials::from_strs("", "x").is_none());
        assert!(WifiCredentials::from_strs("123456789012345678901234567890123", "x").is_none());
    }

    #[test]
    fn sync_with_saved_network_starts_on_confirm_and_tracks_events() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        assert_eq!(state.sync_status, SyncStatus::Idle);
        let state = press(state, Button::Confirm);
        assert_eq!(state.sync_status, SyncStatus::Starting);

        let state = state.apply_sync_event(SyncEvent::Connecting);
        assert_eq!(state.sync_status, SyncStatus::Connecting);
        // In-flight Confirm presses are ignored.
        let held = press(state, Button::Confirm);
        assert_eq!(held.sync_status, SyncStatus::Connecting);
        let state = state.apply_sync_event(SyncEvent::Connected([192, 168, 1, 23]));
        assert_eq!(state.sync_status, SyncStatus::Connected([192, 168, 1, 23]));
        let state = state.apply_sync_event(SyncEvent::Serving([192, 168, 1, 23]));

        // The done press returns Home with the entry status restored.
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Home);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn sync_error_can_be_retried_with_confirm() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        let state =
            press(state, Button::Confirm).apply_sync_event(SyncEvent::Failed(SyncError::Join));
        assert_eq!(state.sync_status, SyncStatus::Error(SyncError::Join));
        let state = press(state, Button::Confirm);
        assert_eq!(state.sync_status, SyncStatus::Starting);
    }

    #[test]
    fn sync_back_returns_home_and_resets_status() {
        let state = press(with_saved_network(ReaderState::boot()), Button::Previous);
        let state = press(state, Button::Confirm).apply_sync_event(SyncEvent::Connecting);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
        assert_eq!(state.sync_status, SyncStatus::Idle);
    }

    #[test]
    fn reader_source_maps_sd_catalog_indices_to_book_ids() {
        assert_eq!(
            ReaderSource::from_book_id(1),
            ReaderSource::BuiltIn { book_id: 1 }
        );
        assert_eq!(ReaderSource::sd(0).book_id(), 2);
        assert_eq!(ReaderSource::sd(7).book_id(), 9);
        assert_eq!(ReaderSource::from_book_id(9).sd_index(), Some(7));
    }

    #[test]
    fn library_open_key_opens_sd_book() {
        let state = in_library(0, 2);
        let previous = state;
        // A row is a book or a folder, and the app holds a count rather than
        // a listing, so the press asks the card instead of opening blind.
        let state = press(press(state, Button::Next), Button::Confirm);
        assert_eq!(state.selection, 1);
        assert_eq!(
            state.library_browse,
            LibraryBrowse::Choosing {
                index: 1,
                request_id: 1,
                browse_epoch: EPOCH
            }
        );
        assert_eq!(
            state.view,
            AppView::Library,
            "the screen holds until the card answers"
        );
        assert!(matches!(
            library_browse_command_for_transition(&previous, &state),
            Some(StorageCommand::ChooseLibraryRow {
                request_id: 1,
                index: 1,
                browse_epoch: EPOCH,
                ..
            })
        ));

        // A book, at that catalog row. The open is already running on the
        // storage side, which is why folding this dispatches nothing.
        let state = state.apply_library_event(
            CTX,
            LibraryEvent::RowIsBook {
                request_id: 1,
                index: 1,
                catalog_epoch: EPOCH,
            },
        );
        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.book_id, ReaderSource::sd(1).book_id());
        assert!(state.library_browse.is_idle());
    }

    /// Confirm on a folder row lists it instead of opening anything, and the
    /// cursor lands where the storage task put it.
    #[test]
    fn confirming_a_folder_row_enters_it() {
        // Two books above one folder, and the cursor on the folder.
        let state = in_library(2, 3);
        let mut state = state;
        state.library_books = 2;
        let entered = press(state, Button::Confirm);
        assert_eq!(
            entered.library_browse,
            LibraryBrowse::Choosing {
                index: 2,
                request_id: 1,
                browse_epoch: EPOCH
            }
        );

        let listed = entered.apply_library_event(
            CTX,
            LibraryEvent::FolderListed {
                request_id: Some(1),
                browse_epoch: EPOCH,
                depth: 1,
                count: 4,
                books: 3,
                selection: 0,
            },
        );
        assert_eq!(listed.view, AppView::Library, "entering opens no book");
        assert_eq!(listed.library_depth, 1);
        assert_eq!((listed.library_count, listed.library_books), (4, 3));
        assert_eq!(listed.selection, 0, "a folder is entered at its top");
        assert!(listed.library_browse.is_idle());
    }

    /// The whole sequence, because pinning only the classification let the
    /// arming be reverted without a test noticing.
    ///
    /// A reader on book B enters Library and picks B's own row. Nothing
    /// closes out, so the open carries no departing book, and it carries the
    /// catalog the row was resolved in. If that catalog is replaced before
    /// the open runs, storage refuses it. Without a rollback the reader stays
    /// in Reading over a row number that now names another book, and the next
    /// page turn extends by that index off a RAM window that checks the index
    /// rather than which book the text came from.
    #[test]
    fn a_refused_row_open_for_the_current_book_puts_the_reader_back() {
        let mut library = in_library(2, 3);
        library.book_id = ReaderSource::sd(2).book_id();
        library.library_browse = LibraryBrowse::Choosing {
            index: 2,
            request_id: 4,
            browse_epoch: EPOCH,
        };

        let reading = library.apply_library_event(
            CTX,
            LibraryEvent::RowIsBook {
                request_id: 4,
                index: 2,
                catalog_epoch: EPOCH,
            },
        );
        assert_eq!(reading.view, AppView::Reading);
        assert_eq!(
            reading.book_id, library.book_id,
            "the same book, by its row"
        );

        let command = storage_command_for_transition(&library, &reading, 1)
            .expect("entering Reading owes an open");
        assert!(
            matches!(
                command,
                StorageCommand::OpenBook {
                    previous: None,
                    catalog_epoch: Some(EPOCH),
                    ..
                }
            ),
            "closes out nobody, and names the catalog it was resolved in: {command:?}"
        );

        let hold = open_hold(&command, &library);
        assert_eq!(hold.opening_book, Some(reading.book_id));
        let rollback = hold
            .rollback
            .expect("an open that can be refused keeps a way back");

        // Storage refuses it, the catalog having moved on.
        let landed = reading.restore_after_failed_open(rollback);
        assert_eq!(
            landed.view,
            AppView::Library,
            "back where the row was picked"
        );
        assert_eq!(landed.book_id, library.book_id);
        assert_eq!(landed.chapter, library.chapter);
        assert_eq!(landed.page, library.page);
    }

    /// An open that can be refused needs somewhere to land when it is.
    ///
    /// The row open for a book already being read is the case this exists
    /// for: it closes out nobody, so the older reading of "can this abort"
    /// said no, while the catalog fence could refuse it all the same. What
    /// followed was worse than a bad screen. The reader stayed in Reading
    /// over a row number a rebuilt catalog had given to another book, and
    /// the next page turn extends by index without a fence, off a RAM window
    /// that checks the index and not which book the text came from.
    #[test]
    fn an_open_that_can_be_refused_says_so() {
        let state = in_library(0, 3);
        let plain = open_book_command(&state, 0, 1, None, None);
        assert!(
            !plain.open_may_refuse(),
            "nothing to close out and no catalog named"
        );
        assert!(
            open_book_command(&state, 0, 1, Some(state.persisted()), None).open_may_refuse(),
            "a switch leaves the reader between two books"
        );
        assert!(
            open_book_command(&state, 0, 1, None, Some(EPOCH)).open_may_refuse(),
            "a named catalog can be refused for having been replaced"
        );
        assert!(
            !StorageCommand::ExtendSection {
                request_id: 1,
                book_id: state.book_id,
                index: 0,
                chapter: 0,
                target_pages: 0,
                type_settings: state.type_settings(),
                portrait: is_portrait(state.orientation),
            }
            .open_may_refuse(),
            "only an open is an open"
        );
    }

    /// A row number means something only inside the catalog that produced
    /// it. Storage answers a press with a number, the app opens on it, and a
    /// scan landing between the two would leave that number naming another
    /// book. The open says which catalog it means so storage can refuse it.
    #[test]
    fn a_row_open_names_the_catalog_the_row_came_from() {
        let mut state = in_library(1, 3);
        state.library_browse = LibraryBrowse::Choosing {
            index: 1,
            request_id: 4,
            browse_epoch: EPOCH,
        };
        let opened = state.apply_library_event(
            CTX,
            LibraryEvent::RowIsBook {
                request_id: 4,
                index: 2,
                catalog_epoch: EPOCH,
            },
        );
        assert_eq!(opened.view, AppView::Reading);
        let command = storage_command_for_transition(&state, &opened, 1);
        assert!(
            matches!(
                command,
                Some(StorageCommand::OpenBook {
                    catalog_epoch: Some(EPOCH),
                    ..
                })
            ),
            "the row open carries its catalog, got {command:?}"
        );

        // A switch that did not come from a row resolves its own index, so
        // it has nothing to be stale against, and naming an epoch there
        // would refuse a good open after any rescan.
        let mut from_home = in_library(0, 3);
        from_home.view = AppView::Home;
        from_home.book_id = ReaderSource::sd(0).book_id();
        let mut reading = from_home;
        reading.view = AppView::Reading;
        reading.book_id = ReaderSource::sd(1).book_id();
        assert!(
            matches!(
                storage_command_for_transition(&from_home, &reading, 1),
                Some(StorageCommand::OpenBook {
                    catalog_epoch: None,
                    ..
                })
            ),
            "a non-row open stays unfenced"
        );
    }

    /// An answer names the press it answers. A row resolved against a
    /// catalog the app has since been told was replaced is refused, and the
    /// refusal has to end that press and no other: a reader who moved on to
    /// a folder is waiting on a newer command, and ending its wait leaves
    /// the response to arrive with nothing expecting it.
    #[test]
    fn a_stale_row_answer_does_not_end_a_newer_move() {
        let mut state = in_folder(1, 2, 1);
        state.library_browse = LibraryBrowse::Leaving {
            request_id: 9,
            browse_epoch: EPOCH,
        };
        let after = state.apply_library_event(
            CTX,
            LibraryEvent::RowIsBook {
                request_id: 4,
                index: 0,
                catalog_epoch: EPOCH + 1,
            },
        );
        assert_eq!(
            after.library_browse,
            LibraryBrowse::Leaving {
                request_id: 9,
                browse_epoch: EPOCH
            },
            "the move it did not answer is still being waited on"
        );
        assert_eq!(after.view, AppView::Library, "and nothing opened");
    }

    /// A picked action holds every other press, and Back is documented as
    /// the deliberate exception. Inside a folder Back was spending itself on
    /// the folder instead, so a reader waiting on an action that no answer
    /// was coming for had no press that left.
    #[test]
    fn back_leaves_a_held_library_from_inside_a_folder() {
        let mut state = in_folder(1, 2, 1);
        state.library_menu = LibraryMenu::Busy {
            action: CLEAR,
            index: 1,
            request_id: 3,
        };
        let out = press(state, Button::Back);
        assert_eq!(out.view, AppView::Home, "one press, and it leaves");
        assert_eq!(
            out.library_menu,
            LibraryMenu::None,
            "leaving Library drops the claim to the answer"
        );
        assert!(
            out.library_browse.is_idle(),
            "and asks the card for nothing on the way out"
        );
    }

    /// Back zooms out one level, which below the root is a folder rather
    /// than the whole screen.
    #[test]
    fn back_leaves_a_folder_before_it_leaves_library() {
        let state = in_folder(1, 2, 1);
        let previous = state;
        let leaving = press(state, Button::Back);
        assert_eq!(
            leaving.view,
            AppView::Library,
            "at depth, Back is a level, not the screen"
        );
        assert_eq!(
            leaving.library_browse,
            LibraryBrowse::Leaving {
                request_id: 1,
                browse_epoch: EPOCH
            }
        );
        assert_eq!(
            library_browse_command_for_transition(&previous, &leaving),
            Some(StorageCommand::LeaveLibraryFolder {
                request_id: 1,
                browse_epoch: EPOCH
            })
        );

        // Back at the root is still the way out of Library.
        let out = leaving.apply_library_event(
            CTX,
            LibraryEvent::FolderListed {
                request_id: Some(1),
                browse_epoch: EPOCH,
                depth: 0,
                count: 3,
                books: 2,
                selection: 2,
            },
        );
        assert_eq!(out.library_depth, 0);
        assert_eq!(out.selection, 2, "returning lands on the row left from");
        assert_eq!(press(out, Button::Back).view, AppView::Home);
    }

    /// A move holds the list still: the rows are about to be replaced, so a
    /// press against the ones on screen would act on a listing already gone.
    /// Back is the exception, and it leaves rather than stacking a move.
    #[test]
    fn a_move_in_flight_holds_the_list() {
        let waiting = press(in_folder(1, 2, 1), Button::Confirm);
        assert!(!waiting.library_browse.is_idle());

        assert_eq!(
            press(waiting, Button::Next).selection,
            waiting.selection,
            "the cursor cannot move off the rows being replaced"
        );
        assert_eq!(
            press(waiting, Button::Confirm).library_browse,
            waiting.library_browse,
            "and no second move starts"
        );
        assert_eq!(
            press(waiting, Button::PagePrevious).library_menu,
            LibraryMenu::None,
            "nor does the actions sheet"
        );

        let left = press(waiting, Button::Back);
        assert_eq!(left.view, AppView::Home);
        assert!(
            left.library_browse.is_idle(),
            "leaving drops the claim to the answer"
        );
    }

    /// The answer to a move nobody is waiting on any more changes nothing.
    #[test]
    fn a_move_walked_away_from_settles_on_nobody() {
        let waiting = press(in_folder(1, 2, 1), Button::Confirm);
        let left = press(waiting, Button::Back);
        let late = left.apply_library_event(
            CTX,
            LibraryEvent::FolderListed {
                request_id: Some(1),
                browse_epoch: EPOCH,
                depth: 4,
                count: 9,
                books: 9,
                selection: 8,
            },
        );
        assert_eq!(late.view, AppView::Home);
        assert_eq!(late.library_depth, left.library_depth);
        assert_eq!(late.library_count, left.library_count);
    }

    /// A relist nobody asked for, which a scan produces, is adopted only
    /// when nothing is in flight.
    #[test]
    fn an_unsolicited_listing_yields_to_a_move() {
        let unsolicited = LibraryEvent::FolderListed {
            request_id: None,
            browse_epoch: EPOCH,
            depth: 0,
            count: 5,
            books: 4,
            selection: 0,
        };

        let idle = in_folder(1, 2, 1).apply_library_event(CTX, unsolicited);
        assert_eq!((idle.library_count, idle.library_books), (5, 4));
        assert_eq!(idle.library_depth, 0);

        let waiting = press(in_folder(1, 2, 1), Button::Confirm);
        let held = waiting.apply_library_event(CTX, unsolicited);
        assert_eq!(
            (held.library_count, held.library_books),
            (waiting.library_count, waiting.library_books),
            "a move still resolving is not overwritten by a relist",
        );
        assert!(!held.library_browse.is_idle());
    }

    /// A card that answered the scan and then would not answer for the rows
    /// is not a card with no books on it. The reader is at the root with
    /// nothing loaded either way; what the screen says about it differs, and
    /// the count alone cannot carry that.
    #[test]
    fn an_unreadable_library_is_not_an_empty_one() {
        let listed = in_library(2, 5).apply_library_event(
            CTX,
            LibraryEvent::FolderListed {
                request_id: None,
                browse_epoch: EPOCH,
                depth: 0,
                count: 0,
                books: 0,
                selection: 0,
            },
        );
        let unreadable = in_library(2, 5).apply_library_event(
            CTX,
            LibraryEvent::LibraryUnreadable {
                browse_epoch: EPOCH,
            },
        );
        assert_eq!(
            (listed.library_count, listed.library_depth),
            (unreadable.library_count, unreadable.library_depth),
            "both leave the reader at the root with nothing loaded",
        );
        assert_eq!(unreadable.selection, 0);
        assert!(unreadable.library_browse.is_idle());
    }

    /// A move is doomed by the storage task being repositioned, not by the
    /// catalog being replaced. The two come apart: a scan whose recovery is
    /// unfinished declines to rebuild the catalog, so its epoch stands, and
    /// goes back to the library root anyway. A move issued in the folder that
    /// scan just left would be read against the root if it were allowed to
    /// run, where the same row number names a different child of a different
    /// place.
    #[test]
    fn an_unreadable_library_overrules_a_move_from_the_position_it_left() {
        let mut at = in_folder(1, 2, 1);
        at.library_browse_epoch = EPOCH;
        let moving = press(at, Button::Confirm);
        assert_eq!(moving.library_browse.browse_epoch(), Some(EPOCH));

        let newer = moving.apply_library_event(
            CTX,
            LibraryEvent::LibraryUnreadable {
                browse_epoch: EPOCH + 1,
            },
        );
        assert!(
            newer.library_browse.is_idle(),
            "the move was issued somewhere the storage task is not any more",
        );
        assert_eq!(newer.library_depth, 0);
        assert_eq!(newer.library_count, 0);
        assert_eq!(newer.library_browse_epoch, EPOCH + 1);

        // A report from the position the move was issued in leaves it alone,
        // because it can still land there.
        let same = moving.apply_library_event(
            CTX,
            LibraryEvent::LibraryUnreadable {
                browse_epoch: EPOCH,
            },
        );
        assert!(!same.library_browse.is_idle());
        assert_eq!(same.library_depth, moving.library_depth);
    }

    /// The whole sequence, with the catalog deliberately held still: a move
    /// pressed in a folder, a scan that rebuilds nothing but repositions to
    /// the root, and the move's own refusal arriving afterwards. Both ends
    /// have to finish at the root.
    #[test]
    fn a_scan_that_only_repositions_still_dooms_a_move() {
        let mut at = in_folder(1, 2, 1);
        at.library_browse_epoch = EPOCH;
        let moving = press(at, Button::Confirm);
        let outstanding = moving.library_browse.request_id().expect("a move is out");

        // The catalog is untouched, so `Scanned` carries the epoch it always
        // had. It settles nothing on its own.
        let scanned = moving.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 4,
                catalog_epoch: EPOCH,
            },
        );
        assert!(!scanned.library_browse.is_idle());

        let relisted = scanned.apply_library_event(
            CTX,
            LibraryEvent::FolderListed {
                request_id: None,
                browse_epoch: EPOCH + 1,
                depth: 0,
                count: 4,
                books: 4,
                selection: 0,
            },
        );
        assert!(
            relisted.library_browse.is_idle(),
            "the reposition doomed the move even though the catalog stood still",
        );
        assert_eq!(relisted.library_depth, 0);

        // And the doomed move's refusal lands on nobody.
        let refused = relisted.apply_library_event(
            CTX,
            LibraryEvent::RowFailed {
                request_id: outstanding,
            },
        );
        assert_eq!(refused.library_depth, 0);
        assert_eq!(refused.library_count, 4);
        assert_eq!(refused.library_browse_epoch, EPOCH + 1);
    }

    /// A row picked in one folder cannot be spent in another: the command
    /// carries the position it was picked in, which is what lets the storage
    /// task refuse it.
    #[test]
    fn a_browse_command_carries_the_position_its_row_was_picked_in() {
        let mut at = in_folder(1, 2, 1);
        at.library_browse_epoch = EPOCH + 3;
        let previous = at;
        let pressed = press(at, Button::Confirm);
        assert!(matches!(
            library_browse_command_for_transition(&previous, &pressed),
            Some(StorageCommand::ChooseLibraryRow { browse_epoch: e, .. }) if e == EPOCH + 3
        ));

        let leaving = press(at, Button::Back);
        assert!(matches!(
            library_browse_command_for_transition(&previous, &leaving),
            Some(StorageCommand::LeaveLibraryFolder { browse_epoch: e, .. }) if e == EPOCH + 3
        ));
    }

    /// A row resolved in a catalog the app has since been told was replaced
    /// opens nothing. This is the half of the fence that catches the ordering
    /// where the scan's `Scanned` overtakes the resolution; the other half is
    /// on the storage side, for the ordering where it does not.
    #[test]
    fn a_row_resolved_in_a_replaced_catalog_opens_nothing() {
        let waiting = press(in_library(1, 3), Button::Confirm);
        let outstanding = waiting.library_browse.request_id().expect("a move is out");
        let rebuilt = waiting.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 3,
                catalog_epoch: EPOCH + 1,
            },
        );

        let answered = rebuilt.apply_library_event(
            CTX,
            LibraryEvent::RowIsBook {
                request_id: outstanding,
                index: 1,
                catalog_epoch: EPOCH,
            },
        );
        assert_eq!(
            answered.view,
            AppView::Library,
            "the row number was resolved in a catalog that is gone",
        );
        assert_eq!(
            answered.book_id, rebuilt.book_id,
            "and the reader is left on the book they were already on",
        );
        assert!(answered.library_browse.is_idle(), "and the wait ends");

        // Resolved in the catalog the app holds, it opens as usual.
        let fresh = rebuilt.apply_library_event(
            CTX,
            LibraryEvent::RowIsBook {
                request_id: outstanding,
                index: 1,
                catalog_epoch: EPOCH + 1,
            },
        );
        assert_eq!(fresh.view, AppView::Reading);
        assert_eq!(fresh.book_id, ReaderSource::sd(1).book_id());
    }

    /// A row that cannot be acted on ends the wait and moves nothing.
    #[test]
    fn a_row_that_fails_leaves_the_list_alone() {
        let waiting = press(in_folder(1, 2, 1), Button::Confirm);
        let failed = waiting.apply_library_event(CTX, LibraryEvent::RowFailed { request_id: 1 });
        assert!(failed.library_browse.is_idle());
        assert_eq!(failed.view, AppView::Library);
        assert_eq!(failed.selection, waiting.selection);
        assert_eq!(failed.library_count, waiting.library_count);
    }

    /// A move the storage queue refused settles here, or the list stays
    /// frozen on rows nothing is coming to replace.
    #[test]
    fn a_move_whose_command_never_left_settles_as_a_standstill() {
        let waiting = press(in_folder(1, 2, 1), Button::Confirm);
        assert!(!waiting.library_browse.is_idle());

        let settled = waiting.library_browse_rejected();
        assert!(settled.library_browse.is_idle());
        assert_eq!(settled.view, AppView::Library, "nothing moved");
        assert_eq!(settled.selection, waiting.selection);
        assert_eq!(settled.library_count, waiting.library_count);
        // And the list takes presses again.
        assert_eq!(
            press(settled, Button::Next).selection,
            waiting.selection + 1
        );
    }

    /// A rescan landing on top of a move: the scan replaces the catalog and
    /// puts the storage task back at the root, so its relist outranks a move
    /// issued against the catalog it replaced. Without that, the move can
    /// only come back refused, and the screen would keep describing a folder
    /// the storage task has already left while every later command landed on
    /// the root it is actually in.
    #[test]
    fn a_rescan_takes_the_screen_back_from_a_move_it_doomed() {
        let moving = press(in_folder(1, 2, 1), Button::Confirm);
        assert_eq!(moving.library_browse.browse_epoch(), Some(EPOCH));

        let scanned = moving.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 4,
                catalog_epoch: EPOCH + 1,
            },
        );
        assert!(
            !scanned.library_browse.is_idle(),
            "the scan alone does not settle the wait",
        );

        let relisted = scanned.apply_library_event(
            CTX,
            LibraryEvent::FolderListed {
                request_id: None,
                browse_epoch: EPOCH + 1,
                depth: 0,
                count: 4,
                books: 4,
                selection: 0,
            },
        );
        assert!(
            relisted.library_browse.is_idle(),
            "the newer listing cancels the move it doomed",
        );
        assert_eq!(relisted.library_depth, 0);
        assert_eq!((relisted.library_count, relisted.library_books), (4, 4));

        // The doomed move's refusal then lands on nobody, and the screen
        // still describes the root the storage task is in.
        let refused = relisted.apply_library_event(CTX, LibraryEvent::RowFailed { request_id: 1 });
        assert_eq!(refused.library_depth, 0);
        assert_eq!((refused.library_count, refused.library_books), (4, 4));
        assert!(refused.library_browse.is_idle());
    }

    /// A folder is a place, not a book: the per-book actions sheet does not
    /// open on one.
    #[test]
    fn the_actions_sheet_does_not_open_on_a_folder_row() {
        let on_book = in_folder(1, 2, 1);
        assert_eq!(
            press(on_book, Button::PagePrevious).library_menu,
            LibraryMenu::Sheet { row: 0 }
        );

        let on_folder = in_folder(2, 2, 1);
        assert_eq!(
            press(on_folder, Button::PagePrevious).library_menu,
            LibraryMenu::None,
        );
    }

    #[test]
    fn library_back_key_returns_home_without_opening() {
        let state = press(ReaderState::boot(), Button::Back).apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 2,
                catalog_epoch: 0,
            },
        );
        let state = press(press(state, Button::Next), Button::Back);
        assert_eq!(state.view, AppView::Home);
        // Browsing did not open anything: the active book is still the
        // scan-time default (first catalog entry), not the browsed row.
        assert_eq!(state.book_id, ReaderSource::sd(0).book_id());
    }

    #[test]
    fn reading_next_previous_bounds_sd_pages() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = 2;
        state.sd_page_count = 2;
        assert_eq!(press(state, Button::Next).page, 1);
        assert_eq!(press(press(state, Button::Next), Button::Next).page, 1);
        assert_eq!(press(press(state, Button::Next), Button::Previous).page, 0);
    }

    #[test]
    fn chapter_selection_changes_reading_chapter() {
        let mut state = ReaderState::boot();
        // Chapter grammar, not orientation: landscape so Confirm acts at
        // once instead of first summoning the portrait key sheet.
        state.orientation = DisplayOrientation::LandscapeButtonsBottom;
        state.view = AppView::Reading;
        state.book_id = 1;
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Chapters);
        let state = press(press(state, Button::Next), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.chapter, 1);
    }

    #[test]
    fn sd_chapter_selection_uses_toc_page_target() {
        let mut state = ReaderState::boot();
        // Chapter grammar, not orientation: landscape so Confirm acts at
        // once instead of first summoning the portrait key sheet.
        state.orientation = DisplayOrientation::LandscapeButtonsBottom;
        state.view = AppView::Reading;
        state.book_id = ReaderSource::sd(0).book_id();
        state.sd_page_count = 40;
        state.sd_chapter_count = 3;
        state.sd_chapter_pages[0] = 0;
        state.sd_chapter_pages[1] = 12;
        state.sd_chapter_pages[2] = 24;

        let state = press(state, Button::Confirm);
        let state = press(press(state, Button::Next), Button::Confirm);

        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.chapter, 1);
        assert_eq!(state.page, 12);
    }

    #[test]
    fn long_toc_selection_keeps_u16_indices() {
        let ctx = ReducerContext::new(1, 1);
        for chapter_count in [255u16, 256, 257, 322] {
            let mut state = ReaderState::boot();
            state.book_id = ReaderSource::sd(0).book_id();
            state.view = AppView::Chapters;
            state.sd_chapter_count = chapter_count;
            state.selection = chapter_count - 1;
            let state = press(state, Button::Confirm);
            assert_eq!(state.chapter, chapter_count - 1);
            assert_eq!(state.chapter_item_count(ctx), chapter_count);
        }
    }

    #[test]
    fn sd_page_navigation_tracks_chapter_without_wrapping_pages() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = ReaderSource::sd(0).book_id();
        state.page = 11;
        state.sd_page_count = 40;
        state.sd_chapter_count = 3;
        state.sd_chapter_pages[0] = 0;
        state.sd_chapter_pages[1] = 12;
        state.sd_chapter_pages[2] = 24;

        let state = press(state, Button::Next);

        assert_eq!(state.page, 12);
        assert_eq!(state.chapter, 1);
    }

    #[test]
    fn sd_page_navigation_tracks_chapters_past_first_screen() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = ReaderSource::sd(0).book_id();
        state.sd_page_count = 400;
        state.sd_chapter_count = 40;
        for index in 0..40 {
            state.sd_chapter_pages[index] = (index as u16) * 10;
        }
        state.page = 249;
        state.chapter = 23;

        let state = press(state, Button::Next);

        assert_eq!(state.page, 250);
        assert_eq!(state.chapter, 25);
        assert_eq!(state.selection, 25);
    }

    #[test]
    fn catalog_scan_does_not_auto_open_from_files() {
        let state = press(ReaderState::boot(), Button::Back);
        assert_eq!(state.view, AppView::Library);
        assert!(!state.read_request_pending);

        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 2,
                catalog_epoch: 0,
            },
        );
        assert_eq!(state.view, AppView::Library);
        assert_eq!(state.library_count, 2);
        assert!(!state.read_request_pending);
    }

    #[test]
    fn scan_defaults_home_to_first_sd_book_until_restore() {
        let state = ReaderState::boot();
        assert_eq!(state.book_id, 1);

        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 3,
                catalog_epoch: 0,
            },
        );
        assert_eq!(state.book_id, ReaderSource::sd(0).book_id());
        assert_eq!(state.chapter, 0);
        assert_eq!(state.page, 0);

        // Saved progress arriving after the scan wins over the default.
        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Restored {
                book_id: ReaderSource::sd(2).book_id(),
                chapter: 4,
                page: 12,
                page_count: 0,
                reading_orientation: DisplayOrientation::LandscapeButtonsBottom as u8,
                refresh_policy: RefreshPolicy::FullOnWake as u8,
                font_size: FontSize::Medium as u8,
                line_spacing: LineSpacing::Normal as u8,
                font_weight: FontWeight::Normal as u8,
                font_family: FontFamily::Literata as u8,
                front_buttons: FrontButtons::PagesRight as u8,
            },
        );
        assert_eq!(state.book_id, ReaderSource::sd(2).book_id());

        // A later rescan must not yank the restored book back.
        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 3,
                catalog_epoch: 0,
            },
        );
        assert_eq!(state.book_id, ReaderSource::sd(2).book_id());
    }

    #[test]
    fn scan_keeps_an_open_builtin_book() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 3,
                catalog_epoch: 0,
            },
        );
        assert_eq!(state.book_id, 1);
    }

    #[test]
    fn restore_keeps_home_key_selection() {
        let state = ReaderState::boot();
        let home_selection = state.selection;
        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Restored {
                book_id: ReaderSource::sd(1).book_id(),
                chapter: 9,
                page: 70,
                page_count: 0,
                reading_orientation: DisplayOrientation::LandscapeButtonsBottom as u8,
                refresh_policy: RefreshPolicy::FullOnWake as u8,
                font_size: FontSize::Medium as u8,
                line_spacing: LineSpacing::Normal as u8,
                font_weight: FontWeight::Normal as u8,
                font_family: FontFamily::Literata as u8,
                front_buttons: FrontButtons::PagesRight as u8,
            },
        );
        assert_eq!(state.selection, home_selection);
        assert_eq!(state.chapter, 9);
        assert_eq!(state.page, 70);
    }

    #[test]
    fn library_open_before_scan_stays_in_files() {
        let state = press(ReaderState::boot(), Button::Back);
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Library);
        assert_eq!(state.book_id, 1);

        let state = state.apply_library_event(
            CTX,
            LibraryEvent::Scanned {
                count: 2,
                catalog_epoch: 0,
            },
        );
        assert_eq!(state.view, AppView::Library);
        assert_eq!(state.library_count, 2);
    }

    #[test]
    fn settings_change_key_cycles_refresh_policy() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state.selection = 4;
        let state = press(state, Button::Confirm);
        assert_eq!(state.refresh_policy, RefreshPolicy::FullEveryTen);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn settings_change_key_cycles_type_size_spacing_and_weight() {
        let state = press(ReaderState::boot(), Button::Next);
        assert_eq!(state.selection, 0);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Merriweather);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Literata);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 1);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_size, FontSize::Large);
        let state = press(press(state, Button::Confirm), Button::Confirm);
        assert_eq!(state.font_size, FontSize::Medium);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 2);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_weight, FontWeight::Heavy);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_weight, FontWeight::Normal);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 3);
        let state = press(state, Button::Confirm);
        assert_eq!(state.line_spacing, LineSpacing::Relaxed);

        let state = press(state, Button::Next);
        assert_eq!(state.selection, 4);
        let state = press(state, Button::Next);
        assert_eq!(state.selection, 5);
        let state = press(state, Button::Next);
        assert_eq!(state.selection, 6);
        let state = press(state, Button::Next);
        assert_eq!(state.selection, 0, "selection wraps after the last row");
    }

    #[test]
    fn settings_change_key_toggles_front_buttons() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state.selection = 6;

        let state = press(state, Button::Confirm);
        assert_eq!(state.front_buttons, FrontButtons::PagesLeft);

        // With the pairs swapped, the physical Next key (second key of the
        // pair now holding back/confirm) carries Confirm's change action,
        // and toggles the setting back.
        let state = press(state, Button::Next);
        assert_eq!(state.front_buttons, FrontButtons::PagesRight);
    }

    #[test]
    fn pages_left_swaps_the_front_pairs_whole() {
        let mut state = ReaderState::boot();
        state.front_buttons = FrontButtons::PagesLeft;
        // The swap is the subject: landscape keeps every press direct, so a
        // summoned portrait key sheet cannot absorb the first one.
        state.orientation = DisplayOrientation::LandscapeButtonsBottom;
        state.view = AppView::Reading;

        // Reading: the physical back/confirm pair now turns pages (order
        // kept within the pair), and the old page pair carries back/confirm.
        state.book_id = ReaderSource::sd(0).book_id();
        state.sd_page_count = 10;
        state.page = 5;
        assert_eq!(press(state, Button::Back).page, 4);
        assert_eq!(press(state, Button::Confirm).page, 6);
        assert_eq!(press(state, Button::Previous).view, AppView::Home);
        assert_eq!(press(state, Button::Next).view, AppView::Chapters);

        // The side page rail is untouched.
        assert_eq!(press(state, Button::PageNext).page, 6);
    }

    #[test]
    fn home_ignores_the_front_pair_swap() {
        // Home is positional: the same physical key opens the same view
        // whether or not the pairs are swapped, so the title page reads
        // identically for every user.
        let mut state = ReaderState::boot();
        state.front_buttons = FrontButtons::PagesLeft;

        assert_eq!(press(state, Button::Back).view, AppView::Library);
        assert_eq!(press(state, Button::Confirm).view, AppView::Reading);
        assert_eq!(press(state, Button::Previous).view, AppView::Wireless);
        assert_eq!(press(state, Button::Next).view, AppView::Settings);
    }

    #[test]
    fn settings_change_key_cycles_the_three_offered_orientations() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state.selection = 5;

        // Portrait is the boot hold and keeps the front column's order, so
        // Confirm stays Confirm and the first change leaves for landscape.
        let state = press(state, Button::Confirm);
        assert_eq!(
            state.orientation,
            DisplayOrientation::LandscapeButtonsBottom
        );

        let state = press(state, Button::Confirm);
        assert_eq!(state.orientation, DisplayOrientation::LandscapeButtonsTop);

        // Rotated 180 degrees, the physical Previous key sits where Confirm
        // was, so it carries the change action. The cycle wraps back to the
        // boot hold, skipping the unoffered buttons-above portrait.
        let state = press(state, Button::Previous);
        assert_eq!(state.orientation, DisplayOrientation::PortraitButtonsLeft);
    }

    #[test]
    fn portrait_keeps_all_physical_buttons() {
        let mut state = ReaderState::boot();
        state.orientation = DisplayOrientation::PortraitButtonsLeft;

        assert_eq!(press(state, Button::Back).view, AppView::Library);
        assert_eq!(press(state, Button::Confirm).view, AppView::Reading);

        // The side pair keeps its physical sense: the hardware walk showed
        // the forward key already lands at its natural end in portrait.
        // In Library the side-back key is repurposed: it opens the per-book
        // actions sheet instead of scrolling (the front pair scrolls both
        // ways and side-forward wraps, so navigation survives).
        let mut library = press(state, Button::Back);
        library.library_count = 3;
        library.library_books = 3;
        let next = press(library, Button::PageNext);
        assert_eq!(next.selection, 1);
        let asked = press(next, Button::PagePrevious);
        assert_eq!(asked.selection, 1);
        assert_eq!(asked.library_menu, LibraryMenu::Sheet { row: 0 });
    }

    #[test]
    fn portrait_reading_summons_the_sheet_before_acting() {
        let mut state = press(ReaderState::boot(), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);
        state.orientation = DisplayOrientation::PortraitButtonsLeft;

        // First named-key press summons; the second acts on its label.
        let state = press(state, Button::Confirm);
        assert!(state.reading_sheet);
        assert_eq!(state.view, AppView::Reading, "summoning is not an action");
        let state = press(state, Button::Confirm);
        assert!(!state.reading_sheet);
        assert_eq!(state.view, AppView::Chapters);

        // Back out of Chapters returns to a sheetless page; Back then
        // summons, and a second Back leaves for Home.
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Reading);
        assert!(!state.reading_sheet);
        let state = press(state, Button::Back);
        assert!(state.reading_sheet);
        let state = press(state, Button::Back);
        assert_eq!(state.view, AppView::Home);
        assert!(!state.reading_sheet);
    }

    #[test]
    fn portrait_page_turns_never_wait_on_the_sheet() {
        let mut state = press(ReaderState::boot(), Button::Confirm);
        state.orientation = DisplayOrientation::PortraitButtonsLeft;

        // The browse pair pages immediately — no summon toll.
        let chapter_before = state.chapter;
        let state = press(state, Button::Next);
        assert!(!state.reading_sheet);
        assert_ne!(state.chapter, chapter_before, "next paged immediately");

        // And a page turn dismisses an up sheet.
        let state = press(state, Button::Confirm);
        assert!(state.reading_sheet);
        let state = press(state, Button::Next);
        assert!(!state.reading_sheet);
        assert_eq!(state.view, AppView::Reading);
    }

    #[test]
    fn landscape_reading_keeps_direct_key_mappings() {
        let mut state = ReaderState::boot();
        state.orientation = DisplayOrientation::LandscapeButtonsBottom;
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Reading);

        // No sheet in landscape: Confirm opens Chapters on the first press.
        let state = press(state, Button::Confirm);
        assert_eq!(state.view, AppView::Chapters, "no sheet in landscape");
        assert!(!state.reading_sheet);
    }

    #[test]
    fn landscape_top_rotates_front_button_mapping() {
        let mut state = ReaderState::boot();
        state.orientation = DisplayOrientation::LandscapeButtonsTop;

        assert_eq!(press(state, Button::Back).view, AppView::Settings);
        assert_eq!(press(state, Button::Confirm).view, AppView::Wireless);
        assert_eq!(press(state, Button::Previous).view, AppView::Reading);
        assert_eq!(press(state, Button::Next).view, AppView::Library);
    }

    #[test]
    fn landscape_top_swaps_page_buttons() {
        let mut state = press(ReaderState::boot(), Button::Back);
        state.library_count = 3;
        state.library_books = 3;
        state.orientation = DisplayOrientation::LandscapeButtonsTop;

        let next = press(state, Button::PagePrevious);
        assert_eq!(next.selection, 1);

        // The physically-lower key maps to logical side-back, which in
        // Library opens the per-book actions sheet (see the portrait test
        // above for the repurpose rationale).
        let asked = press(next, Button::PageNext);
        assert_eq!(asked.selection, 1);
        assert_eq!(asked.library_menu, LibraryMenu::Sheet { row: 0 });
    }

    #[test]
    fn settings_typeface_cycles_custom_only_when_available() {
        let mut state = press(ReaderState::boot(), Button::Next);
        state = state.apply_library_event(CTX, LibraryEvent::CustomFont { available: true });
        assert_eq!(state.selection, 0);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Merriweather);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Custom);
        let state = press(state, Button::Confirm);
        assert_eq!(state.font_family, FontFamily::Literata);
    }

    #[test]
    fn removing_custom_font_falls_back_to_literata() {
        let mut state = ReaderState::boot();
        state.custom_font_available = true;
        state.font_family = FontFamily::Custom;
        let state = state.apply_library_event(CTX, LibraryEvent::CustomFont { available: false });
        assert_eq!(state.font_family, FontFamily::Literata);
        assert!(!state.custom_font_available);
    }

    #[test]
    fn library_restore_updates_progress_and_preferences() {
        let state = ReaderState::boot().apply_library_event(
            CTX,
            LibraryEvent::Restored {
                book_id: 2,
                chapter: 4,
                page: 12,
                page_count: 0,
                reading_orientation: DisplayOrientation::PortraitButtonsRight as u8,
                refresh_policy: RefreshPolicy::FastOnly as u8,
                font_size: FontSize::Large as u8,
                line_spacing: LineSpacing::Compact as u8,
                font_weight: FontWeight::Normal as u8,
                font_family: FontFamily::Literata as u8,
                front_buttons: FrontButtons::PagesRight as u8,
            },
        );
        assert_eq!(state.book_id, 2);
        assert_eq!(state.chapter, 4);
        assert_eq!(state.page, 12);
        assert_eq!(state.orientation, DisplayOrientation::PortraitButtonsRight);
        assert_eq!(state.refresh_policy, RefreshPolicy::FastOnly);
        assert_eq!(state.font_size, FontSize::Large);
        assert_eq!(state.line_spacing, LineSpacing::Compact);
    }

    #[test]
    fn refresh_plan_cleans_after_type_settings_change() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Settings;
        let request = state.render_request(RenderKind::Page);
        planner.record_render(request, RefreshMode::Full);

        state.font_size = FontSize::Large;
        assert_eq!(
            planner.mode_for(state.render_request(RenderKind::Page)),
            RefreshMode::FastClean
        );
    }

    #[test]
    fn refresh_plan_cleans_after_orientation_change() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Settings;
        let request = state.render_request(RenderKind::Page);
        planner.record_render(request, RefreshMode::Full);

        state.orientation = DisplayOrientation::LandscapeButtonsTop;
        assert_eq!(
            planner.mode_for(state.render_request(RenderKind::Page)),
            RefreshMode::FastClean
        );
    }

    #[test]
    fn refresh_plan_uses_fast_clean_for_context_changes_and_fast_for_selection() {
        let mut planner = RefreshPlanner::new();
        let mut request = ReaderState::boot().render_request(RenderKind::Boot);

        // Cold boot is the only render where panel contents are unknown,
        // so it keeps the deep multi-flash full waveform.
        assert_eq!(planner.mode_for(request), RefreshMode::Full);
        planner.record_render(request, RefreshMode::Full);

        request.kind = RenderKind::Page;
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);

        request.view = AppView::Settings;
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
        planner.record_render(request, RefreshMode::FastClean);

        // Cursor moves inside Settings ride the fast differential refresh
        // against the prestaged previous frame; leaving the view is a view
        // change, which gets the one-flicker cleaning refresh.
        request.selection = 1;
        assert_eq!(planner.mode_for(request), RefreshMode::Fast);
        planner.record_render(request, RefreshMode::Fast);

        request.view = AppView::Home;
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
    }

    #[test]
    fn panel_refresh_failure_is_never_acknowledged_as_settled() {
        assert_eq!(
            display_refresh_outcome(true, None),
            (settled(), PowerEvent::DisplaySettled)
        );
        assert_eq!(
            display_refresh_outcome(false, None),
            (
                DisplayEvent::RefreshFailed,
                PowerEvent::DisplayRefreshFailed
            )
        );
    }

    #[test]
    fn refresh_plan_keeps_library_selection_fast() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Library;
        state.library_count = 3;
        let mut request = state.render_request(RenderKind::Page);

        planner.record_render(request, RefreshMode::Full);
        request.selection = 1;

        assert_eq!(planner.mode_for(request), RefreshMode::Fast);
    }

    /// Every step of the actions sheet covers or uncovers list rows, so each
    /// one earns the one-flicker clean rather than ghosting through a partial.
    #[test]
    fn refresh_plan_cleans_for_every_library_menu_step() {
        let mut planner = RefreshPlanner::new();
        let mut state = in_library(1, 4);
        let mut last = state.render_request(RenderKind::Page);
        planner.record_render(last, RefreshMode::Full);

        for button in [
            Button::PagePrevious, // None -> Sheet: the card covers the rows
            Button::Confirm,      // Sheet -> Busy: rail and footer change
        ] {
            state = press(state, button);
            let request = state.render_request(RenderKind::Page);
            assert_ne!(request.library_menu, last.library_menu);
            assert_eq!(
                planner.mode_for(request),
                RefreshMode::FastClean,
                "{:?} -> {:?} redraws the list area",
                last.library_menu,
                request.library_menu
            );
            planner.record_render(request, RefreshMode::FastClean);
            last = request;
        }

        // Settling swaps the footer note in place...
        let settled = state.apply_library_event(
            CTX,
            LibraryEvent::CacheCleared {
                request_id: outstanding(state),
                ok: true,
            },
        );
        let request = settled.render_request(RenderKind::Page);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
        planner.record_render(request, RefreshMode::FastClean);

        // ...and the next press takes the whole card back off the rows.
        let dismissed = press(settled, Button::Next).render_request(RenderKind::Page);
        assert_eq!(dismissed.library_menu, LibraryMenu::None);
        assert_eq!(planner.mode_for(dismissed), RefreshMode::FastClean);
    }

    #[test]
    fn refresh_plan_keeps_chapter_selection_fast() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.view = AppView::Chapters;
        let mut request = state.render_request(RenderKind::Page);

        planner.record_render(request, RefreshMode::Full);
        request.selection = 1;

        assert_eq!(planner.mode_for(request), RefreshMode::Fast);
    }

    #[test]
    fn refresh_plan_counts_fast_refreshes_and_resets_on_sleep() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.refresh_policy = RefreshPolicy::FullEveryTen;
        let request = state.render_request(RenderKind::Page);
        planner.record_render(request, RefreshMode::Full);

        for _ in 0..DEFAULT_FULL_REFRESH_INTERVAL {
            assert_eq!(planner.mode_for(request), RefreshMode::Fast);
            planner.record_render(request, RefreshMode::Fast);
        }
        // Periodic mid-reading cleanup uses the one-flicker clean instead
        // of the jarring multi-flash full waveform.
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);

        // After a display sleep the panel shows the sleep screen the
        // firmware drew, so wake also needs only the one-flicker clean.
        planner.record_sleep(true);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
    }

    #[test]
    fn a_skipped_render_updates_what_is_displayed_without_counting_a_refresh() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        state.refresh_policy = RefreshPolicy::FullEveryTen;
        let request = state.render_request(RenderKind::Page);
        planner.record_render(request, RefreshMode::Full);

        // One short of the clean, so the next counted Fast would trip it.
        for _ in 0..DEFAULT_FULL_REFRESH_INTERVAL - 1 {
            planner.record_render(request, RefreshMode::Fast);
        }
        assert_eq!(planner.mode_for(request), RefreshMode::Fast);

        // A skip drove no waveform, so it leaves the count where it was.
        // Any number of them do.
        for _ in 0..20 {
            planner.record_skipped_render(request);
            assert_eq!(
                planner.mode_for(request),
                RefreshMode::Fast,
                "a skipped render must not drift the clean earlier",
            );
        }

        // And the next real Fast still trips it on schedule.
        planner.record_render(request, RefreshMode::Fast);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
    }

    #[test]
    fn a_skipped_render_still_says_what_the_panel_shows() {
        let mut planner = RefreshPlanner::new();
        let mut state = ReaderState::boot();
        let boot = state.render_request(RenderKind::Boot);
        planner.record_render(boot, RefreshMode::Full);

        state.selection = 1;
        let moved = state.render_request(RenderKind::Page);
        planner.record_skipped_render(moved);
        assert_eq!(
            planner.last_request(),
            Some(moved),
            "the planner models the frame the seam settled on, flushed or not",
        );
        assert!(planner.screen_on(), "a skip leaves the screen lit");
        // Not a cold boot afterwards: the panel's contents are known, so the
        // next render keeps its ordinary mode rather than the deep waveform.
        assert_ne!(planner.mode_for(moved), RefreshMode::Full);
    }

    #[test]
    fn refresh_plan_keeps_deep_full_for_cold_boot_only() {
        let mut planner = RefreshPlanner::new();
        let request = ReaderState::boot().render_request(RenderKind::Boot);

        // Cold boot: unknown panel contents, deep full waveform.
        assert_eq!(planner.mode_for(request), RefreshMode::Full);
        planner.record_render(request, RefreshMode::Full);

        // Wake after sleep: known sleep-screen contents, one-flicker clean.
        planner.record_sleep(true);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);
        planner.record_render(request, RefreshMode::FastClean);
        planner.record_sleep(true);

        // A sleep whose final flush failed still powers the panel down, so
        // the screen is off and the next render re-inits — but the pixels
        // underneath are stale, and the wake render must pay the deep full
        // waveform instead of fast-cleaning over them.
        let mut failed_flush = RefreshPlanner::new();
        failed_flush.record_render(request, RefreshMode::Full);
        failed_flush.record_sleep(false);
        assert!(!failed_flush.screen_on());
        assert_eq!(failed_flush.last_request(), None);
        assert_eq!(failed_flush.mode_for(request), RefreshMode::Full);

        // Disabling fast refresh falls back to the deep full everywhere.
        let conservative = RefreshPlanner::new().with_fast_refresh_enabled(false);
        assert_eq!(conservative.mode_for(request), RefreshMode::Full);
    }

    #[test]
    fn refresh_plan_seeded_deep_sleep_wake_uses_fast_clean() {
        let request = ReaderState::boot().render_request(RenderKind::Boot);

        // A deep-sleep wake is a cold boot with a fresh planner, but the
        // panel still shows the sleep screen the firmware drew before
        // powering down; the seed lets the wake render take the one-flicker
        // clean instead of the multi-flash full waveform.
        let mut planner = RefreshPlanner::new().with_panel_shows_sleep_screen(true);
        assert_eq!(planner.mode_for(request), RefreshMode::FastClean);

        // The seed is consumed by the first render: from here the planner
        // behaves exactly like an in-session one — the post-boot cleanup
        // pass, then fast differentials for same-context turns.
        planner.record_render(request, RefreshMode::FastClean);
        let mut page = request;
        page.kind = RenderKind::Page;
        assert_eq!(planner.mode_for(page), RefreshMode::FastClean);
        planner.record_render(page, RefreshMode::FastClean);
        page.selection = 1;
        assert_eq!(planner.mode_for(page), RefreshMode::Fast);

        // An unseeded cold boot (battery pull, crash, software reset) still
        // pays the deep full waveform — panel contents are unknown.
        assert_eq!(
            RefreshPlanner::new()
                .with_panel_shows_sleep_screen(false)
                .mode_for(request),
            RefreshMode::Full
        );

        // With fast refresh disabled the seed is ignored.
        let conservative = RefreshPlanner::new()
            .with_panel_shows_sleep_screen(true)
            .with_fast_refresh_enabled(false);
        assert_eq!(conservative.mode_for(request), RefreshMode::Full);
    }

    #[test]
    fn refresh_plan_failure_forces_reinit_and_full_waveform() {
        let mut request = ReaderState::boot().render_request(RenderKind::Page);

        // A failed flush or sleep handshake leaves the panel's RAM and
        // waveform state unknown: the planner forgets the screen, so the
        // next render hits the init guard (screen off, no last request)
        // and pays the deep full waveform instead of fast-diffing against
        // a frame that may never have landed.
        let mut planner = RefreshPlanner::new();
        planner.record_render(request, RefreshMode::Full);
        planner.record_failure();
        assert!(!planner.screen_on());
        assert_eq!(planner.last_request(), None);
        assert_eq!(planner.mode_for(request), RefreshMode::Full);

        // The failure clears panel state, not policy: after one successful
        // render, same-context turns ride the fast differential again.
        planner.record_render(request, RefreshMode::Full);
        request.selection = 1;
        assert_eq!(planner.mode_for(request), RefreshMode::Fast);

        // A failure also revokes a deep-sleep wake seed — the sleep screen
        // can no longer be assumed to be on the panel.
        let mut seeded = RefreshPlanner::new().with_panel_shows_sleep_screen(true);
        seeded.record_failure();
        assert_eq!(seeded.mode_for(request), RefreshMode::Full);

        // Disabled fast refresh stays disabled through a failure.
        let mut conservative = RefreshPlanner::new().with_fast_refresh_enabled(false);
        conservative.record_render(request, RefreshMode::Full);
        conservative.record_failure();
        conservative.record_render(request, RefreshMode::Full);
        assert_eq!(conservative.mode_for(request), RefreshMode::Full);
    }

    /// One of every StorageCommand variant, so the admission table below is
    /// exhaustive by construction: a new variant fails the count assertion
    /// until it is classified here.
    fn every_storage_command() -> [StorageCommand; 11] {
        let persisted = PersistedAppState {
            book_id: 0,
            chapter: 0,
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
        };
        let credentials = WifiCredentials::from_strs("ssid", "pass").unwrap();
        [
            StorageCommand::LoadCatalogCache,
            StorageCommand::RefreshCatalog,
            StorageCommand::OpenBook {
                request_id: 1,
                book_id: 1,
                index: 0,
                catalog_epoch: None,
                chapter: 0,
                target_pages: 0,
                type_settings: TypeSettings::DEFAULT,
                portrait: false,
                previous: None,
            },
            StorageCommand::ExtendSection {
                request_id: 1,
                book_id: 1,
                index: 0,
                chapter: 0,
                target_pages: 0,
                type_settings: TypeSettings::DEFAULT,
                portrait: false,
            },
            StorageCommand::LoadChapters {
                request_id: 1,
                book_id: 1,
                index: 0,
            },
            StorageCommand::JumpChapter {
                request_id: 1,
                book_id: 1,
                index: 0,
                chapter: 0,
                type_settings: TypeSettings::DEFAULT,
                portrait: false,
            },
            StorageCommand::StoreProgress(persisted),
            StorageCommand::LoanSyncMemory,
            StorageCommand::StoreWifiCredentials(credentials),
            StorageCommand::ForgetWifiCredentials,
            StorageCommand::ClearBookCache {
                request_id: 1,
                index: 0,
                browse_epoch: 0,
            },
        ]
    }

    #[test]
    fn idle_sync_session_admits_everything_but_upload() {
        let session = SyncSession::Idle;
        assert!(!session.active());
        for command in every_storage_command() {
            assert!(session.admits(&command), "refused idle: {command:?}");
        }
        // Uploads only exist while the browser shelf is being served.
        assert!(!session.admits(&StorageCommand::ReceiveUpload));
    }

    #[test]
    fn physical_key_reaches_the_action_it_names_under_every_setting() {
        use DisplayOrientation::*;
        use FrontButtons::*;
        let orientations = [
            LandscapeButtonsBottom,
            LandscapeButtonsTop,
            PortraitButtonsLeft,
            PortraitButtonsRight,
        ];
        let actions = [
            Button::Power,
            Button::Back,
            Button::Confirm,
            Button::Previous,
            Button::Next,
            Button::PagePrevious,
            Button::PageNext,
        ];
        let views = [
            AppView::Home,
            AppView::Library,
            AppView::Reading,
            AppView::Chapters,
            AppView::Wireless,
            AppView::Settings,
        ];
        for view in views {
            for orientation in orientations {
                for front in [PagesRight, PagesLeft] {
                    for action in actions {
                        let key = physical_key_for(view, orientation, front, action)
                            .unwrap_or_else(|| {
                                panic!(
                                    "no key reaches {action:?} in {view:?} on {orientation:?}/{front:?}"
                                )
                            });
                        // The point of the inverse: pressing what it returns
                        // has to arrive as what was asked for, through the
                        // maps that view actually applies.
                        assert_eq!(
                            arrives_as(view, orientation, front, key),
                            Some(action),
                            "{key:?} did not reach {action:?} in {view:?} on {orientation:?}/{front:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn swapped_front_pair_moves_a_page_turn_off_the_next_key() {
        // The concrete case the bench injector kept getting wrong: on
        // PagesLeft a raw Next arrives as Confirm, so a scenario that sent
        // Next to turn a page opened the chapter list instead.
        assert_eq!(
            orient_button(
                DisplayOrientation::PortraitButtonsLeft,
                swap_front_pairs(FrontButtons::PagesLeft, Some(Button::Next))
            ),
            Some(Button::Confirm)
        );
        assert_eq!(
            physical_key_for(
                AppView::Reading,
                DisplayOrientation::PortraitButtonsLeft,
                FrontButtons::PagesLeft,
                Button::Next
            ),
            Some(Button::Confirm)
        );
    }

    #[test]
    fn home_continue_reading_survives_either_front_pair() {
        // Home is positional and skips the front-pair swap, so inverting
        // both maps put the selftest on the wrong row: asking for Confirm
        // (continue reading) under PagesLeft produced raw Next, which Home
        // reads as Settings. Driven through the reducer rather than the
        // action table, so it tests the path the device takes.
        // The invariant the fix rests on, asserted before the walk so a
        // regression names itself: Home ignores the front pair, so the key
        // that continues reading there cannot depend on it. Inverting the
        // swap made these two differ, which is the whole defect.
        let orientation = ReaderState::boot().orientation;
        assert_eq!(
            physical_key_for(
                AppView::Home,
                orientation,
                FrontButtons::PagesRight,
                Button::Confirm
            ),
            physical_key_for(
                AppView::Home,
                orientation,
                FrontButtons::PagesLeft,
                Button::Confirm
            ),
        );

        for front in [FrontButtons::PagesRight, FrontButtons::PagesLeft] {
            let mut state = ReaderState::boot();
            state.view = AppView::Home;
            state.front_buttons = front;
            // An SD book is already current, so continuing goes straight to
            // Reading rather than by way of the shelf.
            state.book_id = FIRST_SD_BOOK_ID;
            let key = physical_key_for(state.view, state.orientation, front, Button::Confirm)
                .expect("a key reaches Confirm at Home");
            state = state.apply_input(CTX, InputEvent::button(key));
            assert_eq!(
                state.view,
                AppView::Reading,
                "{key:?} did not continue reading at Home on {front:?}"
            );
        }
    }

    #[test]
    fn loaned_sync_session_admits_only_loan_safe_commands() {
        let mut session = SyncSession::default();
        session.loan_granted();
        assert!(session.active());
        let mut admitted = 0;
        for command in every_storage_command() {
            let loan_safe = matches!(
                command,
                StorageCommand::StoreProgress(_) | StorageCommand::StoreWifiCredentials(_)
            );
            assert_eq!(
                session.admits(&command),
                loan_safe,
                "wrong admission while loaned: {command:?}"
            );
            admitted += usize::from(loan_safe);
        }
        assert_eq!(admitted, 2);
        assert!(session.admits(&StorageCommand::ReceiveUpload));
        // Notably refused: a second loan of memory that is already gone.
        assert!(!session.admits(&StorageCommand::LoanSyncMemory));
    }
}
