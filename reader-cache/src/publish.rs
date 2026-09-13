//! The publish tail: swapping a freshly built index in for the one the reader
//! is currently reading out of.
//!
//! This is the code six review rounds of B4 kept finding defects in, and they
//! were all one shape — a write that fails *after* the store has already been
//! updated. Deleting the cache the reader is reading from; returning before
//! putting their page back in the shared text arena; reporting a walk finished
//! when its index never landed; settling for an index nobody is still building.
//! Not one of them was a subtle logic error. All of them are cleanup ordering,
//! which is exactly what a card that fails the Nth write exposes on the first
//! run.
//!
//! Which is why this module lives in a host-testable crate now. `tests/` drives
//! it against an in-memory FAT image whose reads and writes can be failed on
//! demand, so the orderings below are asserted rather than argued about.
//!
//! Telemetry deliberately does not live here. These functions used to take
//! `Instant` and SD-counter snapshots purely to print bench lines; they now
//! return their outcome and the firmware prints it. Where a diagnostic message
//! genuinely belongs to a decision made here, it goes through `cache_log!`.

use crate::files::{self, CacheLoadResult};
use crate::layout;
use crate::store::ReaderStore;
use embedded_sdmmc::{Directory, TimeSource};
use proto::cache::BookV2SectionRecord;

/// How many newly built sections a background walk accumulates before it
/// rewrites `BOOK.BIN`.
///
/// The index is rewritten whole every time — header, one record per section so
/// far, the TOC block and the labels — so publishing on every slice makes a
/// build's index traffic quadratic in its section count. On the measured
/// 100-section book that is a hundred rewrites of a table that is only growing
/// at the end.
///
/// Nothing reads that file mid-build. Page turns are served from the resident
/// index in `ReaderStore`, which is still adopted every slice; the on-disk copy
/// matters only to the *next* open, and while `resume_spine` is set that open
/// rebuilds regardless of how far the file had got.
///
/// So the only cost of batching is work redone when a walk is abandoned: at
/// most this many sections. The final publish always writes, so a book that
/// finishes is never short-changed.
const INDEX_PUBLISH_SECTIONS: usize = 16;

/// What one publish did, for a caller that wants to report it.
///
/// The cover comes back rather than being loaded by the caller because adopting
/// `COVER.BIN` into the store is *publication*, not telemetry: `selected_cover`
/// gates the Library and sleep renders on it, so a path that publishes without
/// it leaves a readable book with no cover. That is exactly what happened when
/// the cover load was moved out to the firmware's reporting helper — the
/// progressive first open never called it, so a book stayed coverless until its
/// background walk finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishReport {
    pub outcome: BookPublishOutcome,
    /// `None` when the publish failed before the cover was reached.
    pub cover: Option<files::CoverLoadResult>,
}

/// What the publish tail could not do. Deliberately narrow: these are the only
/// two failures this module can produce, so the firmware's wider build-error
/// enum converts from it rather than being reused here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// BOOK.BIN did not land. Whether the cache dir may be cleared afterwards
    /// depends on whether a reader is holding those files — see
    /// [`publish_first_open`] against [`finish_background_walk`].
    IndexWrite,
    /// The index landed but the requested section would not read back, so the
    /// window the reader renders from cannot be trusted.
    SectionRead,
}

/// How the publish tail ended. The two failures need different cleanup:
/// a failed index write leaves a truncated BOOK.BIN behind (created or
/// truncated before the body writes), while a section read-back miss
/// leaves a fully written, valid cache worth keeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookPublishOutcome {
    Ready,
    IndexWriteFailed,
    SectionReadFailed,
}

/// The shared publish tail of a section-cache build: write BOOK.BIN, adopt the
/// index into the store, load the requested section, refresh chapter tracking,
/// and adopt the cover. Used by the progressive first open, the completed EPUB
/// build, and the CONT.BIN replay path.
///
/// It reports rather than prints. The ready and bench lines live in the
/// firmware, which owns the clock and the SD counters — dropping those arguments
/// is what let this function move to a crate a host can test. The cover is the
/// one thing that did *not* go with them: adopting it is publication, not
/// telemetry, and every caller needs it (see [`PublishReport`]).
#[expect(clippy::too_many_arguments)] // Every argument is a distinct input to one write; a struct would exist only for this call.
pub fn publish_book_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    requested_global_page: u32,
    library: &mut ReaderStore,
    sections_slice: &[BookV2SectionRecord],
    total_pages: u32,
    book_partial: bool,
    // Non-zero only for the provisional publish of a suspended walk. A
    // completed book stamps `0`, which is what tells the next open that its
    // missing pages — if any — are missing for good and no rebuild would find
    // them.
    resume_spine: u16,
) -> PublishReport
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let wrote_index = files::write_v2_book_index(
        root,
        cache_key,
        source_identity,
        total_pages,
        sections_slice,
        library,
        book_partial,
        resume_spine,
    );
    if !wrote_index {
        return PublishReport {
            outcome: BookPublishOutcome::IndexWriteFailed,
            cover: None,
        };
    }
    // The index naming this section set is now on the card, so any section
    // file past its end is unreachable from it — safe to delete, and nothing
    // a reader could be holding. Order matters: pruning before the index
    // landed would delete sections the *old* index still names.
    //
    // `resume_spine` is the gate. Non-zero means a suspended walk is coming
    // back to write more sections, and `sections_slice` is its provisional
    // frontier rather than the book's real length; pruning against it would
    // delete the sections that walk is about to need. Only a completed
    // build — which stamps zero — knows the final count.
    if resume_spine == 0 {
        let pruned = files::prune_orphan_sections(
            root,
            cache_key,
            library.layout_key(),
            sections_slice.len().min(u16::MAX as usize) as u16,
        );
        if pruned > 0 {
            cache_log!("cache: pruned {} orphaned section files", pruned);
        }
    }
    library.set_book_index(total_pages, book_partial, sections_slice);
    let hit = matches!(
        files::load_v2_section_by_global_page(
            root,
            cache_key,
            source_identity,
            requested_global_page.min(total_pages.saturating_sub(1)),
            library,
        ),
        CacheLoadResult::Hit { .. }
    );
    if !hit {
        return PublishReport {
            outcome: BookPublishOutcome::SectionReadFailed,
            cover: None,
        };
    }
    layout::rebuild_toc_page_targets(library);
    refresh_chapter_tracking(
        root,
        cache_key,
        source_identity,
        requested_global_page.min(total_pages.saturating_sub(1)),
        library,
    );
    // Adopting the cover belongs here, on every successful publish, including
    // the provisional one a progressive open makes.
    let cover = files::load_v2_cover_cache(root, cache_key, library);
    PublishReport {
        outcome: BookPublishOutcome::Ready,
        cover: Some(cover),
    }
}

/// Publish the provisional book a suspended first open has built: the reader
/// starts here, on an index that only spans the sections written so far.
///
/// `partial` is forced true, which is the honest description of what is on the
/// card — and what makes an interrupted build safe, because a later open of a
/// page beyond this frontier misses the index and rebuilds rather than running
/// off the end of it.
#[expect(clippy::too_many_arguments)] // The publish tail's borrow set, same shape as `publish_book_cache` itself
pub fn publish_first_open<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    requested_global_page: u32,
    library: &mut ReaderStore,
    sections_slice: &[BookV2SectionRecord],
    total_pages: u32,
    next_spine: u16,
) -> Result<(), PublishError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let published = publish_book_cache(
        root,
        cache_key,
        source_identity,
        requested_global_page,
        library,
        sections_slice,
        total_pages,
        true,
        // The cursor is the whole reason this index is safe to publish: it is
        // what a later open reads to tell "still being built" from "this is
        // all there is", so an abandoned build rebuilds instead of capping the
        // book at whatever it reached.
        next_spine,
    );
    match published.outcome {
        BookPublishOutcome::Ready => Ok(()),
        // Both failures are the completed build's, verbatim: a provisional
        // publish writes the same BOOK.BIN through the same path, so a
        // truncated one is exactly as unusable and gets the same cleanup.
        BookPublishOutcome::SectionReadFailed => Err(PublishError::SectionRead),
        BookPublishOutcome::IndexWriteFailed => {
            let _ = files::empty_cache_dir(root, cache_key.key);
            Err(PublishError::IndexWrite)
        }
    }
}

/// Put the page the reader is looking at back into the single text arena a
/// build step borrowed, and report whether it landed.
///
/// Every exit from a background step runs this, successes and failures alike.
/// The step drives `LibraryBlockSink` through the same arena the reading view
/// renders from, so between the sink's last write and this call the arena holds
/// the *builder's* last section, not the reader's page. Nothing has awaited in
/// that window, so no render has seen it — but the loop this returns to renders,
/// and a render does not necessarily load a section first.
///
/// Clamped against the store, not against any freshly built total: a step whose
/// index write was refused leaves the store on its previous, shorter index, and
/// asking for a page past it would miss for a second reason.
///
/// A `false` marks the window unusable rather than leaving a half-loaded one
/// that `covers_global_page` would accept. Every page turn issues an extend
/// (`app_core::storage_loop`'s `storage_command_for_transition`), and a window
/// that admits nothing is what makes that extend reload rather than answer from
/// RAM.
fn restore_reader_page<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    reader_page: u32,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let ceiling = library.advertised_page_count().saturating_sub(1);
    let restored = matches!(
        files::load_v2_section_by_global_page(
            root,
            cache_key,
            source_identity,
            reader_page.min(ceiling),
            library,
        ),
        CacheLoadResult::Hit { .. }
    );
    if !restored {
        library.set_section_partial(true);
        cache_log!("epub: reader page {} could not be restored", reader_page);
    }
    restored
}

/// The tail of the step that finishes a background walk.
///
/// Separate from the open path's tail because the two failures mean different
/// things once the book is already open. On an open, a refused index write
/// leaves debris nobody is using and clearing it is the tidy answer. Here the
/// reader is *reading* those section files, and deleting them would take their
/// book away mid-page to save the next open a rebuild it can perfectly well
/// discover for itself — the truncated `BOOK.BIN` is that signal, and CONT.BIN
/// (complete by now) makes the rebuild a fast replay rather than a full one.
pub fn finish_background_walk<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    reader_page: u32,
    published: BookPublishOutcome,
    library: &mut ReaderStore,
) -> Result<(), PublishError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // A clean publish already loaded the reader's page on its way through.
    if published == BookPublishOutcome::Ready {
        return Ok(());
    }
    let restored = restore_reader_page(root, cache_key, source_identity, reader_page, library);
    match published {
        // Handled by the early return above; named rather than left to a
        // wildcard so a new outcome variant is a compile error here instead of
        // silently falling into one of the failure paths below.
        BookPublishOutcome::Ready => Ok(()),
        // The index never landed, so the book is not finished however the page
        // read went. Report it and let the next open rebuild.
        BookPublishOutcome::IndexWriteFailed => {
            cache_log!("epub: final background publish index write failed");
            Err(PublishError::IndexWrite)
        }
        // A section read that failed once and succeeded on the retry. The build
        // itself is *done*: `publish_book_cache` had already written the
        // complete index and adopted it into the store before that read, so the
        // only casualty was the refresh it does afterwards. Reporting this as
        // abandoned would throw away a finished book — no continuation left to
        // announce the final page count, and a reader stuck against the old
        // frontier until they reopen. Finish the job instead.
        BookPublishOutcome::SectionReadFailed if restored => {
            layout::rebuild_toc_page_targets(library);
            let page = reader_page.min(library.advertised_page_count().saturating_sub(1));
            refresh_chapter_tracking(root, cache_key, source_identity, page, library);
            cache_log!(
                "epub: final background publish section read recovered on retry (pages={})",
                library.advertised_page_count()
            );
            Ok(())
        }
        BookPublishOutcome::SectionReadFailed => {
            cache_log!("epub: final background publish section read failed");
            Err(PublishError::SectionRead)
        }
    }
}

/// Adopt what a background step just built, then put the reader's page back in
/// the text arena the step borrowed.
///
/// Returns the section count now standing in the on-disk index, which the next
/// step carries forward to decide when the file is due another rewrite.
///
/// The resident index is adopted on *every* step; the file is not. The reader's
/// page turns and `background_announce`'s frontier both read the resident copy,
/// so it has to track the walk exactly. The file is only ever read by the next
/// open, which rebuilds anyway while `resume_spine` is set — so rewriting it per
/// slice would spend quadratic index traffic on a frontier nobody reads. See
/// [`INDEX_PUBLISH_SECTIONS`].
///
/// An `Err` is how the caller learns the difference between a walk that ended
/// and a walk that broke. Both stop, but only the first may be announced: an
/// announcement forces a full repaint, and repainting over an arena this
/// function could not restore would draw the builder's last section under a
/// reader who never left their page.
#[expect(clippy::too_many_arguments)] // Same borrow set as the publish tail it stands beside
pub fn extend_background_index<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    reader_page: u32,
    next_spine: u16,
    published_sections: u16,
    library: &mut ReaderStore,
    sections_slice: &[BookV2SectionRecord],
    total_pages: u32,
) -> Result<u16, PublishError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let grown = sections_slice
        .len()
        .saturating_sub(published_sections as usize);
    let mut published_now = published_sections;
    // The cursor rides in the index it describes, so a build that never comes
    // back is recognisable as one on the next open instead of masquerading as
    // a short book.
    let wrote = if grown >= INDEX_PUBLISH_SECTIONS {
        let ok = files::write_v2_book_index(
            root,
            cache_key,
            source_identity,
            total_pages,
            sections_slice,
            library,
            true,
            next_spine,
        );
        if ok {
            published_now = sections_slice.len().min(u16::MAX as usize) as u16;
        } else {
            // Deliberately not `empty_cache_dir`, unlike the publish tails: the
            // reader is reading out of this cache right now, and deleting it
            // would take the section files out from under them. The truncated
            // BOOK.BIN only costs the *next* open a rebuild, which it detects
            // for itself.
            cache_log!("epub: build continue index write failed, stopping background build");
        }
        ok
    } else {
        true
    };

    // Always, write or no write: this is the index the reader turns pages
    // against, and the one the frontier announcement is measured from.
    library.set_book_index(total_pages, true, sections_slice);
    // Re-derive the chapter start pages over the grown index: a pure recompute
    // against the resident TOC, no card reads. Without it the Chapters overview
    // would show zeroes for every chapter past the frontier until the walk
    // finished. The *current* chapter is deliberately not refreshed — that reads
    // TOC.BIN, and the reader has not moved.
    layout::rebuild_toc_page_targets(library);

    let restored = restore_reader_page(root, cache_key, source_identity, reader_page, library);
    // Restore first, then report: the reader's page matters more than which
    // failure gets named, and the index write is the earlier one.
    if !wrote {
        return Err(PublishError::IndexWrite);
    }
    if !restored {
        return Err(PublishError::SectionRead);
    }
    Ok(published_now)
}

/// Keep the resident current-chapter index and title pointed at the page just
/// loaded. The per-section chapter marks are read from TOC.BIN once per book
/// (or after a repaginating settings change); the title is a single 48-byte
/// record re-read only when the chapter actually changes.
pub fn refresh_chapter_tracking<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    global_page: u32,
    library: &mut ReaderStore,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let config = layout::reader_layout_config(library.type_settings(), library.portrait());
    let token = (
        source_identity.0,
        source_identity.1,
        config,
        library.custom_font_identity(),
    );
    if !library.chapter_start_ready || library.chapter_start_token != token {
        if files::load_v2_toc_chapter_map(root, cache_key, source_identity, library) {
            library.chapter_start_token = token;
        } else {
            return;
        }
    }
    let current = library.current_chapter_for_page(global_page);
    let needs_refresh =
        current != library.current_chapter() || library.current_chapter_title().is_empty();
    if needs_refresh
        && !files::read_v2_toc_chapter_title(root, cache_key, source_identity, current, library)
    {
        // No title on the card (or a short read): still advance the index so
        // the cursor tracks; the colophon falls back to a numeral.
        library.set_current_chapter(current, "", source_identity);
    }
}
