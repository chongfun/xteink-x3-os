use crate::display_flush::Epd;
use crate::sd_session::{self, SdSessionError};
use display::font::{fixed_ceil, fixed_round, FontFamily, FontStyle, TypeSettings};
use embassy_time::Instant;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use esp_hal::gpio::Output;
use heapless::String;
use proto::book::BookId;
use proto::cache::{BookV2SectionRecord, CONTENT_HEADER_BYTES};
use proto::epub::{
    decode_html_entity, parse_opf, strip_fragment, CssRules, Epub3NavStreamParser, EpubTocSink,
    EpubZipOps, NcxStreamParser, ReadAt, StreamingXmlTokenizer, TocError, XhtmlBlockSink,
    XhtmlBlockStreamParser, XhtmlError, ZipInflateScratch, ZipStream, MAX_ENTRY_NAME_BYTES,
};
use proto::library_path::{BookRoot, LibraryPath};
use proto::nvm::AppStateRecord;
use proto::text::{TextAlign, TextRole};
use reader_cache::files;
use reader_cache::files::{BookIndexLoadResult, CacheLoadResult};
use reader_cache::layout;
use reader_cache::publish;
use reader_cache::store::{
    BookLoadStatus, ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS,
    MAX_READER_BLOCK_TEXT,
};
use reader_cache::{
    READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH, READER_HEADER_SCRATCH, READER_OPF_SCRATCH,
    READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use ui::reading::StyledInkCursor;

/// Per-call clamp on `read_at`. The FAT layer transfers single 512-byte
/// blocks either way, so this only bounds how much one embedded-sdmmc call
/// copies; matching the compressed scratch keeps an 8 KB inflate fetch to
/// one seek + one read call instead of four.
const EPUB_READ_AT_CHUNK_BYTES: usize = 8192;
const EPUB_OPEN_READ_OP_LIMIT: u32 = 65_536;
const EPUB_OPEN_READ_BYTE_LIMIT: u32 = 64 * 1024 * 1024;

/// How long one background build step may hold the display task before it
/// hands back to renders and page turns.
///
/// The budget is only checked at spine boundaries, so a step always runs at
/// least one spine item and a long chapter overshoots — on the measured 11.7 MB
/// book (100 sections in ~64 s) one item is already ~640 ms, so this value
/// mostly decides whether short items get batched. Lowering it does not make
/// page turns arbitrarily snappy; it just makes each step re-pay the card
/// session, zip index, and OPF parse more often.
const BACKGROUND_SLICE_MS: u64 = 400;

/// Everything a suspended progressive build needs to pick the spine walk back
/// up, minus the section records themselves — those stay in
/// [`ReaderCacheScratch::book_sections`], which is exactly why this lives in
/// the same struct.
///
/// Keeping the two together is the whole safety argument. The records are the
/// build's real state and any other build overwrites them, so a resume stored
/// anywhere else could outlive the records it describes and splice one book's
/// tail onto another's index. Here that is not expressible, because exactly one
/// invariant is maintained: **this field is `Some` only while `book_sections`
/// holds that walk's records.** Every route that rewrites them clears it first
/// — see the fast-path split in `build_or_load_book_cache_from_root`, which is
/// also the only route that leaves an existing walk standing.
///
/// RAM: 24 bytes inside the `EPUB_SCRATCH` static (`.bss`), not on any stack.
///
/// `PartialEq` is load-bearing, not derived for convenience: comparing the
/// value before and after an open is how [`build_or_load_book_cache`] tells a
/// walk that survived from one that was replaced.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct BookBuildResume {
    /// Catalog row the build is walking. Re-resolved (and re-checked against
    /// `source_identity`) at every step: a rescan between steps can move a
    /// different book under the same row.
    index: u16,
    source_identity: (u32, u32),
    /// First spine item the next step must build. Always a boundary — the
    /// walk cannot suspend inside an item.
    next_spine: u16,
    section_count: u16,
    total_pages: u32,
    book_partial: bool,
    /// Decided once, from the TOC the first step parsed. A continuation never
    /// re-reads the TOC, so it cannot re-derive this.
    generate_toc_from_headings: bool,
    /// Whether CONT.BIN is still being captured, and how many spine groups it
    /// holds. Once false it stays false for the rest of the build.
    content_ok: bool,
    content_spine_count: u16,
    /// Sections already written into the on-disk index. The walk's own frontier
    /// runs ahead of this between publishes; see `publish::INDEX_PUBLISH_SECTIONS`.
    published_sections: u16,
    /// The layout this walk is paginating for.
    ///
    /// Part of what makes a resume ours, now that a book can hold a stored
    /// pagination per layout. Without it, a walk suspended while building one
    /// layout would be resumed by a step whose writers derive another, and the
    /// second copy would be overwritten a section at a time by the first.
    layout: u8,
}

impl BookBuildResume {
    /// Whether this suspended walk is the one building the book that is *now*
    /// at catalog row `index`.
    ///
    /// The row alone is not the book. A rescan reorders rows, so a walk
    /// suspended on row 7 can find a different title there next time — and the
    /// row number matching is exactly what would license that title's own
    /// half-built index as "someone is still working on it". Nothing would be:
    /// the real walk belongs to a book that has moved, and it abandons itself
    /// the moment it checks. The reader is then left on a frontier no builder
    /// will ever raise, which is the trap `partial_index_is_usable` exists to
    /// prevent.
    ///
    /// Every predicate that decides whether a resume is still ours goes through
    /// here, so the fast path and the outcome check cannot drift apart.
    fn belongs_to(&self, index: usize, source_identity: (u32, u32), layout: u8) -> bool {
        self.index as usize == index
            && self.source_identity == source_identity
            && self.layout == layout
    }
}

pub(crate) struct ReaderCacheScratch<'a> {
    tail: &'a mut [u8; READER_TAIL_SCRATCH],
    header: &'a mut [u8; READER_HEADER_SCRATCH],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    container: &'a mut [u8; READER_CONTAINER_SCRATCH],
    opf: &'a mut [u8; READER_OPF_SCRATCH],
    xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
    book_sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    /// Borrowed, not embedded, and that is load-bearing. `ZipInflateScratch` is
    /// now ~32 KiB (primarily the 32 KB LZ77 window), while its ~10.5 KiB
    /// `DecompressorOxide` decoder is stored in a separate static cell.
    /// Historically, when the scratch held the decoder by value, the combined
    /// struct was 43,280 bytes. Embedding it by value made `ReaderCacheScratch`
    /// 43,340 bytes, and `ZipInflateScratch::new()` returned by value — so
    /// initialising the static depended entirely on LLVM forwarding the `sret`
    /// slot into `.bss` instead of building a copy on the stack. It did, until
    /// a 28-byte field pushed the struct past whatever threshold that decision
    /// hangs on; the copy reappeared, `ensure_epub_scratch` allocated 53,744 bytes
    /// on a 42,136-byte stack, and the overflow wrote through `.bss` into esp-hal's
    /// clock singleton. The next `Clocks::get()` unwrapped a `None`. Behind a
    /// reference the window can never be a stack temporary of this struct at all,
    /// so the cliff is gone rather than merely uphill.
    zip_inflate: &'a mut ZipInflateScratch,
    /// The suspended progressive build that owns `book_sections`, if any.
    ///
    /// RAM (measured): 28 bytes, and the `Option` is free — `BookBuildResume`
    /// is 28 bytes on its own, so the three `bool`s give the discriminant a
    /// niche to live in rather than costing a tag word. This struct is 64 bytes
    /// now that the inflate window is borrowed, and lives in `EPUB_SCRATCH`, a
    /// `StaticCell` in `.bss` — so nothing is charged to the ~43 KB stack the
    /// build's deep frames run in, which is the budget that is actually tight.
    ///
    /// Kept here rather than beside the loop's scheduling handle because the
    /// invariant is a colocation one: this is `Some` only while
    /// `book_sections` holds that walk's records, and a field in the same
    /// struct cannot drift from them the way a parallel copy in the display
    /// task would.
    resume: Option<BookBuildResume>,
}

struct TocScratch<'a> {
    header: &'a mut [u8; 46],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    zip_inflate: &'a mut ZipInflateScratch,
}

struct LibraryTocSink<'a, 'p> {
    library: &'a mut ReaderStore,
    package: &'p proto::epub::EpubPackage<'p>,
    /// The full chapter list streams into this scratch buffer as fixed-size
    /// records, then gets written to TOC.BIN. Holds up to
    /// `buf.len() / TOC_CHAPTER_RECORD_BYTES` chapters.
    toc_buf: &'a mut [u8],
    record_count: usize,
    resident_full: bool,
}

impl EpubTocSink for LibraryTocSink<'_, '_> {
    fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
        let spine_index = self
            .package
            .spine
            .iter()
            .position(|item| href_matches_spine(href, item.href.of(self.package.opf_text)))
            .map(|index| index as i16)
            .unwrap_or(-1);
        // Stream the full chapter list (uncapped up to the scratch buffer)
        // into fixed-size records for TOC.BIN.
        let offset = self.record_count * proto::cache::TOC_CHAPTER_RECORD_BYTES;
        if offset + proto::cache::TOC_CHAPTER_RECORD_BYTES <= self.toc_buf.len() {
            let record = proto::cache::toc_chapter_record(title, level, spine_index);
            if proto::cache::encode_toc_chapter(
                &record,
                &mut self.toc_buf[offset..offset + proto::cache::TOC_CHAPTER_RECORD_BYTES],
            )
            .is_ok()
            {
                self.record_count += 1;
            }
        }
        // The resident copy still feeds the current (capped) overview until
        // stage 2 switches it to the on-disk list.
        if !self.resident_full && !self.library.push_toc_record(title, level, spine_index) {
            self.resident_full = true;
        }
        Ok(())
    }
}

impl<'a> ReaderCacheScratch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tail: &'a mut [u8; READER_TAIL_SCRATCH],
        header: &'a mut [u8; READER_HEADER_SCRATCH],
        name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
        compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
        container: &'a mut [u8; READER_CONTAINER_SCRATCH],
        opf: &'a mut [u8; READER_OPF_SCRATCH],
        xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
        book_sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
        zip_inflate: &'a mut ZipInflateScratch,
    ) -> Self {
        Self {
            tail,
            header,
            name,
            compressed,
            container,
            opf,
            xhtml,
            book_sections,
            zip_inflate,
            resume: None,
        }
    }
}

/// Tears the built scratch down into the raw regions the sync session
/// loans to the radio. One-way: the regions alias the scratch's borrowed
/// arrays, the separate inflate decoder static, and its own struct storage
/// (the inflate window is the bulk of it), so the scratch must never be used
/// as a scratch again — only the session-ending software reset brings the
/// reader pipeline back.
#[allow(unsafe_code)]
pub(crate) fn dismantle_scratch(
    scratch: &'static mut ReaderCacheScratch<'static>,
) -> crate::sync_mem::SyncLoan {
    use crate::sync_mem::{RawRegion, SyncLoan};

    // Raw field pointers first; they chain provenance through the field
    // borrows into the separate backing statics, not into the struct.
    let xhtml = RawRegion {
        ptr: scratch.xhtml.as_mut_ptr(),
        len: READER_XHTML_SCRATCH,
    };
    let opf_ptr = scratch.opf.as_mut_ptr();
    let compressed_ptr = scratch.compressed.as_mut_ptr();
    let container_ptr = scratch.container.as_mut_ptr();
    let tail_ptr = scratch.tail.as_mut_ptr();

    // The zip inflate static allocation becomes the wifi heap_a region.
    let struct_region = RawRegion {
        ptr: (scratch.zip_inflate as *mut ZipInflateScratch).cast::<u8>(),
        len: core::mem::size_of::<ZipInflateScratch>(),
    };

    // The separate zip decompressor static becomes the wifi heap_c region.
    let decoder_region = match scratch.zip_inflate.take_decompressor() {
        Some(decompressor) => RawRegion {
            ptr: (decompressor as *mut proto::epub::DecompressorOxide).cast::<u8>(),
            len: core::mem::size_of::<proto::epub::DecompressorOxide>(),
        },
        None => RawRegion {
            ptr: core::ptr::null_mut(),
            len: 0,
        },
    };

    // SAFETY: each pointer addresses a distinct 'static allocation whose
    // only other path is the scratch struct this function retires.
    unsafe {
        SyncLoan {
            heap_a: struct_region,
            heap_b: xhtml,
            heap_c: decoder_region,
            tcp_rx: core::slice::from_raw_parts_mut(opf_ptr, READER_OPF_SCRATCH),
            tcp_tx: core::slice::from_raw_parts_mut(compressed_ptr, READER_COMPRESSED_SCRATCH),
            http_a: core::slice::from_raw_parts_mut(container_ptr, READER_CONTAINER_SCRATCH),
            http_b: core::slice::from_raw_parts_mut(tail_ptr, READER_TAIL_SCRATCH),
            wifi: None,
            wifi_hint: None,
            catalog_len: 0,
        }
    }
}

/// What an open or extend left for the caller to schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BookBuildOutcome {
    /// Nothing to schedule: served whole from cache, built to the end, or
    /// failed.
    Settled,
    /// A progressive build published this book early and owes background
    /// steps. Its index spans only the pages built so far.
    Started,
    /// A background build for this book was already running and still is —
    /// this call answered from the cache without disturbing it. The caller
    /// keeps the handle it already has.
    Carried,
}

/// Kept out of line: the storage dispatcher's frame must stay small, and the
/// EPUB open path below already runs close to the 30 KB stack region.
///
/// A [`BookBuildOutcome::Started`] or `Carried` means the caller owes this book
/// background steps through [`continue_book_build`], until that reports
/// [`BackgroundStep::Finished`].
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_or_load_book_cache(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    requested_chapter: u16,
    target_pages: usize,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> BookBuildOutcome {
    // The suspended walk as it stood on entry. Only the fast path can leave one
    // untouched — every other route clears it before its first writer runs — so
    // finding the identical value again below is exactly the statement "the
    // cache answered and the walk still owns its records".
    let entry_resume = scratch.resume;
    esp_println::println!(
        "epub: cache open index {} chapter {} target {}",
        index,
        requested_chapter,
        target_pages
    );
    library.begin_book_load();

    let Some(entry) = library.catalog_entry(index) else {
        set_preview_error(library, "BAD INDEX");
        library.set_reader_status(BookLoadStatus::Error);
        scratch.resume = None;
        return BookBuildOutcome::Settled;
    };
    // Read before the load: it is the identity a surviving walk must match, and
    // the load below rewrites the store around it.
    let source_identity = (entry.source_hash, entry.byte_size);

    let status = sd_session::with_root(epd, sd_cs, |root| {
        build_or_load_book_cache_from_root(
            root,
            library,
            index,
            requested_chapter,
            target_pages,
            scratch,
            font_metrics,
        )
    })
    .unwrap_or_else(|err| {
        esp_println::println!("epub: session failed: {:?}", err);
        set_preview_error(library, session_error_label(err));
        BookLoadStatus::Error
    });

    library.finish_book_load(index, requested_chapter, status);
    // A walk only survives if it is still this book's and the open ended Ready.
    // An open of a *different* book reaches here with the resume intact — its
    // fast path never touched the records — and continuing that walk would
    // append one book's sections to another book's live index.
    let live = matches!(status, BookLoadStatus::Ready)
        && scratch
            .resume
            .is_some_and(|state| state.belongs_to(index, source_identity, library.layout_key()));
    if !live {
        scratch.resume = None;
        return BookBuildOutcome::Settled;
    }
    if scratch.resume == entry_resume {
        BookBuildOutcome::Carried
    } else {
        BookBuildOutcome::Started
    }
}

/// Drop any suspended walk the scratch is holding.
///
/// For callers that invalidate a book's cache without going through an open —
/// a cache clear, say. Without it the section records stay described by a
/// resume whose files may be gone, and the next open of that row would report
/// [`BookBuildOutcome::Carried`] and schedule steps over a cache that no longer
/// exists.
pub(crate) fn clear_build_resume(scratch: &mut ReaderCacheScratch<'_>) {
    scratch.resume = None;
}

/// What one background build step left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundStep {
    /// More spine to walk; call again.
    Continued,
    /// The walk reached the end of the book. The store's totals are final and
    /// the reader's page is resident, so this is the one outcome worth
    /// announcing.
    Finished,
    /// The walk stopped without finishing, but the store came through it whole:
    /// the reader's page is resident and the resident index is the one they turn
    /// pages against. A step reaches here by growing the book and *then* failing
    /// — a refused `BOOK.BIN` write, most often — which leaves pages that are
    /// built, on the card, and reachable, behind a page count the app has not
    /// been told about. Announcing is therefore safe, and at the frontier it is
    /// necessary: see `app_core::storage_loop::stopped_announce`.
    Stopped,
    /// The walk stopped without finishing and the store did not come through it:
    /// the arena may hold whatever the builder touched last rather than the page
    /// on screen. Nothing may repaint on it at any page count. The card keeps a
    /// valid, shorter index, and its `resume_spine` marks it as a build that
    /// never came back, so the next open rebuilds rather than trusting it.
    Abandoned,
    /// The step never began — the card would not open a session, a directory,
    /// or the file — so not one record was touched and the walk is intact and
    /// re-armed. Nothing was built, so there is nothing to announce, which is
    /// exactly why the walk has to be kept: a reader already at the frontier
    /// has no page turn that would provoke a rebuild. The caller bounds how
    /// many times it comes back (`app_core::storage_loop::retry_unstarted_step`).
    Retry,
}

/// What one attempt at a step did, as distinct from how it ended.
enum StepAttempt {
    /// The walk never began. `book_sections` is untouched, so the resume that
    /// describes it is still true to the byte and can simply go back.
    NeverBegan(ReaderCacheError),
    /// The walk ran. Whatever it left behind, the resume the build itself set
    /// or cleared is the authority now.
    Ran(Result<(), ReaderCacheError>),
}

/// Which of the two endings a broken step earned.
///
/// The question is not which error was raised but whether the store is still
/// the reader's. [`restore_reader_page`] marks the window partial when it cannot
/// put the reader's page back, and `covers_global_page` is the very predicate a
/// page turn consults — so asking the store is both the accurate answer and the
/// same one the rest of the system will act on.
fn step_ending(library: &ReaderStore, index: usize, current_page: u32) -> BackgroundStep {
    if library.covers_global_page(index, current_page) {
        BackgroundStep::Stopped
    } else {
        BackgroundStep::Abandoned
    }
}

/// One background step of a progressive build: re-open the EPUB, skip to the
/// suspended spine cursor, and walk a slice.
///
/// Deliberately silent about the reader's status. The book has been `Ready`
/// since the first step published it, and a step that fails must leave it that
/// way — the card still holds a valid, if short, index, and the reader is
/// inside it. `current_page` is where the reader is now, not where the open
/// asked to go: each step ends by putting that page's section back in the one
/// text arena the build borrows.
#[inline(never)]
pub(crate) fn continue_book_build(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    current_page: u32,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> BackgroundStep {
    // Taken, not borrowed: every path out of here either stores a fresh resume
    // or means the build is over, so the old one must not survive by default.
    let Some(resume) = scratch.resume.take() else {
        // Nothing was suspended, so nothing ended here either — and with no
        // step run, there is nothing new to announce.
        return BackgroundStep::Abandoned;
    };
    let Some(entry) = library.catalog_entry(resume.index as usize) else {
        esp_println::println!("epub: build continue lost catalog entry, dropping");
        return BackgroundStep::Abandoned;
    };
    if !resume.belongs_to(
        resume.index as usize,
        (entry.source_hash, entry.byte_size),
        library.layout_key(),
    ) {
        // A rescan moved a different book under this row. Building its spine
        // into the previous book's section table would corrupt both.
        esp_println::println!("epub: build continue entry changed, dropping");
        return BackgroundStep::Abandoned;
    }
    let mut display_name = String::<64>::new();
    let _ = display_name.push_str(&entry.display_name);
    let Some((at, path)) = book_locator(library, resume.index as usize) else {
        esp_println::println!("epub: build continue lost locator, dropping");
        return BackgroundStep::Abandoned;
    };

    let step = sd_session::with_root(epd, sd_cs, |root| {
        // The file borrows the directory the walk opened, so the step runs
        // inside the walk rather than carrying a handle out of it.
        let ran = upload_store::library::with_book_at(root, at, &path, |dir, alias| {
            let file = dir.open_file_in_dir(alias, Mode::ReadOnly).ok()?;
            Some(build_or_load_epub_cache_from_file(
                file,
                root,
                &display_name,
                at,
                &path,
                resume.index,
                0,
                // The step's "requested page" is where the reader is now: it
                // is the page the step must leave resident, and the page a
                // finishing step republishes around.
                current_page as usize,
                Some(resume),
                library,
                scratch,
                font_metrics,
            ))
        });
        match ran.ok().flatten().flatten() {
            Some(outcome) => StepAttempt::Ran(outcome),
            None => {
                esp_println::println!("epub: build continue open failed");
                StepAttempt::NeverBegan(ReaderCacheError::MissingSpine)
            }
        }
    });
    match step {
        Ok(StepAttempt::Ran(Ok(()))) => {
            if scratch.resume.is_some() {
                BackgroundStep::Continued
            } else {
                BackgroundStep::Finished
            }
        }
        // The step broke rather than finished. The card keeps a shorter but
        // valid index whose `resume_spine` says a build walked away from it, so
        // the next open rebuilds. Whether the *reader* can be told anything now
        // is a separate question, and only the store can answer it.
        Ok(StepAttempt::Ran(Err(err))) => {
            esp_println::println!("epub: build continue failed: {:?}", err);
            scratch.resume = None;
            step_ending(library, resume.index as usize, current_page)
        }
        // Nothing ran, so nothing is lost by running it again — but the resume
        // was taken on the way in and has to go back, or the walk it describes
        // dies of the bookkeeping rather than the fault.
        Ok(StepAttempt::NeverBegan(err)) => {
            esp_println::println!("epub: build continue never began: {:?}", err);
            scratch.resume = Some(resume);
            BackgroundStep::Retry
        }
        Err(err) => {
            esp_println::println!("epub: build continue session failed: {:?}", err);
            scratch.resume = Some(resume);
            BackgroundStep::Retry
        }
    }
}

#[inline(never)]
pub(crate) fn build_or_load_book_cache_from_root<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &mut ReaderStore,
    index: usize,
    requested_chapter: u16,
    target_pages: usize,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> BookLoadStatus
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    esp_println::println!("epub: card init begin");
    esp_println::println!("epub: open root");
    let mut display_name = String::<64>::new();
    let Some(entry) = library.catalog_entry(index) else {
        return BookLoadStatus::Error;
    };
    let source_identity = (entry.source_hash, entry.byte_size);
    // The row a suspended build re-resolves itself from between steps. The
    // catalog is bounded well below u16, so the clamp is a formality.
    let catalog_index = index.min(u16::MAX as usize) as u16;
    let _ = display_name.push_str(&entry.display_name);
    let located = book_locator(library, index);
    esp_println::println!(
        "epub: catalog entry display='{}' at='{}'",
        display_name,
        located.as_ref().map_or("", |(_, path)| path.as_str()),
    );
    let cache_key = proto::cache::cache_key_from(source_identity.0);
    library.set_cache_key(cache_key.as_str());
    // The cache is reachable only with a provable owner: the key and every
    // artifact identity are 32-bit hashes a twin can share, so a row whose
    // locator is not resident gets no cache access, only the build path.
    let owner = located.as_ref().map(|(at, path)| proto::cache::CacheOwner {
        key: cache_key.as_str(),
        root: *at,
        locator: path.as_str(),
    });
    esp_println::println!("epub: stage ResolveCatalogEntry key={}", cache_key.as_str());
    esp_println::println!(
        "epub: stage TryV2BookIndexFast page={}",
        target_pages as u32
    );
    // The fast path is the one route through here that leaves the scratch's
    // section records alone — it reads the index into a stack array and the
    // section into the text arena. That is what lets an ordinary page turn
    // across a section boundary, which arrives here as an extend, pass over a
    // suspended background build without ending it.
    //
    // It is also why the fast path has to be told whether that build exists: an
    // index published mid-walk is only readable while the walk is coming back
    // for the rest — and "this walk, for this book", not merely for this row.
    let walk_is_live = scratch
        .resume
        .is_some_and(|state| state.belongs_to(index, source_identity, library.layout_key()));
    let fast_hit = owner.as_ref().is_some_and(|owner| {
        try_load_v2_book_cache(
            root,
            owner,
            source_identity,
            target_pages as u32,
            library,
            Instant::now(),
            "fast",
            walk_is_live,
        )
    });
    if !fast_hit {
        // Every remaining route rewrites those records, so whatever build owned
        // them is over. Cleared before the first writer runs rather than after,
        // so no failure path can leave a resume describing records that have
        // already been overwritten.
        scratch.resume = None;
    }
    // The one moment a book gains a layout: the fast path missed, so this
    // open will write one that was not there. Anything over the bound goes
    // now, before the writers run.
    if !fast_hit {
        if let Some(owner) = owner.as_ref() {
            if !files::evict_layouts_for(root, owner, library.layout_key()) {
                esp_println::println!("epub: a stored layout would not delete; it stays counted");
            }
        }
    }
    // Before the replay: the same book at another layout, whose sections are
    // still here.
    let reindexed = !fast_hit
        && owner.as_ref().is_some_and(|owner| {
            try_reindex_layout(
                root,
                owner,
                source_identity,
                target_pages as u32,
                library,
                scratch,
            )
        });
    let replayed = !fast_hit
        && !reindexed
        && owner.as_ref().is_some_and(|owner| {
            try_replay_content_cache(
                root,
                owner,
                source_identity,
                target_pages as u32,
                library,
                scratch,
                font_metrics,
            )
        });
    let status = if fast_hit || reindexed || replayed {
        BookLoadStatus::Ready
    } else {
        // The file borrows the directory the walk opened, so the build runs
        // inside the walk rather than carrying a handle out of it.
        let load_result = located.as_ref().and_then(|(at, path)| {
            upload_store::library::with_book_at(root, *at, path, |dir, alias| {
                let file = dir.open_file_in_dir(alias, Mode::ReadOnly).ok()?;
                Some(build_or_load_epub_cache_from_file(
                    file,
                    root,
                    &display_name,
                    *at,
                    path,
                    catalog_index,
                    requested_chapter,
                    target_pages,
                    None,
                    library,
                    scratch,
                    font_metrics,
                ))
            })
            .ok()
            .flatten()
            .flatten()
        });
        // A directory that would not answer is not the same as a book that is
        // not there, but neither can be read, and the reader is told the same
        // thing either way.
        if load_result.is_none() {
            esp_println::println!("epub: open failed");
            set_preview_error(library, "FILE");
        }
        status_for_load_result(load_result, library)
    };
    if matches!(status, BookLoadStatus::Ready) && !library.title.is_empty() {
        // Persist the just-learned EPUB title into the catalog record, in
        // this same session, so future Library windows and boots label the
        // book without probing its cache. Read-compare first; a reopen with
        // an unchanged title writes nothing.
        let _ = crate::library_sd::update_catalog_title(
            root,
            index,
            source_identity,
            library.title.as_str(),
        );
    }
    status
}

/// Where row `index` is on the card, ready to open: the root its locator is
/// relative to, and the locator.
///
/// Only the active book has a resident locator, and every open stages its row
/// as active first, so `None` means that staging did not happen or the record
/// held nothing openable.
/// A copy whose bytes the card has not recorded, read a slice at a time.
///
/// A scan adopts a book without reading it, so for everything a computer
/// put on the card the claim beside its reading place is the only thing
/// that will say what its bytes were, and the only thing that lets a later
/// scan find the book again after a move. The whole stream has to be read
/// for the claim to say anything, and a book is megabytes: on this card
/// around 550 kB a second, so a large one is the better part of a minute.
/// That cannot stand between the reader and their first page, so it rides
/// the same background slices the spine walk uses.
///
/// Nothing depends on it finishing. A book closed halfway through is a book
/// whose bytes are recorded on some later open, and one that is never
/// opened long enough is one a move cannot find again, which is the state
/// every book was in before.
pub(crate) struct SourceEvidenceJob {
    place: EvidencePlace,
    offset: u32,
    started: Instant,
    hasher: proto::source::SourceHasher,
}

/// Which copy a reading is about: the place itself, exactly.
///
/// Not the cache directory it is filed under, whose name is 28 bits of a
/// hash of this and which two books can share. Deciding by that name would
/// leave a book unread because another one's reading had settled under it,
/// and would let a reader moving between two such books keep the reading
/// they moved away from. Not a row number either, which a rescan
/// renumbers, and not a `BookId`, which a copy the scan has left in
/// question does not have yet.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EvidencePlace {
    at: BookRoot,
    locator: String<{ proto::library_path::MAX_PATH_BYTES }>,
    len: u32,
}

impl EvidencePlace {
    /// The cache directory this place's claim lives in.
    fn key(&self) -> String<{ proto::cache::CACHE_KEY_BYTES }> {
        proto::cache::cache_key_from(proto::cache::source_hash_at(
            self.at,
            self.locator.as_str(),
            self.len,
        ))
    }
}

/// What one slice of that reading did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EvidenceStep {
    /// More of the book to read; call again.
    Continued,
    /// The stream was read whole and the claim now says what it held, or
    /// the claim already said and nothing needed reading.
    Finished,
    /// The card would not give the book up, or gave up less of it than it
    /// said it held. Nothing is recorded from a partial read, so the job is
    /// dropped whole. The caller decides when to ask again: the display task
    /// leaves this copy alone until the reader has opened another book, or
    /// until the next session, rather than reading at a card that just
    /// refused on every page turn.
    Abandoned,
}

/// How much of a book one slice reads. Sized against the background build's
/// own slice, since it is the same reader waiting either way.
const EVIDENCE_SLICE_BYTES: u32 = 192 * 1024;

impl SourceEvidenceJob {
    /// The copy this is reading.
    pub(crate) fn place(&self) -> &EvidencePlace {
        &self.place
    }
}

/// The place the open book occupies, which is where both its reading place
/// and what it recorded of its bytes are filed.
///
/// A place rather than a row number, since a rescan renumbers rows and this
/// has to outlive one, and rather than a [`proto::identity::BookId`], since
/// a book whose adoption a scan is still deciding has no id yet and its
/// bytes are worth reading all the same.
pub(crate) fn evidence_place(library: &ReaderStore) -> Option<EvidencePlace> {
    let index = library.active_index()?;
    let len = library.catalog_entry(index)?.byte_size;
    if len == 0 {
        return None;
    }
    let (at, locator) = library.book_location(index)?;
    let mut owned = String::new();
    owned.push_str(locator).ok()?;
    Some(EvidencePlace {
        at,
        locator: owned,
        len,
    })
}

/// The job for the open book, when the store knows where it is.
///
/// No card access: whether the claim already records the bytes is the first
/// slice's business, so arming costs an open nothing.
pub(crate) fn evidence_job(library: &ReaderStore) -> Option<SourceEvidenceJob> {
    Some(SourceEvidenceJob {
        place: evidence_place(library)?,
        offset: 0,
        started: Instant::now(),
        hasher: proto::source::SourceHasher::new(),
    })
}

/// Read the next slice of a book whose bytes are not recorded yet, and
/// record them once the last slice is in.
#[inline(never)]
pub(crate) fn continue_source_evidence(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    job: &mut SourceEvidenceJob,
) -> EvidenceStep {
    let Ok(path) = LibraryPath::parse(job.place.locator.as_str()) else {
        return EvidenceStep::Abandoned;
    };
    let key = job.place.key();
    let owner = proto::cache::CacheOwner {
        key: key.as_str(),
        root: job.place.at,
        locator: job.place.locator.as_str(),
    };
    let slice = sd_session::with_root(epd, sd_cs, |root| {
        // Asked once, on the first slice: a book whose bytes are already
        // recorded is most of a library most of the time.
        if job.offset == 0
            && files::recorded_evidence(root, &owner).is_some_and(|held| held.digest.is_some())
        {
            return EvidenceStep::Finished;
        }
        let read = upload_store::library::with_book_at(root, job.place.at, &path, |dir, alias| {
            let Ok(file) = dir.open_file_in_dir(alias, Mode::ReadOnly) else {
                return None;
            };
            if file.length() != job.place.len || file.seek_from_start(job.offset).is_err() {
                // A file that is not the length the row said is not the
                // copy this job was armed for.
                return None;
            }
            let mut buffer = [0u8; 512];
            let mut taken = 0u32;
            while taken < EVIDENCE_SLICE_BYTES && job.offset + taken < job.place.len {
                let want = (job.place.len - job.offset - taken).min(buffer.len() as u32) as usize;
                match file.read(&mut buffer[..want]) {
                    Ok(0) => break,
                    Ok(read) => {
                        job.hasher.update(&buffer[..read]);
                        taken += read as u32;
                    }
                    Err(_) => return None,
                }
            }
            Some(taken)
        });
        let Ok(Some(Some(taken))) = read else {
            return EvidenceStep::Abandoned;
        };
        job.offset += taken;
        if job.offset < job.place.len {
            return if taken == 0 {
                // The card stopped giving the book up before its end, so
                // what has been read is not the stream and cannot be
                // recorded as it.
                EvidenceStep::Abandoned
            } else {
                EvidenceStep::Continued
            };
        }
        let digest =
            core::mem::replace(&mut job.hasher, proto::source::SourceHasher::new()).finish();
        match files::record_cache_evidence(root, &owner, None, Some(digest)) {
            Ok(()) => {
                bench_log!(
                    "bench: storage_evidence action=record len={} elapsed_ms={} t_ms={}",
                    job.place.len,
                    job.started.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
                EvidenceStep::Finished
            }
            // A claim that would not take it costs a later scan the chance
            // to find this copy again, and costs this reader nothing.
            Err(_) => {
                esp_println::println!("storage: could not record what this copy is");
                EvidenceStep::Abandoned
            }
        }
    });
    slice.unwrap_or(EvidenceStep::Abandoned)
}

fn book_locator(library: &ReaderStore, index: usize) -> Option<(BookRoot, LibraryPath)> {
    let (at, path) = library.book_location(index)?;
    Some((at, LibraryPath::parse(path).ok()?))
}

#[inline(never)]
/// Row `index`'s location as its catalog record states it, verified against
/// the expected identity: the root, the locator, and the display name the
/// legacy position fallback derives from. Read from the card because the
/// resident window keeps only labels and identity; one record read on paths
/// that already run whole SD sessions. `None` when the record is unreadable,
/// names a root this build does not know, or no longer carries this
/// identity, in which case no keyed cache access can prove ownership.
/// Store where the reader is, under the copy's id.
///
/// `Ok(())` also covers the cases with nothing to store: a copy with no id
/// yet, or a page outside the resident window. `Err` is the card refusing,
/// which the caller owes a retry, because this is the position that survives
/// a layout change and a move and the page-keyed file beside it is not.
fn store_place<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &ReaderStore,
    index: usize,
    screen: u32,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(id) = record_copy_id(root, library, index) else {
        return Ok(());
    };
    let Some(anchor) = library.anchor_for_global_page(screen) else {
        return Ok(());
    };
    let Some(entry) = library.catalog_entry(index) else {
        return Ok(());
    };
    let source = (entry.source_hash, entry.byte_size);
    let progression = match place_progression(root, library, id, screen) {
        Some(progression) => progression,
        // A page total from a half-built index is a floor, not a length, and
        // dividing by it would call page 10 of an eventual 200 the halfway
        // mark. The place keeps whatever fraction it already had, which was
        // measured against a whole book.
        None => return Ok(()),
    };
    match files::write_place(root, id, anchor, source, progression) {
        Ok(()) => Ok(()),
        // The explicitly chosen collision behavior: another copy's id holds
        // the directory this one hashes to, and no retry changes that.
        Err(files::PlaceDenied::Taken) => {
            esp_println::println!("storage: another copy holds this place directory");
            Ok(())
        }
        Err(files::PlaceDenied::Fault) => {
            esp_println::println!("storage: the place write failed");
            Err(())
        }
    }
}

/// How far through the book the reader is, or `None` while the book's length
/// is still being discovered.
///
/// `None` leaves the stored progression alone rather than replacing it with a
/// worse one. It is read only when the source changes under the copy, so a
/// stale fraction measured against the whole book beats a fresh one measured
/// against the part of it that happens to be indexed.
fn place_progression<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &ReaderStore,
    id: proto::identity::BookId,
    screen: u32,
) -> Option<u16>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if library.book_index_is_partial() {
        return files::read_place(root, id).map(|place| place.progression);
    }
    let total = library.advertised_page_count().max(1);
    Some(((u64::from(screen) * u64::from(u16::MAX)) / u64::from(total)) as u16)
}

/// The id of the copy at a catalog row, read from the row itself.
///
/// The active book's id is resident, and every other row's is not, so this
/// goes to the card for it. A row with no id yet is a copy adopted by
/// firmware older than the ledger; it has no place to store and will get one
/// on the next scan.
fn record_copy_id<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &ReaderStore,
    index: usize,
) -> Option<proto::identity::BookId>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if library.active_index() == Some(index) {
        if let Some(id) = library.active_copy_id() {
            return Some(id);
        }
    }
    crate::library_sd::read_catalog_record_at(root, index)?.book_id
}

fn record_location<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
    identity: (u32, u32),
) -> Option<(
    BookRoot,
    String<{ proto::library_path::MAX_PATH_BYTES }>,
    String<64>,
)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let record = crate::library_sd::read_catalog_record_at(root, index)?;
    if (record.source_hash, record.byte_size) != identity {
        return None;
    }
    Some((record.root?, record.path, record.display_name))
}

/// Persists a reading position to both places it lives, in one card session.
///
/// The per-book position file is what this firmware reads back; the copy inside
/// the global record is a mirror, kept because MarigoldOS reads position from
/// there and cards are meant to move between the two. Writing both keeps that
/// promise without making the global copy authoritative — see `book_position`
/// in the display task for the precedence the read side applies.
pub(crate) fn store_app_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    record: AppStateRecord,
) -> bool {
    // The same session lands the global record and, for SD books, the
    // per-book position beside that book's cache, so switching books does
    // not abandon the previous one's place.
    let book = app_core::ReaderSource::from_book_id(record.book_id)
        .sd_index()
        .and_then(|index| {
            library
                .catalog_entry(index as usize)
                .map(|entry| (index as usize, (entry.source_hash, entry.byte_size)))
        });
    sd_session::with_root(epd, sd_cs, |root| {
        let state = files::write_state_file(root, record);
        let position = if let Some((index, identity)) = book {
            match record_location(root, index, identity) {
                Some((at, path, _)) => {
                    let key = proto::cache::cache_key_from(identity.0);
                    let owner = proto::cache::CacheOwner {
                        key: key.as_str(),
                        root: at,
                        locator: path.as_str(),
                    };
                    // The layout-independent place first, and its fault is
                    // the save's fault: the page file beside it cannot carry
                    // a place across a settings change or a move, so treating
                    // a refused place as success would retire a retry the
                    // reader needs.
                    let place = store_place(root, library, index, record.screen);
                    let position = match files::write_position_file(
                        root,
                        &owner,
                        record.chapter,
                        record.screen,
                    ) {
                        Ok(()) => Ok(()),
                        // A full-hash twin's active claim holds the key.
                        // Deliberate and durable, not a card fault: this
                        // book's position is unpersistable while the twin
                        // holds it, and the global record above still
                        // carries the active book's place. Failing here
                        // would block every save over a place that cannot
                        // be stored anyway.
                        Err(files::ClaimDenied::Foreign) => {
                            esp_println::println!(
                                "storage: position write refused by a foreign claim"
                            );
                            Ok(())
                        }
                        Err(files::ClaimDenied::Fault) => Err(()),
                    };
                    place.and(position)
                }
                // No provable location means no directory this write can
                // claim; the global record above still carries the place.
                None => Err(()),
            }
        } else {
            Ok(())
        };
        state.and(position)
    })
    .ok()
    .is_some_and(|result| result.is_ok())
}

/// Writes only the departing book's position file, leaving the global state
/// file naming whichever book is still active.
///
/// The first step of a book-open transaction. It has to be separable from the
/// global write: until the new book is actually open there is no correct value
/// to put in the state file, and writing the old book's position through
/// [`store_app_state`] would point the next boot at a book the reader is in the
/// middle of leaving.
///
/// A book whose catalog entry cannot be resolved reports failure rather than
/// quietly writing nothing — the transaction treats a silent no-op as a lost
/// page, which is exactly what it exists to prevent. Built-in books have no
/// position file and owe nothing, so they succeed.
#[inline(never)]
pub(crate) fn store_book_position(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    record: AppStateRecord,
) -> bool {
    let Some(index) = app_core::ReaderSource::from_book_id(record.book_id).sd_index() else {
        return true;
    };
    let Some(entry) = library.catalog_entry(index as usize) else {
        esp_println::println!(
            "storage: no catalog entry for departing book_id={} index={}",
            record.book_id,
            index
        );
        return false;
    };
    let identity = (entry.source_hash, entry.byte_size);
    sd_session::with_root(epd, sd_cs, |root| {
        let (at, path, _) = record_location(root, index as usize, identity).ok_or(())?;
        let key = proto::cache::cache_key_from(identity.0);
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: at,
            locator: path.as_str(),
        };
        let place = store_place(root, library, index as usize, record.screen);
        let position = match files::write_position_file(root, &owner, record.chapter, record.screen)
        {
            Ok(()) => Ok(()),
            // A twin's active claim holds the key: the departing book's
            // place cannot be stored while it does, and refusing the whole
            // book-open transaction over it would leave the reader stuck on
            // this book. The loss is bounded to this book's place, which no
            // write can land anyway.
            Err(files::ClaimDenied::Foreign) => {
                esp_println::println!("storage: departing position refused by a foreign claim");
                Ok(())
            }
            Err(files::ClaimDenied::Fault) => Err(()),
        };
        // The departing book's place carries the same weight as its page, and
        // for the same reason: nothing else records where it was in a form
        // that survives a layout change.
        place.and(position)
    })
    .ok()
    .is_some_and(|result| result.is_ok())
}

/// Writes only the global state file: which book is active, and the reader
/// settings that travel with it.
///
/// The last step of a book-open transaction. The book's own position file was
/// already written when it was last read, and the open resolved the position
/// from it, so rewriting it here would only copy it back onto itself.
#[inline(never)]
pub(crate) fn store_global_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    record: AppStateRecord,
) -> bool {
    sd_session::with_root(epd, sd_cs, |root| files::write_state_file(root, record))
        .ok()
        .is_some_and(|result| result.is_ok())
}

/// What a copy's stored place says, before any pagination exists to resolve
/// it against.
///
/// Two shapes because two things can be stored. A place names content and has
/// to be resolved once the book is paginated for this layout, which is the
/// point of it: the page it lands on depends on the layout, and the layout is
/// adopted by the open that reads this. A legacy position names a page under
/// whatever layout wrote it, which is all an older card holds.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SavedPlace {
    Place {
        anchor: proto::anchor::ContentAnchor,
        /// Whether the bytes the anchor was resolved against are still the
        /// bytes at this locator. False demotes the anchor to a guess and
        /// leaves the progression, per the reading-position PRD's R13.
        exact: bool,
        progression: u16,
    },
    Page {
        chapter: u16,
        page: u32,
    },
}

impl SavedPlace {
    /// Where to open while the place is still unresolved.
    ///
    /// For a place that is the spine item's first page: the item is known
    /// from the anchor, and which page inside it needs a pagination that this
    /// open has yet to build. [`resolve_place`] refines it once there is one.
    pub(crate) fn provisional(self) -> (u16, u32) {
        match self {
            Self::Place { anchor, .. } => (anchor.spine, 0),
            Self::Page { chapter, page } => (chapter, page),
        }
    }
}

/// The stored place for a catalog entry's copy, or the page an older card
/// holds for it.
///
/// Reads the place first and falls back to the page-keyed position, so a card
/// written by earlier firmware resumes where it left off. The next save
/// publishes a place.
#[inline(never)]
pub(crate) fn load_place(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    index: usize,
) -> Option<SavedPlace> {
    let entry = library.catalog_entry(index)?;
    let identity = (entry.source_hash, entry.byte_size);
    sd_session::with_root(epd, sd_cs, |root| {
        let (at, path, display_name) = record_location(root, index, identity)?;
        let key = proto::cache::cache_key_from(identity.0);
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: at,
            locator: path.as_str(),
        };
        if let Some(id) = record_copy_id(root, library, index) {
            if let Some(place) = files::read_place(root, id) {
                return Some(SavedPlace::Place {
                    anchor: place.anchor,
                    exact: place.describes(identity),
                    progression: place.progression,
                });
            }
        }
        files::read_position_file_or_legacy(root, &owner, display_name.as_str(), entry.byte_size)
            .map(|(chapter, page)| SavedPlace::Page { chapter, page })
    })
    .ok()
    .flatten()
}

/// The page a stored place opens at, once the book is paginated for the
/// layout this open adopted.
///
/// Has to run after the index is resident and cannot run before: the index
/// that names sections and their start pages is the one thing that says where
/// an anchor falls, and it belongs to a layout. Resolving against the index
/// that happened to be in RAM would answer with another book's geometry, or
/// with this book's under the settings the reader just left.
///
/// `None` for a place that needs no refining, or one this book cannot place.
#[inline(never)]
pub(crate) fn resolve_place(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    index: usize,
    place: SavedPlace,
) -> Option<u32> {
    let SavedPlace::Place {
        anchor,
        exact,
        progression,
    } = place
    else {
        return None;
    };
    let total = library.advertised_page_count();
    if !exact {
        // The bytes changed under the copy, so the anchor is a guess and the
        // progression carries what an edit leaves: the reader lands nearby
        // rather than at page one.
        let page = ((u64::from(progression) * u64::from(total)) / u64::from(u16::MAX)) as u32;
        esp_println::println!(
            "restore: the source changed; resuming near {}/{}",
            page,
            total
        );
        return Some(page.min(total.saturating_sub(1)));
    }
    let entry = library.catalog_entry(index)?;
    let identity = (entry.source_hash, entry.byte_size);
    let record = library
        .section_for_anchor(anchor)
        .and_then(|section| library.book_section(section))?;
    sd_session::with_root(epd, sd_cs, |root| {
        let (at, path, _) = record_location(root, index, identity)?;
        let key = proto::cache::cache_key_from(identity.0);
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: at,
            locator: path.as_str(),
        };
        // By section, not by spine. One spine item can hold several sections,
        // and the file is named for the section; the header's spine check
        // then confirms the file is the item the anchor names.
        let within = files::page_of_anchor_in_section(
            root,
            &owner,
            library.layout_key(),
            record.section,
            anchor,
        )?;
        Some(record.start_page.saturating_add(u32::from(within)))
    })
    .ok()
    .flatten()
}

/// Load the book's full chapter list from TOC.BIN into the reader's section
/// buffer for the Chapters overview. The reading section reloads on exit.
#[inline(never)]
pub(crate) fn load_chapters_into_store(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    selection: usize,
) -> bool {
    let Some(entry) = library.catalog_entry(index) else {
        return false;
    };
    let source_identity = (entry.source_hash, entry.byte_size);
    let key = proto::cache::cache_key_from(source_identity.0);
    // The chapters view opens for the loaded book, whose locator is
    // resident; a row with no resident locator has no provable directory.
    let Some((at, path)) = book_locator(library, index) else {
        return false;
    };
    // Center the window on the selection so scrolling either way has slack
    // before the next reload.
    let window_start = selection.saturating_sub(reader_cache::store::TOC_WINDOW_CAPACITY / 2);
    sd_session::with_root(epd, sd_cs, |root| {
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: at,
            locator: path.as_str(),
        };
        files::load_v2_toc_into_text(root, &owner, source_identity, library, window_start)
    })
    .unwrap_or(false)
}

/// Make the TOC window cover the Chapters rows visible around `selection`,
/// reloading it from TOC.BIN only on a miss — the overview analogue of
/// `ensure_folder_page`. Cheap when the window already covers.
pub(crate) fn ensure_toc_window(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    selection: usize,
    portrait: bool,
) -> bool {
    let first_visible =
        ui::render::toc_scroll_start(selection, library.overview_chapter_count(), portrait);
    if library.toc_window_covers(first_visible, ui::render::toc_visible_rows(portrait)) {
        return true;
    }
    load_chapters_into_store(epd, sd_cs, library, index, selection)
}

#[inline(never)]
pub(crate) fn store_wifi_credentials(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    record: proto::nvm::WifiCredentialsRecord,
) -> bool {
    sd_session::with_root(epd, sd_cs, |root| {
        files::write_wifi_file(root, record).is_ok()
    })
    .unwrap_or(false)
}

#[inline(never)]
pub(crate) fn store_wifi_ap_hint(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    record: proto::nvm::WifiApHintRecord,
) -> bool {
    sd_session::with_root(epd, sd_cs, |root| {
        files::write_wifi_hint_file(root, record).is_ok()
    })
    .unwrap_or(false)
}

#[inline(never)]
pub(crate) fn load_wifi_ap_hint(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
) -> Option<proto::nvm::WifiApHintRecord> {
    // Not point-free: passing the function directly fixes its `Directory`
    // lifetime to one region, and `with_root` needs a `for<'a>` caller.
    #[allow(clippy::redundant_closure)]
    sd_session::with_root(epd, sd_cs, |root| files::read_wifi_hint_file(root))
        .ok()
        .flatten()
}

#[inline(never)]
pub(crate) fn forget_wifi_credentials(epd: &mut Epd, sd_cs: &mut Output<'static>) -> bool {
    // Not point-free: passing the function directly fixes its `Directory`
    // lifetime to one region, and `with_root` needs a `for<'a>` caller.
    #[allow(clippy::redundant_closure)]
    sd_session::with_root(epd, sd_cs, |root| files::delete_wifi_file(root)).unwrap_or(false)
}

/// Delete one catalog row's rebuildable cache (sections, BOOK/TOC/COVER and
/// the content stream), keeping its position files and catalog entry. The
/// row's identity is checked against the cache header before anything is
/// deleted, so a stale index or a key collision clears nothing. When the
/// cleared book is the resident one, the loaded state drops with it and the
/// next open takes the ordinary cache-miss rebuild path instead of
/// answering from RAM.
#[inline(never)]
pub(crate) fn clear_book_cache(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: u16,
) -> bool {
    let index = index as usize;
    let resolved = match library.catalog_entry(index) {
        Some(entry) => Some((
            proto::cache::cache_key_from(entry.source_hash),
            entry.source_hash,
            entry.byte_size,
        )),
        // The row is neither the open book nor inside the resident list
        // window, so read it off the card directly rather than through
        // `load_active_entry`, which would publish it as the active entry and
        // evict the open book's.
        None => crate::library_sd::read_row_cache_identity(epd, sd_cs, index),
    };
    let Some((cache_key, source_hash, source_size)) = resolved else {
        return false;
    };
    // Whether the delete ran at all, and whether it left nothing behind. The
    // two differ: a refusal touches nothing, while a partial pass can take
    // BOOK.BIN and stall on a section, and the resident state has to be
    // dropped in that case as surely as in the clean one.
    let (attempted, cleared) = sd_session::with_root(epd, sd_cs, |root| {
        // The row is a specific book, so its exact ownership is checkable
        // where the sweep's is not: a full 32-bit twin passes the header
        // identity test below, and without this gate the wrong row could
        // clear the claim holder's cache. Unclaimed directories fall through
        // to the identity test, which is all pre-claim caches ever had.
        if let Some((at, path, _)) = record_location(root, index, (source_hash, source_size)) {
            let owner = proto::cache::CacheOwner {
                key: cache_key.as_str(),
                root: at,
                locator: path.as_str(),
            };
            match files::cache_dir_claim(root, &owner) {
                files::ClaimState::MineActive
                | files::ClaimState::MineReleased
                | files::ClaimState::Unclaimed => {}
                files::ClaimState::OtherActive
                | files::ClaimState::OtherReleased
                | files::ClaimState::Fault => return (false, false),
            }
        }
        match files::read_cache_header(root, cache_key.as_str()) {
            files::CacheHeader::Present(header) => {
                if header.source_hash != source_hash || header.source_size != source_size {
                    // Whatever sits under this key is not this book's cache;
                    // refuse rather than delete another book's data.
                    return (false, false);
                }
            }
            // An index that will not read cannot say whose cache this is, and
            // a key is 28 bits of hash — a collision is a case the format
            // admits. Fail closed: a corrupt or briefly unreadable BOOK.BIN
            // belonging to another book must not be answered by deleting it.
            files::CacheHeader::Unreadable => return (false, false),
            // No index at all is different. Nothing usable is there for
            // anyone, so sweeping the shells a truncated pass left behind
            // costs the colliding book nothing it had not already lost — and
            // its position files are never swept regardless.
            files::CacheHeader::Absent => {}
        }
        //
        // The delete reports on every file it was supposed to remove, not
        // just the index: a pass that took BOOK.BIN but stalled on a section
        // has freed no space and left a directory the sweep will keep
        // tripping over, and telling the user "cache cleared" for that is a
        // lie they cannot check.
        let emptied = files::empty_cache_dir(root, cache_key.as_str());
        (
            true,
            emptied
                && matches!(
                    files::read_cache_header(root, cache_key.as_str()),
                    files::CacheHeader::Absent
                ),
        )
    })
    .unwrap_or((false, false));
    // Keyed on the attempt, not the verdict. A half-finished delete leaves
    // the resident sections, index, TOC, and cover describing files that are
    // already gone; answering the next read from that RAM would strand the
    // reader on a section crossing. Dropping it costs a rebuild, which is
    // what the failed clear will need anyway.
    if attempted {
        if library.loaded_index == Some(index) {
            library.loaded_index = None;
            library.clear_book_index();
            library.clear_lines();
            library.clear_toc();
            library.set_text_holds_toc(false);
            library.set_reader_status(BookLoadStatus::Empty);
        }
        if library.current_index() == Some(index) {
            // COVER.BIN is gone; the resident cover regenerates on rebuild.
            library.clear_cover();
        }
    }
    cleared
}

#[inline(never)]
pub(crate) fn load_wifi_credentials(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
) -> Option<proto::nvm::WifiCredentialsRecord> {
    // Not point-free: passing the function directly fixes its `Directory`
    // lifetime to one region, and `with_root` needs a `for<'a>` caller.
    #[allow(clippy::redundant_closure)]
    sd_session::with_root(epd, sd_cs, |root| files::read_wifi_file(root))
        .ok()
        .flatten()
}

/// Kept out of line for the same stack discipline as the store side.
#[inline(never)]
pub(crate) fn load_app_state(epd: &mut Epd, sd_cs: &mut Output<'static>) -> Option<AppStateRecord> {
    // Not point-free: the generic fn item fails the closure's HRTB check.
    #[allow(clippy::redundant_closure)]
    sd_session::with_root(epd, sd_cs, |root| files::read_state_file(root))
        .ok()
        .flatten()
}

#[inline(never)]
pub(crate) fn load_custom_font_manifest(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
) {
    if display::font::builtin_custom_available() {
        library.set_custom_font(
            Some(display::font::builtin_custom_name()),
            display::font::builtin_custom_identity(),
            &[],
        );
        esp_println::println!(
            "font: builtin custom '{}' identity={:016x}",
            display::font::builtin_custom_name(),
            display::font::builtin_custom_identity()
        );
        return;
    }
    // The closure is not redundant: passing the function directly fixes its
    // `Directory` lifetime to one specific region, and `with_root` needs a
    // `for<'a>` caller. Clippy suggests a form that does not compile.
    #[allow(clippy::redundant_closure)]
    let manifest = sd_session::with_root(epd, sd_cs, |root| files::read_custom_font_manifest(root))
        .ok()
        .flatten();
    if let Some(manifest) = manifest {
        esp_println::println!(
            "font: custom '{}' identity={:016x}",
            manifest.name.as_str(),
            manifest.identity
        );
        library.set_custom_font(
            Some(manifest.name.as_str()),
            manifest.identity,
            &manifest.faces[..manifest.face_count],
        );
    } else {
        esp_println::println!("font: no custom font pack");
        library.set_custom_font(None, 0, &[]);
    }
}

/// Track the chapter for the page just rendered while reading, past the
/// reducer's 128-chapter `sd_chapter_for_page` cap. Cheap in-RAM resolve every
/// render; only touches SD (a 48-byte TOC title read) when the chapter actually
/// changes. Returns the new uncapped chapter so the caller can forward it to the
/// reducer, else `None` when nothing changed (or the chapter map is not
/// resident, e.g. a built-in book).
#[inline(never)]
pub(crate) fn track_reading_chapter(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    global_page: u32,
    library: &mut ReaderStore,
) -> Option<u16> {
    if !library.chapter_start_ready() {
        return None;
    }
    let current = library.current_chapter_for_page(global_page);
    if current == library.current_chapter() {
        return None;
    }
    let index = library.loaded_index?;
    load_chapter_title(epd, sd_cs, index, current, library);
    Some(current)
}

/// Read a chapter's title from the book's TOC.BIN and make it the resident
/// current chapter. Used at boot restore (so wake-to-Home names the chapter
/// before the book is opened) and on reading renders when the chapter changes.
/// Tags the title with the book's source identity; a colophon shows it only for
/// that book.
#[inline(never)]
pub(crate) fn load_chapter_title(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    index: usize,
    chapter: u16,
    library: &mut ReaderStore,
) {
    let Some(entry) = library.catalog_entry(index) else {
        return;
    };
    let source_identity = (entry.source_hash, entry.byte_size);
    let cache_key = proto::cache::cache_key_from(source_identity.0);
    // A book with no locator is not found, the same as one whose title would
    // not read: either way the fallback below runs.
    let found = match book_locator(library, index) {
        Some((at, path)) => sd_session::with_root(epd, sd_cs, |root| {
            let owner = proto::cache::CacheOwner {
                key: cache_key.as_str(),
                root: at,
                locator: path.as_str(),
            };
            files::read_v2_toc_chapter_title(root, &owner, source_identity, chapter, library)
        })
        .unwrap_or(false),
        None => false,
    };
    if !found {
        // Tag the source even on a miss so a stale title from another book is
        // never shown; the colophon falls back to a numeral for this chapter.
        library.set_current_chapter(chapter, "", source_identity);
    }
}

/// Read the book's total page count from its V2 index header at boot restore so
/// the Home progress bar has a denominator before the book is opened. Returns 0
/// if unavailable (caller leaves the bar on its fallback).
#[inline(never)]
pub(crate) fn restore_book_page_count(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    index: usize,
    library: &ReaderStore,
) -> u32 {
    let Some(entry) = library.catalog_entry(index) else {
        return 0;
    };
    let source_identity = (entry.source_hash, entry.byte_size);
    let cache_key = proto::cache::cache_key_from(source_identity.0);
    let Some((at, path)) = book_locator(library, index) else {
        return 0;
    };
    sd_session::with_root(epd, sd_cs, |root| {
        let owner = proto::cache::CacheOwner {
            key: cache_key.as_str(),
            root: at,
            locator: path.as_str(),
        };
        files::read_v2_book_total_pages(root, &owner, source_identity, library)
    })
    .unwrap_or(0)
}

#[inline(never)]
#[expect(clippy::too_many_arguments)] // Cache identity, the page wanted, the store, and the two flags the decision needs
fn try_load_v2_book_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    requested_global_page: u32,
    library: &mut ReaderStore,
    started: Instant,
    label: &str,
    // Whether a suspended walk for this very book is still in the scratch.
    // Only that makes an unfinished index safe to read from.
    walk_is_live: bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match files::load_v2_book_index(root, owner, source_identity, library) {
        BookIndexLoadResult::Hit { unfinished }
            if !app_core::storage_loop::partial_index_is_usable(unfinished, walk_is_live) =>
        {
            // An index a build walked away from. Reading it would cap the book
            // at whatever that build reached, with no input able to raise it —
            // the reducer clamps the reader to the advertised count. Rebuild
            // instead; the rebuild is progressive, so the first page still
            // arrives in about a second.
            esp_println::println!(
                "epub: v2 {label} book index unfinished with no build running; rebuilding"
            );
            false
        }
        BookIndexLoadResult::Hit { .. } => {
            match files::load_v2_section_by_global_page(
                root,
                owner,
                source_identity,
                requested_global_page,
                library,
            ) {
                CacheLoadResult::Hit { pages, .. } => {
                    layout::rebuild_toc_page_targets(library);
                    publish::refresh_chapter_tracking(
                        root,
                        owner,
                        source_identity,
                        requested_global_page,
                        library,
                    );
                    let cover = files::load_v2_cover_cache(root, owner, library);
                    esp_println::println!(
                        "epub: v2 {label} book cache ready after {} ms (total={} section_pages={} toc={} cover={:?})",
                        started.elapsed().as_millis(),
                        library.advertised_page_count(),
                        pages,
                        library.toc_count(),
                        cover
                    );
                    true
                }
                other => {
                    esp_println::println!("epub: {label} book index section load {:?}", other);
                    false
                }
            }
        }
        BookIndexLoadResult::Invalid => {
            esp_println::println!("epub: v2 {label} book index invalid");
            false
        }
        BookIndexLoadResult::Miss => {
            esp_println::println!("epub: v2 {label} book index miss");
            false
        }
    }
}

fn set_preview_error(library: &mut ReaderStore, message: &str) {
    library.set_reader_error(message);
}

fn status_for_load_result(
    result: Option<Result<(), ReaderCacheError>>,
    library: &mut ReaderStore,
) -> BookLoadStatus {
    match result {
        Some(Ok(())) => BookLoadStatus::Ready,
        Some(Err(err)) => {
            esp_println::println!("epub: load failed: {:?}", err);
            set_preview_error_from_error(library, err);
            BookLoadStatus::Error
        }
        None => BookLoadStatus::Error,
    }
}

fn session_error_label(error: SdSessionError) -> &'static str {
    match error {
        SdSessionError::CardInit => "CARD INIT",
        SdSessionError::Volume => "VOLUME",
        SdSessionError::Root => "ROOT",
    }
}

fn set_preview_error_from_error(library: &mut ReaderStore, error: ReaderCacheError) {
    let message = match error {
        ReaderCacheError::Zip(proto::epub::ZipError::OutputTooSmall) => "EPUB TOO BIG",
        ReaderCacheError::Zip(proto::epub::ZipError::EntryBufferTooSmall) => "PATH LONG",
        ReaderCacheError::Zip(proto::epub::ZipError::UnsupportedCompression) => "ZIP METHOD",
        ReaderCacheError::Zip(proto::epub::ZipError::EntryNotFound) => "ZIP MISSING",
        ReaderCacheError::Zip(proto::epub::ZipError::Inflate) => "ZIP INFLATE",
        ReaderCacheError::Zip(proto::epub::ZipError::Io) => "OPEN BUDGET",
        ReaderCacheError::Zip(_) => "ZIP",
        ReaderCacheError::Epub(proto::epub::EpubError::TooManyManifestItems) => "OPF MANIFEST",
        ReaderCacheError::Epub(proto::epub::EpubError::TooManySpineItems) => "OPF SPINE",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpfPath) => "NO OPF",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpf) => "NO OPF FILE",
        ReaderCacheError::Epub(proto::epub::EpubError::Utf8) => "OPF UTF8",
        ReaderCacheError::Epub(proto::epub::EpubError::Zip(_)) => "OPF ZIP",
        ReaderCacheError::Epub(_) => "OPF",
        ReaderCacheError::Xhtml(proto::epub::XhtmlError::TooManyRuns) => "TEXT FULL",
        ReaderCacheError::Utf8 => "UTF8",
        ReaderCacheError::MissingSpine => "NO SPINE",
        ReaderCacheError::NoBodyText => "NO BODY TEXT",
        ReaderCacheError::SectionRead => "SECTION READ",
        ReaderCacheError::IndexWrite => "CACHE WRITE",
        ReaderCacheError::EntryNameTooLong => "PATH LONG",
    };
    set_preview_error(library, message);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderCacheError {
    Zip(proto::epub::ZipError),
    Epub(proto::epub::EpubError),
    Xhtml(proto::epub::XhtmlError),
    Utf8,
    MissingSpine,
    NoBodyText,
    /// The book built and its cache was written, but the requested section
    /// failed to load back from the card.
    SectionRead,
    /// The book built, but writing BOOK.BIN failed partway; the cache dir
    /// was cleared because a truncated index serves no load path.
    IndexWrite,
    EntryNameTooLong,
}

/// Print what a completed publish did.
///
/// Reporting only: every argument is already-decided fact, including the cover,
/// which the publish adopted into the store on its way through. This function
/// deliberately touches neither the card nor the store — an earlier version
/// loaded the cover here, which quietly made a *reporting* helper responsible
/// for publication state, and the one publish path that does not report (the
/// progressive first open) lost its cover as a result.
#[expect(clippy::too_many_arguments)] // Telemetry glue: every argument is one field of one line.
fn report_publish(
    cache_key: &str,
    label: &str,
    open_started: Instant,
    spine_started: Instant,
    io_start: crate::sd_session::sd_stats::Snapshot,
    section_write_micros: u64,
    cover: Option<files::CoverLoadResult>,
    sections: usize,
    total_pages: u32,
    book_partial: bool,
) {
    esp_println::println!("epub: stage PublishLoaded");
    esp_println::println!(
        "epub: {label} book cache ready after {} ms (total={} sections={} partial={} cover={:?} key {})",
        open_started.elapsed().as_millis(),
        total_pages,
        sections,
        book_partial,
        cover,
        cache_key
    );
    let io = crate::sd_session::sd_stats::snapshot().since(io_start);
    bench_log!(
        "bench: storage_build elapsed_ms={} spine_ms={} write_ms={} sections={} pages={} rd_calls={} rd_blocks={} wr_calls={} wr_blocks={} key={}",
        open_started.elapsed().as_millis(),
        spine_started.elapsed().as_millis(),
        section_write_micros / 1000,
        sections,
        total_pages,
        io.read_calls,
        io.read_blocks,
        io.write_calls,
        io.write_blocks,
        cache_key
    );
}

impl From<publish::PublishError> for ReaderCacheError {
    fn from(error: publish::PublishError) -> Self {
        match error {
            publish::PublishError::IndexWrite => Self::IndexWrite,
            publish::PublishError::SectionRead => Self::SectionRead,
        }
    }
}

impl From<proto::epub::ZipError> for ReaderCacheError {
    fn from(value: proto::epub::ZipError) -> Self {
        Self::Zip(value)
    }
}

impl From<proto::epub::EpubError> for ReaderCacheError {
    fn from(value: proto::epub::EpubError) -> Self {
        Self::Epub(value)
    }
}

impl From<proto::epub::XhtmlError> for ReaderCacheError {
    fn from(value: proto::epub::XhtmlError) -> Self {
        Self::Xhtml(value)
    }
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn build_or_load_epub_cache_from_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_path: &str,
    at: BookRoot,
    locator: &LibraryPath,
    catalog_index: u16,
    _requested_chapter: u16,
    target_pages: usize,
    resume: Option<BookBuildResume>,
    library: &mut ReaderStore,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> Result<(), ReaderCacheError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let open_started = Instant::now();
    let source_len = file.length();
    // Identity hashes where the book is and the length the file has now,
    // rather than the catalog's copy of it, so a file replaced on the card
    // re-keys instead of answering from the old book's cache. `source_path`
    // stays a label from here on.
    let source_identity = (
        proto::cache::source_hash_at(at, locator.as_str(), source_len),
        source_len,
    );
    let cache_key = proto::cache::cache_key_from(source_identity.0);
    library.set_cache_key(cache_key.as_str());
    let owner = proto::cache::CacheOwner {
        key: cache_key.as_str(),
        root: at,
        locator: locator.as_str(),
    };

    esp_println::println!("epub: stage OpenSdFile len={}", source_len);
    let requested_global_page = target_pages as u32;

    esp_println::println!("epub: zip open len={}", source_len);
    let reader = SdFileReadAt {
        file,
        len: source_len,
        read_ops: 0,
        read_bytes: 0,
    };
    let zip = ZipStream::new(reader, scratch.tail)?;
    esp_println::println!(
        "epub: zip ready after {} ms",
        open_started.elapsed().as_millis()
    );

    let outcome = build_or_load_epub_cache_from_zip(
        zip,
        root,
        source_path,
        source_identity,
        &owner,
        catalog_index,
        requested_global_page,
        resume,
        open_started,
        library,
        font_metrics,
        ZipBuildScratch {
            header: scratch.header,
            name: scratch.name,
            compressed: scratch.compressed,
            container: scratch.container,
            opf: scratch.opf,
            xhtml: scratch.xhtml,
            book_sections: scratch.book_sections,
            zip_inflate: &mut *scratch.zip_inflate,
        },
    );
    // The one place the suspended build is stored, so it can never be set for
    // a walk that failed: an error carries no continuation.
    scratch.resume = match &outcome {
        Ok(next) => *next,
        Err(_) => None,
    };
    outcome.map(|_| ())
}

struct ZipBuildScratch<'a> {
    header: &'a mut [u8; READER_HEADER_SCRATCH],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    container: &'a mut [u8; READER_CONTAINER_SCRATCH],
    opf: &'a mut [u8; READER_OPF_SCRATCH],
    xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
    book_sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    zip_inflate: &'a mut ZipInflateScratch,
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn build_or_load_epub_cache_from_zip<
    Z,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    mut zip: Z,
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_path: &str,
    source_identity: (u32, u32),
    owner: &proto::cache::CacheOwner<'_>,
    catalog_index: u16,
    requested_global_page: u32,
    resume: Option<BookBuildResume>,
    open_started: Instant,
    library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    scratch: ZipBuildScratch<'_>,
) -> Result<Option<BookBuildResume>, ReaderCacheError>
where
    Z: EpubZipOps,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let io_start = crate::sd_session::sd_stats::snapshot();
    let mut section_write_micros: u64 = 0;
    esp_println::println!("epub: stage ParseContainerAndOpf");
    // ~260 B stack local: copying the OPF path out releases the borrow on
    // scratch.container so the capture stage below can reuse that buffer
    // during the spine walk. Memory delta: +260 B in the EPUB-open frame;
    // both boards' release links (stack ASSERT) pass with it.
    let mut opf_path_buf = String::<256>::new();
    proto::epub::load_container_xml_and_find_opf_path(
        &mut zip,
        scratch.header,
        scratch.name,
        scratch.compressed,
        scratch.container,
        &mut *scratch.zip_inflate,
        &mut opf_path_buf,
    )
    .map_err(|err| match err {
        // Keep the pre-helper diagnostics: zip failures surface as zip
        // errors and container text failures as "UTF8", never as
        // OPF-stage labels.
        proto::epub::EpubError::Zip(zip_err) => ReaderCacheError::Zip(zip_err),
        proto::epub::EpubError::Utf8 => ReaderCacheError::Utf8,
        other => ReaderCacheError::Epub(other),
    })?;
    let opf_path = opf_path_buf.as_str();

    let opf_entry = zip.find_entry(opf_path, scratch.header, scratch.name)?;
    esp_println::println!(
        "epub: opf compressed={} uncompressed={}",
        opf_entry.compressed_size,
        opf_entry.uncompressed_size
    );
    let (opf_len, opf_complete) = zip.read_entry_prefix_streamed(
        opf_entry,
        scratch.compressed,
        scratch.opf,
        &mut *scratch.zip_inflate,
    )?;
    if !opf_complete {
        esp_println::println!(
            "epub: opf prefix truncated at {} of {} bytes",
            opf_len,
            opf_entry.uncompressed_size
        );
    }
    let opf_xml =
        core::str::from_utf8(&scratch.opf[..opf_len]).map_err(|_| ReaderCacheError::Utf8)?;
    let package = parse_opf(opf_xml, BookId(2), source_path, 0, opf_path)?;
    esp_println::println!(
        "epub: opf parsed after {} ms (spine={} truncated={})",
        open_started.elapsed().as_millis(),
        package.spine.len(),
        package.spine_truncated
    );

    // A continuation keeps the labels, cover, TOC, and TOC.BIN the first step
    // already resolved — they describe the book, not the range built so far —
    // and goes straight back to the remaining spine items.
    if resume.is_none() {
        library.set_book_labels(package.meta.title, package.meta.author);
        library.clear_cover();
    }
    if zip.is_forward_only() {
        library.clear_toc();
    } else if resume.is_none() {
        // Stream the whole chapter list into the (currently idle) xhtml
        // scratch as fixed records, then write it to TOC.BIN so the overview
        // can read it from the card instead of holding it all resident.
        let toc_record_count = load_epub_toc(
            &mut zip,
            opf_path,
            &package,
            library,
            &mut scratch.xhtml[..],
            TocScratch {
                header: scratch.header,
                name: scratch.name,
                compressed: scratch.compressed,
                zip_inflate: &mut *scratch.zip_inflate,
            },
        );
        let toc_bytes = toc_record_count
            .saturating_mul(proto::cache::TOC_CHAPTER_RECORD_BYTES)
            .min(scratch.xhtml.len());
        let wrote_toc = files::write_v2_toc_file(
            root,
            owner,
            source_identity,
            toc_record_count,
            &scratch.xhtml[..toc_bytes],
        );
        esp_println::println!(
            "epub: toc.bin wrote {} chapter(s) ok={}",
            toc_record_count,
            wrote_toc
        );
    }
    esp_println::println!(
        "epub: toc parsed after {} ms ({} item(s))",
        open_started.elapsed().as_millis(),
        library.toc_count()
    );
    let css_rules = CssRules::new();

    esp_println::println!("epub: stage BuildV2BookCache");
    let spine_started = Instant::now();
    let mut xhtml_path = String::<MAX_ENTRY_NAME_BYTES>::new();
    // A continuation adopts the counters of the steps before it and the
    // section records they left in the scratch statics — clearing them here
    // would throw away the built half of the book.
    let (mut section_count, mut total_pages, mut saw_spine, resumed_partial) = match resume {
        Some(state) => (
            state.section_count as usize,
            state.total_pages,
            true,
            state.book_partial,
        ),
        None => {
            scratch.book_sections.fill(EMPTY_BOOK_SECTION_RECORD);
            (0usize, 0u32, false, false)
        }
    };
    let sections = &mut *scratch.book_sections;
    // A spine clipped at MAX_SPINE_ITEMS means the tail chapters were dropped at
    // parse, so the book is partial even if every kept section caches cleanly.
    let mut book_partial = resumed_partial || package.spine_truncated;
    let visible_page_capacity = library.page_capacity().max(1);
    // Decided once, by the step that read the TOC. A continuation has not, and
    // by now the resident TOC may hold headings this build generated, so
    // re-deriving it here would flip the answer mid-book.
    let generate_toc_from_headings = match resume {
        Some(state) => state.generate_toc_from_headings,
        None => library.toc_count() == 0,
    };
    // Items this step has committed to. `suspend_before` uses it to guarantee a
    // step always takes at least one item, however large.
    let mut walked_this_step = 0usize;
    let phase = match resume {
        Some(_) => app_core::storage_loop::BuildPhase::Background {
            slice_ms: BACKGROUND_SLICE_MS,
        },
        None => app_core::storage_loop::BuildPhase::FirstOpen {
            requested_page: requested_global_page,
        },
    };
    let start_spine_index = package
        .text_reference_href
        .and_then(|href| {
            package
                .spine
                .iter()
                .position(|item| href_matches_spine(href, item.href.of(package.opf_text)))
        })
        .unwrap_or_else(|| inferred_start_spine_index(&package));
    // Where a continuation picks the walk back up. Always a spine boundary the
    // previous step finished, so the sections already on the card end exactly
    // where this resumes.
    let resume_spine_index = resume.map_or(0, |state| state.next_spine as usize);

    // The whole cache tree is created here, once; the capture and the
    // sections walk below only open what already exists.
    let cache_dirs_ok = files::ensure_v2_cache_dirs(root, owner).is_ok();
    if !cache_dirs_ok {
        esp_println::println!("cache: v2 ensure dirs failed key={}", owner.key);
    }
    // Capture the push_block stream into CONT.BIN alongside the build, so a
    // type-settings change can replay it without re-parsing the EPUB. The
    // capture is a pure accelerator: it disables itself on any failure and
    // never fails the build. Its directory handle outlives the whole walk
    // because the capture's file handle borrows it.
    let content_dir = if cache_dirs_ok {
        files::open_v2_book_dir(root, owner)
    } else {
        None
    };
    // One SD block of capture staging, carved from the 4 KB container
    // scratch — container.xml parsing is finished with it by now, so this
    // costs no new RAM. The FAT layer pays per-write-call overhead (block
    // lookup plus read-modify-write of partial sectors), so a full-block
    // stage batches twice the records per call compared to the earlier
    // 256 B stage.
    const CONTENT_STAGE_BYTES: usize = 512;
    let (stage_buf, _) = scratch.container.split_at_mut(CONTENT_STAGE_BYTES);
    // A continuation appends to the file the first step created; the header
    // stays incomplete until the last step finishes it, so a build abandoned
    // between steps leaves something replay refuses rather than a truncated
    // stream it would trust. Once the capture has failed it stays failed —
    // resuming an append past a hole would corrupt the record framing.
    let mut content = match resume {
        None => files::ContentCapture::begin(content_dir.as_ref(), source_identity, stage_buf),
        Some(state) if state.content_ok => files::ContentCapture::resume(
            content_dir.as_ref(),
            source_identity,
            stage_buf,
            state.content_spine_count,
        ),
        Some(_) => files::ContentCapture::disabled(stage_buf, source_identity),
    };

    // The SECTIONS directory stays open for the whole spine walk; every
    // section flush is one file open instead of a four-level dir walk.
    //
    // `Ok(Some(next_spine))` means the walk suspended and owes a continuation
    // from that spine item; `Ok(None)` means it reached the end of the book.
    let walk = files::with_v2_sections_dir(root, owner, |sections_dir| {
        for (spine_index, spine) in package.spine.iter().enumerate().filter(|(index, item)| {
            *index >= start_spine_index
                && *index >= resume_spine_index
                && !item.href.is_empty()
                && !spine_item_is_navigation(item, &package)
        }) {
            if section_count >= sections.len() {
                book_partial = true;
                break;
            }
            saw_spine = true;
            resolve_epub_href(opf_path, spine.href.of(package.opf_text), &mut xhtml_path)?;
            esp_println::println!("epub: find spine {}", xhtml_path.as_str());
            let Ok(xhtml_entry) = zip.find_entry(&xhtml_path, scratch.header, scratch.name) else {
                continue;
            };
            esp_println::println!(
                "epub: spine {} compressed={} uncompressed={}",
                xhtml_path.as_str(),
                xhtml_entry.compressed_size,
                xhtml_entry.uncompressed_size
            );
            let spine_u16 = spine_index.min(u16::MAX as usize) as u16;
            // Stop *before* this item if it would overrun the slice. The check
            // after an item cannot bound a step -- it passes under budget and
            // the next item runs for seconds, which is what made page turns
            // wait up to 3.4 s on device. Nothing has been written for this
            // item yet, so the resume points *at* it rather than past it.
            if phase.suspend_before(
                open_started.elapsed().as_millis(),
                xhtml_entry.uncompressed_size,
                walked_this_step,
            ) {
                esp_println::println!(
                    "epub: slice yields before spine {} ({} B)",
                    spine_u16,
                    xhtml_entry.uncompressed_size
                );
                return Ok(Some(spine_u16));
            }
            walked_this_step += 1;
            let mut sink = LibraryBlockSink::begin_spine(
                &mut *library,
                root,
                sections_dir,
                source_identity,
                &mut *font_metrics,
                &mut *sections,
                &mut section_count,
                &mut total_pages,
                &mut book_partial,
                &mut section_write_micros,
                spine_u16,
                visible_page_capacity,
                generate_toc_from_headings,
            );
            // Stream the whole member through the resumable block parser in
            // bounded windows: spine XHTML of any size decodes completely, with
            // no 24 KB prefix truncation. The parser's in-body assumption is
            // sniffed from the first decoded window, mirroring the
            // whole-document contains() check.
            let mut capture_sink = CapturingBlockSink {
                inner: &mut sink,
                capture: &mut content,
                spine_index: spine_u16,
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser: Option<XhtmlBlockStreamParser> = None;
            let mut parse_error: Option<XhtmlError> = None;
            let read_result = zip.read_entry_to_sink(
                xhtml_entry,
                scratch.compressed,
                scratch.xhtml,
                &mut *scratch.zip_inflate,
                &mut |chunk| {
                    let parser = parser.get_or_insert_with(|| {
                        let has_body =
                            bytes_contain(chunk, b"<body") || bytes_contain(chunk, b":body");
                        XhtmlBlockStreamParser::new(!has_body)
                    });
                    tokenizer
                        .feed_xhtml_blocks(chunk, parser, Some(&css_rules), &mut capture_sink)
                        .map_err(|err| {
                            parse_error = Some(err);
                            proto::epub::ZipError::Inflate
                        })
                },
            );
            match read_result {
                Ok(()) => {
                    if let Some(parser) = parser.as_mut() {
                        if let Err(err) = tokenizer.finish_xhtml_blocks(parser, &mut capture_sink) {
                            if !capture_sink.inner.stopped {
                                return Err(err.into());
                            }
                        }
                    }
                }
                Err(_) if parse_error.is_some() => {
                    let err = parse_error.take().expect("parse error recorded");
                    if capture_sink.inner.stopped {
                        esp_println::println!(
                            "epub: bounded open stopped at spine {} after {} section(s): {:?}",
                            spine_index,
                            *capture_sink.inner.section_count,
                            err
                        );
                    } else {
                        return Err(err.into());
                    }
                }
                Err(err) => return Err(err.into()),
            }
            sink.finish_spine(false);
            content.spine_end(spine_u16);

            // The only place the walk may stop. Everything the sink held for
            // this spine item is flushed and the capture is framed, so the
            // section files on the card end exactly where `next_spine` says
            // the next step begins.
            if phase.suspend_here(
                total_pages,
                section_count,
                open_started.elapsed().as_millis(),
                spine_index + 1 < package.spine.len(),
            ) {
                return Ok(Some(spine_u16.saturating_add(1)));
            }
        }
        Ok::<Option<u16>, ReaderCacheError>(None)
    });

    if let Ok(Some(next_spine)) = walk {
        // Suspending, not finishing: flush what the capture staged but leave
        // its header incomplete for the next step to append to.
        let (content_ok, content_spine_count) = content.suspend();
        let mut state = BookBuildResume {
            index: catalog_index,
            source_identity,
            next_spine,
            section_count: section_count.min(u16::MAX as usize) as u16,
            total_pages,
            book_partial,
            generate_toc_from_headings,
            content_ok,
            content_spine_count,
            // Replaced below by whichever tail runs; a first open always
            // publishes, a continuation only past the batching threshold.
            published_sections: 0,
            layout: library.layout_key(),
        };
        return if resume.is_none() {
            publish::publish_first_open(
                root,
                owner,
                source_identity,
                requested_global_page,
                library,
                &sections[..section_count],
                total_pages,
                next_spine,
            )
            .map_err(ReaderCacheError::from)
            .inspect(|()| {
                bench_log!(
                    "bench: storage_first_page elapsed_ms={} pages={} sections={} key={}",
                    open_started.elapsed().as_millis(),
                    total_pages,
                    section_count,
                    owner.key
                );
            })
            .map(|()| {
                state.published_sections = state.section_count;
                Some(state)
            })
        } else {
            publish::extend_background_index(
                root,
                owner,
                source_identity,
                requested_global_page,
                next_spine,
                resume.map_or(0, |previous| previous.published_sections),
                library,
                &sections[..section_count],
                total_pages,
            )
            .map_err(ReaderCacheError::from)
            .inspect(|published| {
                esp_println::println!(
                    "epub: build continue next_spine={} sections={} published={} pages={} step_ms={}",
                    next_spine,
                    section_count,
                    published,
                    total_pages,
                    open_started.elapsed().as_millis(),
                );
            })
            .map(|published| {
                state.published_sections = published;
                Some(state)
            })
        };
    }

    // Keep CONT.BIN only when the walk captured the whole book: a partial
    // or failed capture is deleted so replay can never resurrect a
    // truncated stream.
    let content_kept = content.finish(
        content_dir.as_ref(),
        walk.is_ok() && !book_partial && section_count > 0 && total_pages > 0,
    );
    esp_println::println!("epub: content capture kept={}", content_kept);
    walk?;

    if section_count > 0 && total_pages > 0 {
        // A finished continuation republishes around the page the reader is
        // actually on, not the one the original open asked for — that page is
        // long since rendered and the reader has moved.
        let continuing = resume.is_some();
        let published = publish::publish_book_cache(
            root,
            owner,
            source_identity,
            requested_global_page,
            library,
            &sections[..section_count],
            total_pages,
            book_partial,
            // The walk reached the end of the spine, so nothing is coming back
            // for more; any remaining `book_partial` is permanent.
            0,
        );
        if published.outcome == publish::BookPublishOutcome::Ready {
            report_publish(
                owner.key,
                if continuing { "final" } else { "full" },
                open_started,
                spine_started,
                io_start,
                section_write_micros,
                published.cover,
                section_count,
                total_pages,
                book_partial,
            );
        }
        if continuing {
            // The last step of a background walk publishes under a reader who
            // is already inside this book, on a page served from these very
            // files. That makes both failures mean something different here
            // than they do on an open, so they get their own tail rather than
            // the one below, which is written for a book nobody is holding.
            return publish::finish_background_walk(
                root,
                owner,
                source_identity,
                requested_global_page,
                published.outcome,
                library,
            )
            .map(|()| None)
            .map_err(ReaderCacheError::from);
        }
        match published.outcome {
            publish::BookPublishOutcome::Ready => Ok(None),
            publish::BookPublishOutcome::SectionReadFailed => {
                // The build and index write succeeded; only the requested
                // section failed to read back. Keep the freshly written
                // cache — the next open retries the fast path against it —
                // and report the read failure rather than a bogus content
                // diagnosis.
                Err(ReaderCacheError::SectionRead)
            }
            publish::BookPublishOutcome::IndexWriteFailed => {
                // The index write failed partway, so BOOK.BIN may be a
                // truncated file that serves neither the fast path nor
                // replay (the labels load bails on it). Clear this layout's
                // debris so the next open rebuilds it cleanly. Safe only
                // because this arm is the *open* path: the book never became
                // readable, so nothing is holding these files.
                //
                // This layout's, not the book's. Another layout's pagination
                // is finished work that this failure says nothing about, and
                // CONT.BIN is settings-independent: taking it would turn the
                // next open from a replay into a full re-parse.
                let _ = files::empty_layout_cache(root, owner.key, library.layout_key());
                Err(ReaderCacheError::IndexWrite)
            }
        }
    } else if saw_spine {
        Err(ReaderCacheError::NoBodyText)
    } else {
        Err(ReaderCacheError::MissingSpine)
    }
}

/// Forwards `push_block` to the build sink while recording the exact call
/// into the content capture. Wrapping at the trait seam keeps the capture
/// out of `LibraryBlockSink`'s already-wide borrow set, and guarantees the
/// captured stream is byte-for-byte what the sink consumed.
struct CapturingBlockSink<
    'w,
    'v,
    's,
    S,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    S: XhtmlBlockSink,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    inner: &'w mut S,
    capture: &'w mut files::ContentCapture<'v, 's, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    spine_index: u16,
}

impl<S, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    XhtmlBlockSink for CapturingBlockSink<'_, '_, '_, S, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    S: XhtmlBlockSink,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
        paragraph_end: bool,
        logical_offset: u32,
    ) -> Result<(), XhtmlError> {
        self.capture.push_block_record(
            self.spine_index,
            text,
            role,
            style,
            align,
            paragraph_end,
            logical_offset,
        );
        self.inner
            .push_block(text, role, style, align, paragraph_end, logical_offset)
    }
}

/// Put a book back together from the section files one layout already has.
///
/// Runs between the fast path and the replay, on the route a typography flip
/// takes: the sections for this layout are still on the card and only the
/// index naming them is gone. A header read per section, against 25 s of
/// replay or a cold build's minute.
///
/// Labels and TOC come from the index being replaced, which holds them for the
/// book rather than for a layout. Without them the replay runs instead.
#[inline(never)]
fn try_reindex_layout<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    requested_page: u32,
    library: &mut ReaderStore,
    scratch: &mut ReaderCacheScratch<'_>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let started = Instant::now();
    if !files::load_v2_book_labels_and_toc(root, owner, source_identity, library) {
        return false;
    }
    let Some((count, total_pages)) = files::reindex_layout_from_sections(
        root,
        owner,
        source_identity,
        library,
        &mut scratch.book_sections[..],
    ) else {
        return false;
    };
    let sections = &scratch.book_sections[..count];
    library.begin_book_load();
    let published = publish::publish_book_cache(
        root,
        owner,
        source_identity,
        requested_page,
        library,
        sections,
        total_pages,
        false,
        0,
    );
    if published.outcome != publish::BookPublishOutcome::Ready {
        esp_println::println!("epub: reindex publish failed, falling back");
        return false;
    }
    library.finish_book_load(
        published.cover.map_or(0, |_| 0),
        0,
        reader_cache::store::BookLoadStatus::Ready,
    );
    esp_println::println!(
        "epub: reindexed {} section(s) for this layout in {} ms",
        count,
        started.elapsed().as_millis()
    );
    true
}

/// Rebuild the section cache by replaying CONT.BIN — the captured
/// `push_block` stream — through the same build sink, skipping the zip
/// read, inflate, and XML parse entirely. Runs when the v2 index missed
/// (typically a type-settings change); returns true when the book
/// published Ready. Any validation or framing failure deletes the file and
/// returns false, so the caller falls back to the full EPUB build.
#[inline(never)]
fn try_replay_content_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    requested_global_page: u32,
    library: &mut ReaderStore,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Why a replay attempt ended without publishing.
    enum ReplayFail {
        /// No usable attempt was possible (no file, or the old index's
        /// labels/TOC couldn't be recovered) — leave CONT.BIN alone.
        Bail,
        /// CONT.BIN exists but is stale, truncated, or corrupt: delete it.
        Corrupt,
    }

    let open_started = Instant::now();
    let io_start = crate::sd_session::sd_stats::snapshot();
    let spine_started = Instant::now();
    scratch.book_sections.fill(EMPTY_BOOK_SECTION_RECORD);
    let sections = &mut *scratch.book_sections;
    let xhtml_buf = &mut scratch.xhtml[..];
    let mut section_count = 0usize;
    let mut total_pages = 0u32;
    let mut book_partial = false;
    let mut section_write_micros: u64 = 0;

    let replayed = files::with_v2_sections_dir(root, owner, |sections_dir| {
        // One open serves both the header validation and the record
        // stream; the handle stays live for the whole replay.
        files::with_v2_content_file(root, owner, Mode::ReadOnly, |file| {
            let mut header_bytes = [0u8; CONTENT_HEADER_BYTES];
            if files::read_exact_file(file, &mut header_bytes).is_err() {
                return Err(ReplayFail::Corrupt);
            }
            let Ok(header) = proto::cache::decode_content_header(&header_bytes) else {
                return Err(ReplayFail::Corrupt);
            };
            if !header.complete
                || header.source_hash != source_identity.0
                || header.source_size != source_identity.1
            {
                // Incomplete or written for a different file under the
                // same key: dead weight.
                return Err(ReplayFail::Corrupt);
            }
            // The old BOOK.BIN is layout-invalid but its labels and TOC
            // copy are settings-independent; carry them into the rewritten
            // index. Bail to the full build when they can't be recovered —
            // it re-parses them.
            if !files::load_v2_book_labels_and_toc(root, owner, source_identity, library) {
                return Err(ReplayFail::Bail);
            }
            library.clear_cover();
            esp_println::println!("epub: stage ReplayContentCache");
            let visible_page_capacity = library.page_capacity().max(1);
            let generate_toc_from_headings = library.toc_count() == 0;
            replay_content_records(
                file,
                root,
                sections_dir,
                source_identity,
                library,
                font_metrics,
                sections,
                &mut section_count,
                &mut total_pages,
                &mut book_partial,
                &mut section_write_micros,
                xhtml_buf,
                visible_page_capacity,
                generate_toc_from_headings,
                header.spine_count,
                header.content_len,
            )
            .map_err(|()| ReplayFail::Corrupt)
        })
        .unwrap_or(Err(ReplayFail::Bail))
    });

    match replayed {
        Err(ReplayFail::Bail) => return false,
        Err(ReplayFail::Corrupt) => {
            esp_println::println!("epub: content replay failed, falling back to full build");
            files::delete_v2_content_file(root, owner);
            return false;
        }
        Ok(()) if section_count == 0 || total_pages == 0 => {
            esp_println::println!("epub: content replay failed, falling back to full build");
            files::delete_v2_content_file(root, owner);
            return false;
        }
        Ok(()) => {}
    }
    let published = publish::publish_book_cache(
        root,
        owner,
        source_identity,
        requested_global_page,
        library,
        &sections[..section_count],
        total_pages,
        book_partial,
        // Replay is never progressive; it always publishes a whole book.
        0,
    );
    if published.outcome == publish::BookPublishOutcome::Ready {
        report_publish(
            owner.key,
            "replay",
            open_started,
            spine_started,
            io_start,
            section_write_micros,
            published.cover,
            section_count,
            total_pages,
            book_partial,
        );
    }
    if published.outcome != publish::BookPublishOutcome::Ready {
        // Either failure leaves this layout's cache state unusable (a
        // truncated BOOK.BIN or an unreadable section); the full build
        // rewrites it either way. Scoped to the layout: the full build that
        // follows reads CONT.BIN, and another layout's pagination is not
        // implicated in this one's failure.
        esp_println::println!("epub: content replay publishing failed, falling back to full build");
        let _ = files::empty_layout_cache(root, owner.key, library.layout_key());
        return false;
    }
    true
}

/// Stream CONT.BIN's records into fresh `LibraryBlockSink` runs, one per
/// captured spine item, mirroring the full build's spine loop. The framing
/// discipline — marker-terminated groups, no mid-group spine change,
/// clean EOF only after every expected spine — is owned and enforced by
/// `proto::cache::replay_content_stream`, which the host regression tests
/// drive directly; this function only supplies the file reader and the
/// per-group sink runs.
#[expect(clippy::too_many_arguments)] // Intentionally retained wide signature to pass borrow-checked references and layout settings directly without struct wrapping
fn replay_content_records<
    'r,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    sections_dir: Option<&'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    sections: &mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    section_count: &mut usize,
    total_pages: &mut u32,
    book_partial: &mut bool,
    write_micros: &mut u64,
    buf: &mut [u8],
    visible_page_capacity: usize,
    generate_toc_from_headings: bool,
    expected_spines: u16,
    expected_len: u32,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if file.length() != expected_len {
        return Err(());
    }
    if file.seek_from_start(CONTENT_HEADER_BYTES as u32).is_err() {
        return Err(());
    }
    let mut read = |dst: &mut [u8]| file.read(dst).map_err(|_| proto::cache::ContentReplayError);
    let mut on_group =
        |group_spine: u16,
         group: &mut proto::cache::ContentGroupReader<'_>|
         -> Result<proto::cache::ContentReplayFlow, proto::cache::ContentReplayError> {
            if *section_count >= sections.len() {
                *book_partial = true;
                return Ok(proto::cache::ContentReplayFlow::Stop);
            }
            let mut sink = LibraryBlockSink::begin_spine(
                &mut *library,
                root,
                sections_dir,
                source_identity,
                &mut *font_metrics,
                &mut *sections,
                &mut *section_count,
                &mut *total_pages,
                &mut *book_partial,
                &mut *write_micros,
                group_spine,
                visible_page_capacity,
                generate_toc_from_headings,
            );
            while let Some((record, text)) = group.next_block()? {
                if sink
                    .push_block(
                        text,
                        record.role,
                        record.style,
                        record.align,
                        record.paragraph_end,
                        record.logical_offset,
                    )
                    .is_err()
                {
                    if sink.stopped {
                        // Section capacity exhausted at these settings — the
                        // same stop a full build takes; publish partial.
                        sink.finish_spine(false);
                        return Ok(proto::cache::ContentReplayFlow::Stop);
                    }
                    return Err(proto::cache::ContentReplayError);
                }
            }
            sink.finish_spine(false);
            Ok(proto::cache::ContentReplayFlow::Continue)
        };
    proto::cache::replay_content_stream(&mut read, buf, expected_spines, &mut on_group)
        .map(|_| ())
        .map_err(|_| ())
}

fn spine_item_is_navigation(
    item: &proto::epub::SpineItem,
    package: &proto::epub::EpubPackage<'_>,
) -> bool {
    let opf = package.opf_text;
    let href = item.href.of(opf);
    let lower_href = LowerAscii::<160>::new(href);
    let lower_props = LowerAscii::<96>::new(item.properties.of(opf));
    item.media_type.of(opf) == "application/x-dtbncx+xml"
        || package.nav_href.map(|nav| nav == href).unwrap_or(false)
        || package.ncx_href.map(|ncx| ncx == href).unwrap_or(false)
        || lower_props.word_eq("nav")
        || lower_href.ends_with("toc.xhtml")
        || lower_href.ends_with("toc.html")
        || lower_href.ends_with("nav.xhtml")
        || lower_href.ends_with("nav.html")
}

fn inferred_start_spine_index(package: &proto::epub::EpubPackage<'_>) -> usize {
    if package.spine.len() <= 1 {
        return 0;
    }
    let Some(first) = package.spine.first() else {
        return 0;
    };
    let lower_href = LowerAscii::<MAX_ENTRY_NAME_BYTES>::new(first.href.of(package.opf_text));
    if lower_href.contains("titlepage")
        || lower_href.contains("title-page")
        || lower_href.contains("cover")
    {
        1
    } else {
        0
    }
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn load_epub_toc<Z>(
    zip: &mut Z,
    opf_path: &str,
    package: &proto::epub::EpubPackage<'_>,
    library: &mut ReaderStore,
    toc_buf: &mut [u8],
    scratch: TocScratch<'_>,
) -> usize
where
    Z: EpubZipOps,
{
    library.clear_toc();
    // Small reusable window for inflate output. The streaming tokenizer
    // consumes these chunks incrementally and never needs the whole TOC
    // resident — so any-size book is fine.
    let mut output_window = [0u8; 512];

    for toc_href in [package.nav_href, package.ncx_href].into_iter().flatten() {
        let mut toc_path = String::<MAX_ENTRY_NAME_BYTES>::new();
        if resolve_epub_href(opf_path, toc_href, &mut toc_path).is_err() {
            continue;
        }
        let Ok(toc_entry) = zip.find_entry(&toc_path, scratch.header, scratch.name) else {
            continue;
        };
        esp_println::println!(
            "epub: toc entry {} compressed={} uncompressed={}",
            toc_path.as_str(),
            toc_entry.compressed_size,
            toc_entry.uncompressed_size
        );

        let mut sink = LibraryTocSink {
            library: &mut *library,
            package,
            toc_buf: &mut *toc_buf,
            record_count: 0,
            resident_full: false,
        };
        let mut tokenizer = StreamingXmlTokenizer::new();
        let is_ncx = toc_path.as_str().ends_with(".ncx");
        let parse_ok = if is_ncx {
            let mut parser = NcxStreamParser::new();
            let feed_result = zip.read_entry_to_sink(
                toc_entry,
                scratch.compressed,
                &mut output_window,
                scratch.zip_inflate,
                &mut |chunk| {
                    tokenizer
                        .feed_ncx(chunk, &mut parser, &mut sink)
                        .map_err(|_| proto::epub::ZipError::Inflate)
                },
            );
            feed_result.is_ok() && tokenizer.finish_ncx(&mut parser, &mut sink).is_ok()
        } else {
            let mut parser = Epub3NavStreamParser::new();
            let feed_result = zip.read_entry_to_sink(
                toc_entry,
                scratch.compressed,
                &mut output_window,
                scratch.zip_inflate,
                &mut |chunk| {
                    tokenizer
                        .feed_nav(chunk, &mut parser, &mut sink)
                        .map_err(|_| proto::epub::ZipError::Inflate)
                },
            );
            feed_result.is_ok() && tokenizer.finish_nav(&mut parser, &mut sink).is_ok()
        };

        if parse_ok && (sink.record_count > 0 || sink.library.toc_count() > 0) {
            esp_println::println!(
                "epub: toc streamed {} chapter(s) ({} resident) from {} overflow={}",
                sink.record_count,
                sink.library.toc_count(),
                toc_path.as_str(),
                sink.resident_full
            );
            return sink.record_count;
        }
        esp_println::println!("epub: toc parse failed for {}", toc_path.as_str());
        sink.library.clear_toc();
    }
    esp_println::println!("epub: toc unavailable, chapters fall back to spine");
    0
}

fn href_matches_spine(href: &str, spine_href: &str) -> bool {
    let href = strip_fragment(href);
    href == spine_href
        || href.ends_with(spine_href)
        || spine_href.ends_with(href)
        || file_name(href) == file_name(spine_href)
}

fn file_name(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

struct SdFileReadAt<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    len: u32,
    read_ops: u32,
    read_bytes: u32,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> ReadAt
    for SdFileReadAt<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    type Error = ();

    fn len(&mut self) -> Result<u32, Self::Error> {
        Ok(self.len)
    }

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
        if self.read_ops >= EPUB_OPEN_READ_OP_LIMIT || self.read_bytes >= EPUB_OPEN_READ_BYTE_LIMIT
        {
            esp_println::println!(
                "epub: open read budget exceeded ops={} bytes={} at offset={} request={}",
                self.read_ops,
                self.read_bytes,
                offset,
                out.len()
            );
            return Err(());
        }
        let requested = out.len();
        let remaining_budget = EPUB_OPEN_READ_BYTE_LIMIT.saturating_sub(self.read_bytes) as usize;
        let read_len = requested
            .min(EPUB_READ_AT_CHUNK_BYTES)
            .min(remaining_budget);
        if read_len == 0 {
            return Err(());
        }
        let mut last_err = None;
        for attempt in 0..3 {
            if let Err(err) = self.file.seek_from_start(offset) {
                last_err = Some(err);
                continue;
            }
            match self.file.read(&mut out[..read_len]) {
                Ok(count) => {
                    self.read_ops = self.read_ops.saturating_add(1);
                    self.read_bytes = self.read_bytes.saturating_add(count as u32);
                    if attempt > 0 {
                        esp_println::println!(
                            "epub: read_at recovered at {} len {} attempt {}",
                            offset,
                            read_len,
                            attempt + 1
                        );
                    }
                    return Ok(count);
                }
                Err(err) => {
                    last_err = Some(err);
                    for _ in 0..128 {
                        core::hint::spin_loop();
                    }
                }
            }
        }
        let err = last_err.expect("read_at records an error before retry exhaustion");
        esp_println::println!(
            "epub: read_at failed at {} len {}: {:?}",
            offset,
            read_len,
            err
        );
        Err(())
    }
}

fn resolve_epub_href(
    opf_path: &str,
    href: &str,
    out: &mut String<MAX_ENTRY_NAME_BYTES>,
) -> Result<(), ReaderCacheError> {
    out.clear();
    if href.starts_with('/') {
        out.push_str(href.trim_start_matches('/'))
            .map_err(|_| ReaderCacheError::EntryNameTooLong)?;
        return Ok(());
    }
    if let Some((dir, _)) = opf_path.rsplit_once('/') {
        out.push_str(dir)
            .and_then(|_| out.push('/'))
            .map_err(|_| ReaderCacheError::EntryNameTooLong)?;
    }
    let href_no_fragment = href.split('#').next().unwrap_or(href);
    out.push_str(href_no_fragment)
        .map_err(|_| ReaderCacheError::EntryNameTooLong)
}

struct LibraryBlockSink<
    'a,
    'r,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    library: &'a mut ReaderStore,
    root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    /// The book's SECTIONS directory, opened once for the whole build.
    /// `None` when the cache directories could not be created — section
    /// writes then fail per section and the book publishes as partial.
    sections_dir: Option<&'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    source_identity: (u32, u32),
    font_metrics: &'a mut crate::custom_font::MetricCache,
    sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    section_count: &'a mut usize,
    total_pages: &'a mut u32,
    book_partial: &'a mut bool,
    /// Accumulated wall time spent writing section files, for the
    /// `bench: storage_build` line.
    write_micros: &'a mut u64,
    spine_index: u16,
    line: String<MAX_READER_BLOCK_TEXT>,
    /// Running ink width of `line`. `line` always starts with a style
    /// marker (or is empty), so the cursor's default font never shows
    /// through and the running width matches a from-scratch measure.
    line_ink: LineInkCursor,
    line_role: TextRole,
    line_align: TextAlign,
    line_style: FontStyle,
    pending_space: bool,
    dropping_paragraph: bool,
    stopped: bool,
    target_pages: usize,
    generate_toc_from_headings: bool,
    generated_toc_for_spine: bool,
    /// Incremental page-index cursor: each appended line advances the page
    /// records in O(1) instead of triggering a full `rebuild_page_index`
    /// re-walk of every accumulated block — O(blocks²) per section before.
    /// Hard invariant: the records must stay bit-identical to a full
    /// rebuild (they persist into section files), which the shared cursor's
    /// host tests check step-by-step against the naive walk. The carry path
    /// falls back to a full rebuild and re-adopts its cursor.
    page_cursor: ui::reading::PageIndexCursor,
    /// Latched when a page record was dropped past the `pages` capacity,
    /// mirroring the full rebuild's silent drop.
    page_overflowed: bool,
    /// Where the parser block being consumed starts in the spine item's
    /// logical content stream, and how long it is.
    block_offset: u32,
    block_len: u32,
    /// How much of that block the lines have taken, counted as the words go
    /// by: the text reaching this sink is decoded and normalized, so it no
    /// longer indexes the parser's bytes. Clamped to the block, which keeps
    /// every anchor inside the block it came from and in order.
    block_consumed: u32,
    /// Where the line being accumulated began. Becomes the anchor of a page
    /// when this line is the one that opens it.
    line_offset: u32,
}

impl<'a, 'r, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    LibraryBlockSink<'a, 'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Fresh sink state for one spine-item run. Both the full EPUB build
    /// and the CONT.BIN replay construct their runs here, so the two paths
    /// start from identical state by construction — replay must reproduce
    /// the full build's layout exactly.
    #[expect(clippy::too_many_arguments)] // The sink's borrow set: counters and stores that must stay caller-owned references, not a wrapper struct built per spine item
    fn begin_spine(
        library: &'a mut ReaderStore,
        root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        sections_dir: Option<&'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        source_identity: (u32, u32),
        font_metrics: &'a mut crate::custom_font::MetricCache,
        sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
        section_count: &'a mut usize,
        total_pages: &'a mut u32,
        book_partial: &'a mut bool,
        write_micros: &'a mut u64,
        spine_index: u16,
        target_pages: usize,
        generate_toc_from_headings: bool,
    ) -> Self {
        library.clear_lines();
        let type_settings = library.type_settings();
        let page_box = library.page_box();
        Self {
            library,
            root,
            sections_dir,
            source_identity,
            font_metrics,
            sections,
            section_count,
            total_pages,
            book_partial,
            write_micros,
            spine_index,
            line: String::new(),
            line_ink: LineInkCursor::new(type_settings, FontStyle::Regular),
            line_role: TextRole::Body,
            line_align: TextAlign::Justify,
            line_style: FontStyle::Regular,
            pending_space: false,
            dropping_paragraph: false,
            stopped: false,
            target_pages,
            generate_toc_from_headings,
            generated_toc_for_spine: false,
            block_offset: 0,
            block_len: 0,
            block_consumed: 0,
            line_offset: 0,
            page_cursor: ui::reading::PageIndexCursor::start(page_box),
            page_overflowed: false,
        }
    }
}

#[derive(Clone, Copy)]
enum LineInkCursor {
    BuiltIn(StyledInkCursor),
    Custom(CustomLineInkCursor),
}

impl LineInkCursor {
    fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        if settings.family == FontFamily::Custom && !display::font::builtin_custom_available() {
            Self::Custom(CustomLineInkCursor::new(settings, default_style))
        } else {
            Self::BuiltIn(StyledInkCursor::new(settings, default_style))
        }
    }

    fn width(&self) -> i16 {
        match self {
            Self::BuiltIn(cursor) => cursor.width(),
            Self::Custom(cursor) => cursor.width(),
        }
    }
}

#[derive(Clone, Copy)]
struct CustomLineInkCursor {
    advance_fp: i32,
    right: i16,
    settings: TypeSettings,
    style: FontStyle,
}

impl CustomLineInkCursor {
    const fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        Self {
            advance_fp: 0,
            right: 0,
            settings,
            style: default_style,
        }
    }

    fn reset_pair(&mut self) {}

    fn push_metric(&mut self, metric: display::font::GlyphMetric) {
        let advance = fixed_round(self.advance_fp);
        let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance_fp += metric.advance_fp as i32;
    }

    fn push_fallback(&mut self) {
        self.advance_fp += 8 << 4;
        self.right = self.right.max(fixed_ceil(self.advance_fp));
    }

    fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn reset_line_ink(&mut self) {
        self.line_ink = LineInkCursor::new(self.library.type_settings(), FontStyle::Regular);
    }

    fn push_line_ink_str(&mut self, text: &str) {
        match &mut self.line_ink {
            LineInkCursor::BuiltIn(cursor) => cursor.push_str(text),
            LineInkCursor::Custom(cursor) => {
                crate::custom_font::for_each_metric(
                    self.root,
                    self.library,
                    &mut *self.font_metrics,
                    cursor.settings.size,
                    cursor.settings.weight,
                    cursor.style,
                    text,
                    |style, metric| {
                        if cursor.style != style {
                            cursor.reset_pair();
                            cursor.style = style;
                        }
                        if let Some(metric) = metric {
                            cursor.push_metric(metric);
                        } else {
                            cursor.push_fallback();
                        }
                    },
                );
            }
        }
    }

    fn line_ink_width(&self) -> i16 {
        self.line_ink.width()
    }

    /// Mirror the block just appended (at `block_count - 1`) into the page
    /// index through the shared incremental cursor — O(1) per line.
    fn note_block_appended(&mut self) {
        let index = self.library.block_count() - 1;
        let pages_before = self.library.page_count();
        self.page_overflowed |=
            layout::place_appended_block(self.library, &mut self.page_cursor, index);
        if self.library.page_count() > pages_before {
            // This line opened the page, so where the line began is where the
            // page begins.
            self.library.set_last_page_offset(self.line_offset);
        }
    }

    /// Take one word into the line in progress.
    fn note_word_placed(&mut self, word_len: usize, line_was_empty: bool) {
        if line_was_empty {
            self.line_offset = self.block_offset.saturating_add(self.block_consumed);
        }
        let taken = (word_len as u32).saturating_add(1);
        self.block_consumed = self
            .block_consumed
            .saturating_add(taken)
            .min(self.block_len);
    }

    /// Bounded fix-up for `mark_last_block_paragraph_end`: the mark grows
    /// the last block by its trailing paragraph gap after placement, so
    /// re-place just that block; when it no longer fits its page, move it
    /// to a fresh one, exactly as a full rebuild would.
    fn note_last_block_grew(&mut self, index: usize) {
        self.page_overflowed |=
            layout::replace_last_block(self.library, &mut self.page_cursor, index);
    }

    fn reset_page_cursor(&mut self) {
        self.page_cursor = ui::reading::PageIndexCursor::start(self.library.page_box());
        self.page_overflowed = false;
    }

    fn finish_spine(&mut self, partial: bool) {
        flush_styled_preview_line(self, true);
        self.flush_section(partial || self.stopped, false);
    }

    fn flush_section(&mut self, partial: bool, carry_incomplete: bool) -> bool {
        if self.library.block_count() == 0 || self.library.page_count() == 0 {
            self.library.clear_lines();
            self.reset_page_cursor();
            return true;
        }
        if *self.section_count >= self.sections.len() {
            *self.book_partial = true;
            self.stopped = true;
            return false;
        }
        if partial {
            *self.book_partial = true;
        }

        // Intermediate sections end on a whole page: the half-finished final
        // page carries into the next section rather than being written as a
        // short, half-empty page the reader would stop on mid-chapter. The
        // last section of a chapter (finish_spine) keeps its trailing page —
        // that is the genuine end of the text.
        let full_blocks = self.library.block_count();
        let full_pages = self.library.page_count();
        let carry_first = if carry_incomplete && full_pages > 1 {
            let cut = self.library.page_first_block(full_pages - 1);
            (cut > 0 && cut < full_blocks).then_some(cut)
        } else {
            None
        };
        let trimmed =
            carry_first.map(|cut| self.library.trim_to_page_boundary(cut, full_pages - 1));

        self.library.set_cached_spine(self.spine_index);
        self.library.set_section_partial(partial);
        let section_id = (*self.section_count).min(u16::MAX as usize) as u16;
        let write_started = Instant::now();
        let wrote = match self.sections_dir {
            Some(sections) => files::write_v2_section_cache_in(
                sections,
                self.source_identity,
                section_id,
                self.library,
            ),
            None => false,
        };
        *self.write_micros += write_started.elapsed().as_micros();
        if !wrote {
            *self.book_partial = true;
        }
        self.sections[*self.section_count] = BookV2SectionRecord {
            section: section_id,
            spine: self.spine_index,
            start_page: *self.total_pages,
            page_count: self.library.page_count().min(u16::MAX as usize) as u16,
            partial,
            // Where this section opens, taken from the page that opens it.
            logical_offset: self
                .library
                .page_anchor(0)
                .map_or(0, |anchor| anchor.offset),
        };
        *self.total_pages = (*self.total_pages).saturating_add(self.library.page_count() as u32);
        *self.section_count += 1;

        match carry_first {
            Some(cut) => {
                if let Some(tail) = trimmed {
                    self.library.restore_trimmed(tail);
                }
                self.library.carry_last_page(cut);
                // Full rebuild on the carry path: the carried blocks were
                // rebased, so re-derive the page records and adopt the
                // walk's cursor for the appends that follow.
                let (cursor, overflowed) = layout::rebuild_page_index(self.library);
                self.page_cursor = cursor;
                self.page_overflowed = overflowed;
            }
            None => {
                self.library.clear_lines();
                self.reset_page_cursor();
            }
        }
        true
    }

    fn flush_if_full(&mut self) {
        if self.library.page_count() >= self.target_pages
            || self.library.block_count() >= self.library.block_capacity().saturating_sub(4)
            || self.library.text_capacity_reached()
        {
            flush_styled_preview_line(self, false);
            self.flush_section(false, true);
        }
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> XhtmlBlockSink
    for LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
        paragraph_end: bool,
        logical_offset: u32,
    ) -> Result<(), XhtmlError> {
        if self.stopped {
            return Err(XhtmlError::TooManyRuns);
        }
        self.flush_if_full();
        self.block_offset = logical_offset;
        self.block_len = text.len() as u32;
        self.block_consumed = 0;
        push_styled_preview_fragment(
            self,
            text,
            preview_style_for_proto_style(style, role),
            role,
            align,
            paragraph_end,
        );
        self.flush_if_full();
        Ok(())
    }
}

fn preview_style_for_proto_style(style: proto::text::FontStyle, role: TextRole) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => {
            if matches!(
                role,
                TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
            ) {
                FontStyle::Bold
            } else {
                FontStyle::Regular
            }
        }
    }
}

fn push_styled_preview_fragment<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    text: &str,
    style: FontStyle,
    role: TextRole,
    align: TextAlign,
    paragraph_end: bool,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.dropping_paragraph {
        if paragraph_end {
            sink.dropping_paragraph = false;
            sink.pending_space = false;
        }
        return;
    }
    let starts_with_space = text
        .chars()
        .next()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let ends_with_space = text
        .chars()
        .next_back()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let mut normalized = String::<MAX_READER_BLOCK_TEXT>::new();
    push_normalized_decoded(text, &mut normalized);
    trim_trailing_space(&mut normalized);
    if !sanitize_preview_block(&mut normalized) {
        sink.dropping_paragraph = !paragraph_end;
        sink.pending_space = false;
        // A rejected block still ends its paragraph when it says it does.
        // `</p>` after a style run flushes an *empty* block carrying
        // `paragraph_end`, and sanitizing rejects empty text, so returning
        // here dropped the only terminator the paragraph had and the next one
        // ran into it. The `normalized.is_empty()` arm below already flushes
        // for exactly this reason; it just never sees empty text, because
        // this arm claims it first.
        if paragraph_end {
            flush_styled_preview_line(sink, true);
        }
        return;
    }
    if normalized.is_empty() {
        sink.pending_space |= starts_with_space || ends_with_space;
        if paragraph_end {
            flush_styled_preview_line(sink, true);
        }
        return;
    }

    normalize_decorative_separator(&mut normalized);
    let align = block_align_for(align, normalized.as_str(), role);
    let page_box = sink.library.page_box();
    let x = page_box.x_for(role);
    let max_x = page_box.right;
    // Book-style first-line indent: a Body paragraph's opening line wraps
    // against a narrower column. Only Left/Justify Body takes it, matching
    // `ui::reading::block_first_line_indent`, so the built line breaks agree
    // with how the page later draws them.
    let indent = if matches!(role, TextRole::Body)
        && matches!(align, TextAlign::Left | TextAlign::Justify)
    {
        layout::paragraph_indent(sink.library.type_settings().size)
    } else {
        0
    };

    if !sink.line.is_empty() && (sink.line_role != role || sink.line_align != align) {
        flush_styled_preview_line(sink, false);
    }
    if sink.line.is_empty() {
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = FontStyle::Regular;
    }

    let mut first_word = true;
    for word in normalized.split_whitespace() {
        let attach = is_leading_punctuation_word(word) && !sink.line.is_empty();
        let leading_space = !sink.line.is_empty()
            && !attach
            && (sink.pending_space || !first_word || starts_with_space);
        let kept_len = sink.line.len();
        let kept_ink = sink.line_ink;
        let line_was_empty = sink.line.is_empty();
        if append_styled_word(&mut sink.line, word, style, sink.line_style, leading_space).is_err()
        {
            sink.line.truncate(kept_len);
            flush_styled_preview_line(sink, false);
            let _ = append_styled_word(&mut sink.line, word, style, sink.line_style, false);
            let mut measure = String::<MAX_READER_BLOCK_TEXT>::new();
            let _ = measure.push_str(sink.line.as_str());
            sink.push_line_ink_str(measure.as_str());
            sink.note_word_placed(word.len(), true);
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
            first_word = false;
            continue;
        }
        let mut measure = String::<MAX_READER_BLOCK_TEXT>::new();
        let _ = measure.push_str(&sink.line[kept_len..]);
        sink.push_line_ink_str(measure.as_str());
        sink.note_word_placed(word.len(), line_was_empty);

        // The line in progress opens a paragraph while no line of it has
        // flushed yet: the previous block still closes a paragraph. Once the
        // first line flushes (as a non-end block) this goes false and the
        // continuation lines wrap at the full width.
        let opens_paragraph = sink.library.block_count() == 0
            || sink
                .library
                .block_paragraph_end
                .get(sink.library.block_count().wrapping_sub(1))
                .copied()
                .unwrap_or(true);
        let x_eff = if opens_paragraph { x + indent } else { x };
        if !line_was_empty && sink.line_ink_width() + x_eff + layout::READER_WRAP_SAFETY > max_x {
            sink.line.truncate(kept_len);
            sink.line_ink = kept_ink;
            flush_styled_preview_line(sink, false);
            let _ = append_styled_word(&mut sink.line, word, style, sink.line_style, false);
            let mut measure = String::<MAX_READER_BLOCK_TEXT>::new();
            let _ = measure.push_str(sink.line.as_str());
            sink.push_line_ink_str(measure.as_str());
            sink.note_word_placed(word.len(), true);
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
        } else {
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
        }
        first_word = false;
    }

    sink.pending_space |= ends_with_space;
    if paragraph_end {
        flush_styled_preview_line(sink, true);
    }
}

fn append_styled_word<const N: usize>(
    line: &mut String<N>,
    word: &str,
    style: FontStyle,
    current_style: FontStyle,
    leading_space: bool,
) -> Result<(), ()> {
    if leading_space {
        line.push(' ').map_err(|_| ())?;
    }
    // Emit a style marker only when the run actually changes. Plain prose
    // (the bulk of a book) then carries no markers at all -- ~25-30% more
    // text per section and proportionally fewer chunks against the same
    // 16 KB arena. The draw and measure paths both key off the running
    // font, so a dropped redundant marker is a no-op; and because each line
    // draws from a Regular default and `current_style` is reset to Regular
    // on every flush, a continuation line still re-marks a non-Regular
    // opening word.
    if style != current_style {
        append_style_marker(line, style)?;
    }
    line.push_str(word).map_err(|_| ())
}

fn append_style_marker<const N: usize>(line: &mut String<N>, style: FontStyle) -> Result<(), ()> {
    line.push(layout::STYLE_MARKER).map_err(|_| ())?;
    line.push(layout::style_marker_code(style)).map_err(|_| ())
}

fn flush_styled_preview_line<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    paragraph_end: bool,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.line.is_empty() {
        if paragraph_end {
            let count = sink.library.block_count();
            let changed = count > 0 && !sink.library.block_paragraph_end[count - 1];
            sink.library.mark_last_block_paragraph_end();
            if changed {
                // Height-only change: the trailing paragraph gap grew the
                // last block after it was placed, so re-place just that
                // block instead of rebuilding the whole page index.
                sink.note_last_block_grew(count - 1);
            }
        }
        return;
    }

    let line = sink.line.clone();
    let role = sink.line_role;
    let align = sink.line_align;
    let style = layout::first_styled_line_style(line.as_str()).unwrap_or(FontStyle::Regular);
    if sink.generate_toc_from_headings
        && !sink.generated_toc_for_spine
        && matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
    {
        let mut title = String::<160>::new();
        push_plain_styled_text(line.as_str(), &mut title);
        trim_trailing_space(&mut title);
        if !title.is_empty()
            && sink.library.push_toc_record(
                title.as_str(),
                heading_toc_level(role),
                sink.spine_index as i16,
            )
        {
            sink.generated_toc_for_spine = true;
        }
    }
    let mut appended_from = sink.library.block_count();
    if !sink.library.push_line_block(
        line.as_str(),
        style,
        role,
        align,
        paragraph_end,
        sink.spine_index,
    ) {
        // The section arena (text bytes or the block table) just filled.
        // Flush what we have to a section file and retry the line into a
        // fresh arena, so a long chapter chunks and continues instead of
        // losing its tail. (At the book-wide section ceiling flush_section
        // refuses and sets book_partial; the line is then genuinely
        // dropped, which is the separate whole-book limit.)
        sink.flush_section(false, true);
        appended_from = sink.library.block_count();
        let _ = sink.library.push_line_block(
            line.as_str(),
            style,
            role,
            align,
            paragraph_end,
            sink.spine_index,
        );
    }
    if sink.library.block_count() > appended_from {
        // push_line_block can also no-op (empty trim, full block table), so
        // only an actual append advances the page cursor.
        sink.note_block_appended();
    }
    sink.line.clear();
    sink.reset_line_ink();
    sink.line_style = FontStyle::Regular;
    sink.pending_space = false;
}

fn heading_toc_level(role: TextRole) -> u8 {
    match role {
        TextRole::Heading1 => 1,
        TextRole::Heading2 => 2,
        TextRole::Heading3 => 3,
        TextRole::Body | TextRole::BlockQuote => 1,
    }
}

fn push_plain_styled_text<const N: usize>(styled: &str, out: &mut String<N>) {
    let mut skip_style_code = false;
    for ch in styled.chars() {
        if skip_style_code {
            skip_style_code = false;
            continue;
        }
        if ch == layout::STYLE_MARKER {
            skip_style_code = true;
            continue;
        }
        let _ = out.push(ch);
    }
}

fn is_leading_punctuation_word(word: &str) -> bool {
    word.chars()
        .next()
        .map(|ch| {
            matches!(
                ch,
                ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{2019}' | '\u{201D}'
            )
        })
        .unwrap_or(false)
}

fn block_align_for(run_align: TextAlign, block: &str, role: TextRole) -> TextAlign {
    if run_align == TextAlign::Center
        || matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
        || is_decorative_separator(block)
    {
        TextAlign::Center
    } else {
        run_align
    }
}

fn normalize_decorative_separator<const N: usize>(block: &mut String<N>) {
    if !is_decorative_separator(block.as_str()) {
        return;
    }
    block.clear();
    let _ = block.push_str("* * *");
}

fn is_decorative_separator(text: &str) -> bool {
    let mut saw_mark = false;
    let mut mark_count = 0u8;
    for ch in text.chars() {
        if ch == '*' {
            saw_mark = true;
            mark_count = mark_count.saturating_add(1);
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        return false;
    }
    saw_mark && mark_count >= 3
}

fn push_normalized_decoded<const N: usize>(text: &str, out: &mut String<N>) {
    let mut previous_space = true;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if let Some(decoded) = decode_html_entity(rest) {
            if decoded.is_whitespace() {
                if !previous_space && out.push(' ').is_err() {
                    break;
                }
                previous_space = true;
            } else if push_normalized_char(decoded, out).is_err() {
                break;
            } else {
                previous_space = false;
            }
            cursor += rest.find(';').map(|index| index + 1).unwrap_or(1);
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            if !previous_space && out.push(' ').is_err() {
                break;
            }
            previous_space = true;
        } else if push_normalized_char(ch, out).is_err() {
            break;
        } else {
            previous_space = false;
        }
        cursor += ch.len_utf8();
    }
}

fn push_normalized_char<const N: usize>(ch: char, out: &mut String<N>) -> Result<(), ()> {
    match ch {
        '\u{00A0}' => out.push(' ').map_err(|_| ()),
        ch if ch as u32 <= u16::MAX as u32 => out.push(ch).map_err(|_| ()),
        _ => out.push('?').map_err(|_| ()),
    }
}

fn is_epub_titlepage_label(text: &str) -> bool {
    let lower = LowerAscii::<128>::new(text);
    lower.starts_with(": ")
        || lower.eq("title")
        || lower.eq("author")
        || lower.eq("creator")
        || lower.eq("language")
        || lower.eq("english")
        || lower.eq("english:")
        || lower.eq("release date")
        || lower.eq("original publication")
        || lower.starts_with("most recently updated")
        || lower.starts_with("other information")
        || lower.starts_with("other formats")
        || lower.starts_with("credits")
        || lower.starts_with("produced by")
        || lower.starts_with("transcribed from")
        || lower.starts_with("project gutenberg")
        || lower.starts_with("the project gutenberg")
}

fn sanitize_preview_block<const N: usize>(block: &mut String<N>) -> bool {
    trim_trailing_space(block);
    trim_leading_space(block);
    if block.is_empty() {
        return false;
    }
    if is_epub_titlepage_label(block) || contains_gutenberg_metadata(block.as_str()) {
        return false;
    }
    if is_decorative_separator(block.as_str()) {
        normalize_decorative_separator(block);
        return true;
    }
    if let Some(rest) = decorative_prefix_rest(block.as_str()) {
        if rest.is_empty() {
            normalize_decorative_separator(block);
            return true;
        }
        if is_epub_titlepage_label(rest) || contains_gutenberg_metadata(rest) {
            return false;
        }
    }
    true
}

fn decorative_prefix_rest(text: &str) -> Option<&str> {
    let mut mark_count = 0u8;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '*' {
            mark_count = mark_count.saturating_add(1);
            end = index + ch.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if mark_count >= 3 {
        Some(text[end..].trim())
    } else {
        None
    }
}

fn contains_gutenberg_metadata(text: &str) -> bool {
    let lower = LowerAscii::<160>::new(text);
    lower.contains("most recently updated")
        || lower.contains("project gutenberg ebook")
        || lower.contains("start of the project gutenberg")
        || lower.contains("end of the project gutenberg")
        || lower.contains("other information and formats")
        || lower.contains("this ebook is for the use of anyone")
        || lower.contains("project gutenberg license")
        || lower.contains("www.gutenberg.org")
        || lower.contains("laws of the country where you are located")
}

fn trim_trailing_space<const N: usize>(text: &mut String<N>) {
    while text.as_str().as_bytes().last().copied() == Some(b' ') {
        text.pop();
    }
}

fn trim_leading_space<const N: usize>(text: &mut String<N>) {
    let trim_len = text
        .as_str()
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    if trim_len == 0 {
        return;
    }
    let mut trimmed = String::<N>::new();
    let _ = trimmed.push_str(&text.as_str()[trim_len..]);
    *text = trimmed;
}

struct LowerAscii<const N: usize> {
    text: String<N>,
}

impl<const N: usize> LowerAscii<N> {
    fn new(input: &str) -> Self {
        let mut text = String::new();
        for byte in input.bytes() {
            if text.push((byte as char).to_ascii_lowercase()).is_err() {
                break;
            }
        }
        Self { text }
    }

    fn eq(&self, other: &str) -> bool {
        self.text.as_str() == other
    }

    fn starts_with(&self, other: &str) -> bool {
        self.text.as_str().starts_with(other)
    }

    fn ends_with(&self, other: &str) -> bool {
        self.text.as_str().ends_with(other)
    }

    fn contains(&self, other: &str) -> bool {
        self.text.as_str().contains(other)
    }

    fn word_eq(&self, other: &str) -> bool {
        self.text
            .as_str()
            .split_ascii_whitespace()
            .any(|word| word == other)
    }
}
