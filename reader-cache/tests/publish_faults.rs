//! Fault-injection tests for the publish tail, against a real embedded-sdmmc
//! FAT16 filesystem on an in-memory card that can fail the Nth read or write.
//!
//! These exist because of what B4 cost. Six review rounds, six defects, every
//! one of them in the code below, and every one the same shape: a write fails
//! *after* the store has already been updated, and the cleanup gets it wrong.
//! Deleting the cache the reader is reading out of. Returning before putting
//! their page back in the shared text arena. Reporting a walk finished when its
//! index never landed. None of it was subtle, and none of it was caught,
//! because the code lived in a `#![no_main]` firmware binary that no test could
//! reach.
//!
//! So each test below arms a fault at a specific write or read and asserts what
//! must survive it. The card model — `FaultyDisk`, `FaultPlan`, the MBR + FAT16
//! image — is copied from `upload-store/tests/transaction.rs` rather than shared:
//! integration tests cannot import each other, and exporting a harness from
//! `upload-store`'s public API to avoid ~120 duplicated lines would put test
//! scaffolding in a shipped crate. If a third crate ever needs it, that is the
//! point to extract it into a dev-only crate of its own.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use display::font::FontStyle;
use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use proto::cache::{
    BookV2SectionRecord, CoverCacheHeader, CACHE_COVER_FILE, COVER_BYTES, COVER_HEIGHT,
    COVER_STRIDE, COVER_WIDTH,
};
use proto::source::CachedSourceDigest;
use proto::text::{TextAlign, TextRole};
use reader_cache::files::{self, CacheLoadResult};
use reader_cache::layout;
use reader_cache::publish::{self, BookPublishOutcome, PublishError};
use reader_cache::store::{BookLoadStatus, ReaderStore};

const BLOCK_BYTES: usize = 512;
/// 16 MiB card: big enough that fatfs picks FAT16 and small enough to stay fast.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

/// The book under test. The key is what names its cache directory; the identity
/// pair is the (source_hash, byte_size) every cache file is stamped with, and
/// the thing a mismatched file is rejected on.
const KEY: &str = "TESTBOOK";
const IDENTITY: (u32, u32) = (0xABCD_1234, 4096);
/// The owner every keyed access proves itself against; the claim gate is
/// exercised on its own in the twin tests below.
const OWNER: proto::cache::CacheOwner<'static> = proto::cache::CacheOwner {
    key: KEY,
    root: proto::library_path::BookRoot::Library,
    locator: "Fiction/Test.epub",
};

// ---------------------------------------------------------------------------
// Fault-injecting in-memory block device
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskError;

impl core::fmt::Display for DiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "injected disk error")
    }
}

impl std::error::Error for DiskError {}

/// Arms exactly-once faults: `fail_write_in = Some(n)` fails the (n+1)th
/// subsequent write, then disarms. Exactly-once is the point — the cleanup
/// paths under test issue their *own* I/O after the fault, and a sticky fault
/// would conflate "this write failed" with "the card is gone", which is a
/// different scenario with different correct behaviour.
#[derive(Default)]
struct FaultPlan {
    fail_write_in: Cell<Option<u32>>,
    fail_read_in: Cell<Option<u32>>,
}

impl FaultPlan {
    fn take_fault(counter: &Cell<Option<u32>>) -> bool {
        match counter.get() {
            Some(0) => {
                counter.set(None);
                true
            }
            Some(n) => {
                counter.set(Some(n - 1));
                false
            }
            None => false,
        }
    }
}

struct FaultyDisk {
    data: RefCell<Vec<u8>>,
    fault: FaultPlan,
    writes: Cell<u32>,
    reads: Cell<u32>,
}

/// The test holds one `Rc` handle for arming faults while the `VolumeManager`
/// owns another. The newtype exists because the orphan rule forbids
/// implementing the foreign `BlockDevice` for `Rc<_>`.
#[derive(Clone)]
struct SharedDisk(Rc<FaultyDisk>);

impl std::ops::Deref for SharedDisk {
    type Target = FaultyDisk;
    fn deref(&self) -> &FaultyDisk {
        &self.0
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        self.reads.set(self.reads.get() + 1);
        if FaultPlan::take_fault(&self.fault.fail_read_in) {
            return Err(DiskError);
        }
        let data = self.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        self.writes.set(self.writes.get() + 1);
        if FaultPlan::take_fault(&self.fault.fail_write_in) {
            return Err(DiskError);
        }
        let mut data = self.data.borrow_mut();
        for (i, block) in blocks.iter().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            data[at..at + BLOCK_BYTES].copy_from_slice(&block[..]);
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        Ok(BlockCount(DISK_BLOCKS))
    }
}

// ---------------------------------------------------------------------------
// FAT16 image: MBR partition table + fatfs-formatted partition
// ---------------------------------------------------------------------------

fn format_disk() -> Vec<u8> {
    let mut disk = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    let part_blocks = DISK_BLOCKS - PART_START_BLOCK;

    let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
    fatfs::format_volume(
        std::io::Cursor::new(partition.as_mut_slice()),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
    )
    .expect("format FAT16 partition");
    disk[PART_START_BLOCK as usize * BLOCK_BYTES..].copy_from_slice(&partition);

    let entry = 446;
    disk[entry] = 0x00;
    disk[entry + 4] = 0x06;
    disk[entry + 8..entry + 12].copy_from_slice(&PART_START_BLOCK.to_le_bytes());
    disk[entry + 12..entry + 16].copy_from_slice(&part_blocks.to_le_bytes());
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk
}

struct StaticTime;

impl TimeSource for StaticTime {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 4,
            zero_indexed_day: 19,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

type Mgr = VolumeManager<SharedDisk, StaticTime, 8, 8, 1>;
type Dir<'a> = Directory<'a, SharedDisk, StaticTime, 8, 8, 1>;

fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(FaultyDisk {
        data: RefCell::new(format_disk()),
        fault: FaultPlan::default(),
        writes: Cell::new(0),
        reads: Cell::new(0),
    }))
}

fn open_mgr(disk: &SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000)
}

fn open_root(mgr: &Mgr) -> Dir<'_> {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("open volume");
    let raw_root = mgr
        .open_root_dir(volume.to_raw_volume())
        .expect("open root");
    Directory::new(raw_root, mgr)
}

// ---------------------------------------------------------------------------
// A book on the card, built the way the firmware builds one
// ---------------------------------------------------------------------------

/// `ReaderStore` is ~47 KB, so it is boxed here for the same reason the
/// firmware keeps it in a static: nobody should be moving it through a stack
/// frame. (See the crate root on why it has no `Default`.)
fn new_store() -> Box<ReaderStore> {
    let mut store = Box::new(ReaderStore::new());
    store.set_cache_key(KEY);
    // A one-book catalog. `set_current_index` silently declines an index past
    // the catalog total, and `selected_cover` compares against `current_index`,
    // so without this the cover assertions below would pass or fail for reasons
    // that have nothing to do with the cover.
    store.set_catalog_total(1);
    store
}

/// Fill the store's line buffer with one section's worth of body text and
/// paginate it, leaving the store exactly as a finished spine item leaves it.
fn fill_section(store: &mut ReaderStore, spine: u16, lines: usize) {
    store.clear_lines();
    for n in 0..lines {
        let line = format!("section {spine} line {n} with enough words to occupy a row");
        assert!(
            store.push_line_block(
                &line,
                FontStyle::Regular,
                TextRole::Body,
                TextAlign::Left,
                true,
                spine,
            ),
            "line buffer should hold the fixture text"
        );
    }
    layout::rebuild_page_index(store);
    assert!(
        store.page_count() > 0,
        "fixture must paginate to real pages"
    );
}

/// Write one section file, returning the index record describing it.
fn write_section(
    root: &Dir<'_>,
    store: &mut ReaderStore,
    section: u16,
    start_page: u32,
) -> BookV2SectionRecord {
    fill_section(store, section, 6);
    // The section file records `cached_spine`, and the load rejects a file whose
    // spine disagrees with the index record pointing at it. Without this every
    // section is written as spine 0, which happens to match for section 0 and
    // silently fails for every other — a fixture flaw a mutation check caught
    // only once a test reached past the first section.
    store.set_cached_spine(section);
    let page_count = store.page_count().min(u16::MAX as usize) as u16;
    let wrote = files::with_v2_sections_dir(root, &OWNER, |sections| {
        let sections = sections.expect("sections dir should exist");
        files::write_v2_section_cache_in(sections, IDENTITY, section, store)
    });
    assert!(wrote, "section {section} should write");
    BookV2SectionRecord {
        section,
        spine: section,
        start_page,
        page_count,
        partial: false,
        logical_offset: 0,
    }
}

/// A book whose cache directory and section files exist on the card, with the
/// store holding the last section written. Returns the section records.
fn build_book(
    root: &Dir<'_>,
    store: &mut ReaderStore,
    sections: usize,
) -> Vec<BookV2SectionRecord> {
    files::ensure_v2_cache_dirs(root, &OWNER).expect("cache dirs");
    let mut records = Vec::new();
    let mut start_page = 0;
    for n in 0..sections {
        let record = write_section(root, store, n as u16, start_page);
        start_page += u32::from(record.page_count);
        records.push(record);
    }
    records
}

fn total_pages(records: &[BookV2SectionRecord]) -> u32 {
    records.iter().map(|r| u32::from(r.page_count)).sum::<u32>()
}

/// Whether the book's cache directory still holds its section files. This is
/// the question the round-2 finding turned on: a failing publish must not take
/// the files out from under a reader who is reading them.
fn sections_still_on_card(root: &Dir<'_>, count: usize) -> bool {
    (0..count).all(|n| file_in_sections_dir(root, &section_name(n as u16)))
}

/// The name a section file takes under the store's default layout, which is
/// the layout every test in this file builds under.
fn section_name(section: u16) -> String {
    let mut name = heapless::String::<{ proto::cache::CACHE_SECTION_FILE_BYTES }>::new();
    proto::cache::section_file_name(default_layout_key(), section, &mut name);
    name.as_str().into()
}

/// Taken from a store rather than assumed, so the fixture and the writer
/// cannot disagree about which layout named the files.
fn default_layout_key() -> u8 {
    new_store().layout_key()
}

/// Whether one named file exists in the book's `SECTIONS/` directory.
fn file_in_sections_dir(root: &Dir<'_>, name: &str) -> bool {
    files::with_v2_sections_dir(root, &OWNER, |sections| {
        let Some(sections) = sections else {
            return false;
        };
        sections
            .open_file_in_dir(name, embedded_sdmmc::Mode::ReadOnly)
            .is_ok()
    })
}

/// Put a file that is not one of ours into `SECTIONS/`, to prove the prune
/// deletes by parsed ordinal rather than by "everything that is here".
fn write_stray_file(root: &Dir<'_>, name: &str) {
    files::with_v2_sections_dir(root, &OWNER, |sections| {
        let sections = sections.expect("sections dir should exist");
        let file = sections
            .open_file_in_dir(name, embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
            .expect("stray file should create");
        file.write(b"not a section").expect("stray write");
    });
}

/// Lay down a valid COVER.BIN for the book, the way a completed build does.
/// There is no public writer for it, so this encodes the header and body
/// directly — which also means the test is pinning the format the loader reads.
fn write_cover(root: &Dir<'_>) {
    let dir = files::open_v2_book_dir(root, &OWNER).expect("book cache dir");
    let file = dir
        .open_file_in_dir(
            CACHE_COVER_FILE,
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )
        .expect("create COVER.BIN");
    let mut header = [0u8; proto::cache::COVER_HEADER_BYTES];
    proto::cache::encode_cover_header(
        CoverCacheHeader {
            width: COVER_WIDTH as u16,
            height: COVER_HEIGHT as u16,
            stride: COVER_STRIDE as u16,
        },
        &mut header,
    )
    .expect("encode cover header");
    file.write(&header).expect("write cover header");
    file.write(&[0xA5u8; COVER_BYTES])
        .expect("write cover bits");
    file.flush().expect("flush cover");
}

// ---------------------------------------------------------------------------
// The regressions
// ---------------------------------------------------------------------------

/// Invariant: a background step whose final index write fails must not delete
/// the cache, and must put the reader's page back in the shared text arena.
///
/// This is review round 2, plus the promise `finish_background_walk` documents:
/// the builder drives the sink through the same arena the reading view renders
/// from, so between its last write and this call the arena holds the *builder's*
/// section, not the reader's page. Every exit has to restore it.
///
/// The fixture therefore models a reader who is already open on a page in one
/// section while the builder has left a later one resident — without that, the
/// restore is unguarded, and deleting the `restore_reader_page` call passes.
#[test]
fn a_failed_final_index_write_restores_the_reader_and_keeps_their_sections() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    let pages = total_pages(&records);
    // A page in the middle section, so "restored" cannot be confused with
    // "never moved" or "always page zero".
    let reader_page = records[1].start_page;
    let reader_section_start = records[1].start_page;
    let builder_page = records[2].start_page;
    assert_ne!(reader_page, 0, "the reader must not sit on page zero");
    assert_ne!(reader_page, builder_page);

    // An open reader on their page.
    store.begin_book_load();
    store.set_book_index(pages, false, &records);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert!(matches!(
        files::load_v2_section_by_global_page(&root, &OWNER, IDENTITY, reader_page, &mut store),
        CacheLoadResult::Hit { .. }
    ));
    assert!(store.covers_global_page(0, reader_page));
    assert_eq!(store.current_section_start_page, reader_section_start);

    // The builder borrows the arena for a later section, which is the state a
    // step is in when its final publish runs.
    assert!(matches!(
        files::load_v2_section_by_global_page(&root, &OWNER, IDENTITY, builder_page, &mut store),
        CacheLoadResult::Hit { .. }
    ));
    assert_eq!(
        store.current_section_start_page, builder_page,
        "the arena should now hold the builder's section, not the reader's"
    );

    // Fail the very next write, which is BOOK.BIN's.
    disk.fault.fail_write_in.set(Some(0));
    let outcome = publish::publish_book_cache(
        &root,
        &OWNER,
        IDENTITY,
        reader_page,
        &mut store,
        &records,
        pages,
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::IndexWriteFailed);
    assert_eq!(outcome.cover, None, "a failed publish reaches no cover");

    let tail = publish::finish_background_walk(
        &root,
        &OWNER,
        IDENTITY,
        reader_page,
        outcome.outcome,
        &mut store,
    );
    assert_eq!(
        tail,
        Err(PublishError::IndexWrite),
        "the walk must report the failure rather than claim it finished"
    );
    assert!(
        sections_still_on_card(&root, 3),
        "the reader's section files must survive a failed index write"
    );
    assert_eq!(
        store.current_section_start_page, reader_section_start,
        "the reader's page must be back in the arena, not the builder's section"
    );
    assert!(
        store.covers_global_page(0, reader_page),
        "the restored window must cover the page the reader is on"
    );
}

/// Invariant: the same failure on the *open* path does clear the cache. Pinning
/// both halves together is the point — the asymmetry is deliberate, and a future
/// refactor that unifies the two tails would silently break one of them.
#[test]
fn a_failed_first_open_index_write_clears_the_cache_nobody_is_holding() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    let pages = total_pages(&records);

    disk.fault.fail_write_in.set(Some(0));
    let result = publish::publish_first_open(
        &root, &OWNER, IDENTITY, 0, &mut store, &records, pages,
        // A provisional publish carries a non-zero cursor.
        1,
    );
    assert_eq!(result, Err(PublishError::IndexWrite));
    assert!(
        !sections_still_on_card(&root, 3),
        "an open that never became readable should leave no debris"
    );
}

/// Invariant: a step that cannot put the reader's page back must not report
/// success, and must leave the window unusable rather than half-loaded.
///
/// This is review round 1's third finding and round 4's subject: the difference
/// between a walk that *ended* and one that *broke*, which the caller decides
/// entirely on whether the store came through coherent.
///
/// Proves both directions in one test on purpose. The first half establishes
/// that this fixture really can produce a covered window — without it the
/// negative assertion below passes for the wrong reason, which is exactly what
/// a mutation check caught here on the first attempt.
#[test]
fn a_section_that_will_not_read_back_marks_the_window_unusable() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 2);
    let pages = total_pages(&records);
    assert!(files::write_v2_book_index(
        &root, &OWNER, IDENTITY, pages, &records, &store, false, 0,
    ));

    // Put the store in the state a completed open leaves it. The order is the
    // firmware's and it matters: `begin_book_load` clears the resident index, so
    // adopting it has to happen after the open begins and before it finishes --
    // which is exactly where `publish_book_cache` does it.
    store.begin_book_load();
    store.set_book_index(pages, false, &records);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    // Baseline: a clean load covers the page.
    assert!(
        matches!(
            files::load_v2_section_by_global_page(&root, &OWNER, IDENTITY, 0, &mut store),
            CacheLoadResult::Hit { .. }
        ),
        "fixture must be able to load a section, or the negative case below is vacuous"
    );
    assert!(
        store.covers_global_page(0, 0),
        "a clean load must leave the page renderable from RAM"
    );

    // Now drive the publish tail itself with the read armed to fail, so the
    // orchestration is what gets pinned rather than the loader beneath it.
    // `extend_background_index` adopts the grown index, then restores the
    // reader's page; that restore is what must fail and be reported.
    disk.fault.fail_read_in.set(Some(0));
    let extended = publish::extend_background_index(
        &root, &OWNER, IDENTITY, 1, 2, 0, &mut store, &records, pages,
    );
    assert_eq!(
        extended,
        Err(PublishError::SectionRead),
        "a step that cannot put the reader's page back must report it, not claim success"
    );
    assert!(
        !store.covers_global_page(0, 1),
        "a window that could not be loaded must not be advertised as covering the page"
    );
}

/// Invariant: an index a build walked away from is refused when no walk is
/// live, and accepted while one is.
///
/// The trap this closes: reading a partial index with nobody building caps the
/// book at whatever that build reached, and the reducer clamps the reader to the
/// advertised count, so no input can provoke the rebuild that would fix it. The
/// policy is `app_core::storage_loop::partial_index_is_usable`; what this test
/// pins is that `resume_spine` survives a write/read round trip, because the
/// policy is worthless if the flag does not.
#[test]
fn an_index_a_build_abandoned_is_recognisable_as_one() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 2);
    let pages = total_pages(&records);

    // A provisional publish stamps the cursor it will resume from.
    assert!(files::write_v2_book_index(
        &root, &OWNER, IDENTITY, pages, &records, &store, true, 7,
    ));
    let loaded = files::load_v2_book_index(&root, &OWNER, IDENTITY, &mut store);
    assert!(
        matches!(loaded, files::BookIndexLoadResult::Hit { unfinished: true }),
        "an index stamped with a resume cursor must read back as unfinished, got {loaded:?}"
    );
    assert!(
        !app_core::storage_loop::partial_index_is_usable(true, false),
        "unfinished with no walk live must be refused"
    );
    assert!(
        app_core::storage_loop::partial_index_is_usable(true, true),
        "unfinished with its own walk live must be accepted"
    );

    // A completed publish stamps zero, which is what says the missing pages --
    // if any -- are missing for good.
    assert!(files::write_v2_book_index(
        &root, &OWNER, IDENTITY, pages, &records, &store, false, 0,
    ));
    assert!(
        matches!(
            files::load_v2_book_index(&root, &OWNER, IDENTITY, &mut store),
            files::BookIndexLoadResult::Hit { unfinished: false }
        ),
        "a finished index must not read back as unfinished"
    );
}

/// Invariant: a clean publish leaves the reader on the page it was asked for.
/// The baseline the fault cases are measured against — if this fails, the
/// assertions above are proving nothing.
///
/// Requests a non-zero page in a later section on purpose, so it catches both
/// "nothing was loaded" and "page zero is always loaded" regressions.
#[test]
fn a_clean_publish_leaves_the_requested_page_resident() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    let pages = total_pages(&records);
    let requested = records[2].start_page;
    assert_ne!(requested, 0, "the fixture must exercise a non-zero page");

    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root, &OWNER, IDENTITY, requested, &mut store, &records, pages, false, 0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert_eq!(store.advertised_page_count(), pages);
    assert_eq!(
        store.current_section_start_page, requested,
        "the publish must leave the requested page's section resident"
    );
    assert!(
        store.covers_global_page(0, requested),
        "the requested page must be renderable from RAM after a clean publish"
    );
    assert!(
        sections_still_on_card(&root, 3),
        "a clean publish must not delete anything"
    );
}

/// Invariant: a rebuild that produces fewer sections than the one before it
/// takes the stranded tail with it.
///
/// Section files are keyed by a dense section ordinal, so a rebuild producing
/// fewer of them rewrites `S000..S(new-1)` and leaves `S(new)..S(old-1)` on the
/// card referenced by nothing — `load_v2_section_by_global_page` indexes off
/// BOOK.BIN. Type-size and orientation changes do not cause this (sections are
/// content-bounded; see `prune_orphan_sections_in`), so the shrink is
/// constructed here rather than driven through a setting.
#[test]
fn a_rebuild_with_fewer_sections_prunes_the_stranded_tail() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let wide = build_book(&root, &mut store, 5);
    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        &OWNER,
        IDENTITY,
        0,
        &mut store,
        &wide,
        total_pages(&wide),
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);
    assert!(sections_still_on_card(&root, 5));

    // A completed rebuild over the same content with a smaller final section
    // set, as a change to the capacity constants would produce: it rewrites
    // S000..S002 and leaves S003 and S004 behind.
    let narrow = &wide[..3];
    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        &OWNER,
        IDENTITY,
        0,
        &mut store,
        narrow,
        total_pages(narrow),
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        sections_still_on_card(&root, 3),
        "the sections the new index names must survive"
    );
    assert!(
        !file_in_sections_dir(&root, &section_name(3))
            && !file_in_sections_dir(&root, &section_name(4)),
        "the sections the new index no longer names must be gone"
    );
}

/// Invariant: a suspended walk is never pruned against.
///
/// This is the one that makes the prune safe to have at all. A progressive
/// first open publishes a provisional index spanning only the sections written
/// so far, then keeps walking. Pruning against that frontier would delete the
/// sections the walk is about to need — turning a cheap continuation into a
/// full rebuild, and doing it to a reader who is mid-book. `resume_spine` is
/// the gate: non-zero means someone is coming back.
#[test]
fn a_suspended_walk_keeps_the_sections_past_its_frontier() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let all = build_book(&root, &mut store, 5);
    let frontier = &all[..2];

    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        &OWNER,
        IDENTITY,
        0,
        &mut store,
        frontier,
        total_pages(frontier),
        true,
        // Non-zero: this walk is suspended at spine 2 and will resume.
        2,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        sections_still_on_card(&root, 5),
        "a provisional publish must leave every section the walk will resume into"
    );
}

/// Invariant: the prune deletes by parsed section ordinal, not by "whatever is
/// in the directory". `SECTIONS/` lives on removable media, so anything whose
/// name is not one of ours is somebody else's problem and must be left alone.
#[test]
fn the_prune_leaves_names_it_does_not_recognise() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let wide = build_book(&root, &mut store, 4);
    write_stray_file(&root, "NOTES.TXT");
    // Two digits, so not a name this code ever writes.
    write_stray_file(&root, "S12.BIN");

    let narrow = &wide[..1];
    store.begin_book_load();
    let outcome = publish::publish_book_cache(
        &root,
        &OWNER,
        IDENTITY,
        0,
        &mut store,
        narrow,
        total_pages(narrow),
        false,
        0,
    );
    assert_eq!(outcome.outcome, BookPublishOutcome::Ready);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        !file_in_sections_dir(&root, &section_name(1)),
        "the orphaned section should still go"
    );
    assert!(
        file_in_sections_dir(&root, "NOTES.TXT") && file_in_sections_dir(&root, "S12.BIN"),
        "names this code does not write must be left alone"
    );
}

/// Invariant: one refused delete does not strand the orphans behind it.
///
/// `remove_file_reclaiming_clusters` opens, truncates, closes and deletes, so a
/// fault in any of those fails one particular file. An earlier version read
/// that as "the card is refusing deletes" and returned, which left every later
/// orphan on the card — and because the prune only runs after a *completed*
/// rebuild, the next attempt needs another full rebuild, cache clear, or the
/// book leaving the card. For the capacity-constant change this feature exists
/// for, that may never come.
///
/// Exactly-once is the card model on purpose (see `FaultPlan`): a sticky fault
/// is a different scenario. This arms one refused write, which lands on the
/// first orphan's truncate, and asserts the rest still go.
#[test]
fn a_refused_delete_does_not_strand_the_orphans_behind_it() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // Five sections, of which S002..S004 are about to become orphans. All
    // three land in one listing batch (SECTION_SWEEP_BATCH is 16), so this
    // exercises "kept going after the failure", not "picked it up next pass".
    build_book(&root, &mut store, 5);
    assert!(sections_still_on_card(&root, 5));

    disk.fault.fail_write_in.set(Some(0));
    let removed = files::prune_orphan_sections(&root, &OWNER, default_layout_key(), 2);

    assert!(
        file_in_sections_dir(&root, &section_name(0))
            && file_in_sections_dir(&root, &section_name(1)),
        "the sections the index still names must survive"
    );
    assert_eq!(
        removed, 3,
        "every orphan must come off the card, including the one behind the refused write"
    );
    assert!(
        !file_in_sections_dir(&root, &section_name(2))
            && !file_in_sections_dir(&root, &section_name(3))
            && !file_in_sections_dir(&root, &section_name(4)),
        "a refused delete must not strand the orphans after it"
    );
}

/// Invariant: a progressive first open adopts the cover, like every other
/// successful publish.
///
/// The regression this pins: the crate extraction moved the `COVER.BIN` load out
/// of `publish_book_cache` and into the firmware's reporting helper. That helper
/// only runs on the *final* publish, so a progressively opened book had no cover
/// until its background walk finished — and `selected_cover` gates the Library
/// and sleep renders on it, so backing out of the book or sleeping mid-build
/// rendered without a cover that was already sitting on the card. With the
/// indefinite step retry, a long card outage could hold that state for minutes.
///
/// Adopting the cover is publication, not telemetry, so it belongs on the path
/// every publish takes.
#[test]
fn a_progressive_first_open_adopts_the_cover() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    let records = build_book(&root, &mut store, 3);
    write_cover(&root);

    // Only the first section is published, as a suspended walk would leave it.
    let first = &records[..1];
    let pages = total_pages(first);

    store.begin_book_load();
    let result = publish::publish_first_open(
        &root, &OWNER, IDENTITY, 0, &mut store, first, pages,
        // Non-zero: the walk is coming back for the rest.
        1,
    );
    assert_eq!(result, Ok(()), "the provisional publish should succeed");
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    assert!(
        store.covers_global_page(0, 0),
        "the requested page must be resident after a provisional publish"
    );
    assert!(
        store
            .selected_cover(app_core::ReaderSource::sd(0).book_id())
            .is_some(),
        "the cover must be available before the background walk finishes"
    );
}

/// Invariant: once a background walk has grown past the batching threshold, the
/// step that rewrites BOOK.BIN reports the new published frontier — and if that
/// write fails, it still must not delete the cache the reader is reading from.
///
/// The other `extend_background_index` test stays under
/// `INDEX_PUBLISH_SECTIONS`, so it only ever exercises the restore read and the
/// index write never happens. This one crosses the threshold, which is the only
/// way to reach the `published_now` advance and the refused-write branch beside
/// it.
#[test]
fn a_step_past_the_batching_threshold_publishes_and_survives_a_refused_write() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();

    // Enough sections that `grown` clears INDEX_PUBLISH_SECTIONS in one step.
    let sections = 17;
    let records = build_book(&root, &mut store, sections);
    let pages = total_pages(&records);
    let reader_page = records[1].start_page;

    store.begin_book_load();
    store.set_book_index(pages, false, &records);
    store.finish_book_load(0, 0, BookLoadStatus::Ready);

    // A clean step: the index lands, so the frontier advances to every section
    // the walk has built.
    let published = publish::extend_background_index(
        &root,
        &OWNER,
        IDENTITY,
        reader_page,
        // The spine cursor the resume would carry.
        sections as u16,
        0,
        &mut store,
        &records,
        pages,
    );
    assert_eq!(
        published,
        Ok(sections as u16),
        "crossing the threshold must advance the published frontier to every built section"
    );

    // The same step with the index write refused. The reader is inside these
    // files, so a truncated BOOK.BIN is the correct cost -- never a deleted
    // cache.
    disk.fault.fail_write_in.set(Some(0));
    let refused = publish::extend_background_index(
        &root,
        &OWNER,
        IDENTITY,
        reader_page,
        sections as u16,
        0,
        &mut store,
        &records,
        pages,
    );
    assert_eq!(
        refused,
        Err(PublishError::IndexWrite),
        "a refused index write must be reported, not swallowed"
    );
    assert!(
        sections_still_on_card(&root, sections),
        "a refused index write must not take the reader's sections with it"
    );
    assert!(
        store.covers_global_page(0, reader_page),
        "the reader's page must still be resident after the refused write"
    );
}

// ---------------------------------------------------------------------------
// Position survival across the v8 re-key
// ---------------------------------------------------------------------------

/// A card upgraded across catalog v8 holds every inactive book's reading
/// position under the key pre-v8 firmware derived from the display path.
/// Position is the one non-rebuildable thing under a key, so the new lookup
/// must recover it from the old directory; a mutation that drops the legacy
/// layer resumes every such book at the beginning.
/// Write one section file under a layout that is not the store's, by moving
/// the store to those settings for the write and back afterwards. Stands in
/// for the reader having used that configuration earlier.
fn write_section_under(
    root: &Dir<'_>,
    store: &mut ReaderStore,
    portrait: bool,
    section: u16,
) -> u8 {
    let settings = store.type_settings();
    let was_portrait = store.portrait();
    store.set_layout(settings, portrait);
    let key = store.layout_key();
    write_section(root, store, section, 0);
    store.set_layout(settings, was_portrait);
    key
}

/// R9, the flow the milestone exists for: A, then B, then back to A, with A's
/// pagination still on the card.
#[test]
fn a_second_layout_does_not_take_the_first_ones_pagination() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();
    files::ensure_v2_cache_dirs(&root, &OWNER).expect("cache dirs");

    let landscape = write_section_under(&root, &mut store, false, 0);
    let portrait = write_section_under(&root, &mut store, true, 0);
    assert_ne!(landscape, portrait, "the page box names a layout apart");

    let mut resident = files::resident_layouts(&root, &OWNER);
    resident.sort_unstable();
    let mut expected = [landscape, portrait];
    expected.sort_unstable();
    assert_eq!(
        resident.as_slice(),
        &expected[..],
        "both layouts keep their own pagination"
    );
}

/// R10: the bound is on stored layouts, and a third arriving evicts one.
/// Nothing evicts merely because a layout stopped being current.
#[test]
fn a_third_layout_evicts_one_and_only_then() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();
    files::ensure_v2_cache_dirs(&root, &OWNER).expect("cache dirs");

    let first = write_section_under(&root, &mut store, false, 0);
    let second = write_section_under(&root, &mut store, true, 0);
    assert_eq!(files::resident_layouts(&root, &OWNER).len(), 2);

    // Re-opening one of the two evicts nothing: it is already resident.
    assert!(files::evict_layouts_for(&root, &OWNER, first));
    assert_eq!(
        files::resident_layouts(&root, &OWNER).len(),
        2,
        "a layout already on the card costs nothing to open"
    );

    // A third makes room, and the one that goes is not the one arriving.
    let third = first.wrapping_add(64).max(1);
    assert!(files::evict_layouts_for(&root, &OWNER, third));
    let left = files::resident_layouts(&root, &OWNER);
    assert_eq!(left.len(), 1, "one of the two was evicted");
    assert!(
        left.contains(&first) || left.contains(&second),
        "and the survivor is one of the two that were there"
    );
    assert!(!left.contains(&third), "the arriving layout writes its own");
}

/// R11: pagination is derived and a place is not. Evicting every layout of a
/// book leaves the reader's place where it was.
#[test]
fn evicting_pagination_leaves_the_place_alone() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut store = new_store();
    files::ensure_v2_cache_dirs(&root, &OWNER).expect("cache dirs");

    let id = book_id(11);
    let anchor = proto::anchor::ContentAnchor::at(2, 900);
    files::write_place(&root, id, anchor, source(), Some(0)).expect("the place stores");

    let layout = write_section_under(&root, &mut store, false, 0);
    assert!(files::empty_layout_cache(&root, KEY, layout));
    assert!(files::resident_layouts(&root, &OWNER).is_empty());
    assert_eq!(
        place_anchor(&root, id),
        Some(anchor),
        "the place is not pagination and does not go with it"
    );
}

/// The content a test writes its places against: a length, and no recorded
/// hash unless the test is about one.
fn source() -> proto::nvm::PlaceSource {
    proto::nvm::PlaceSource {
        byte_size: IDENTITY.1,
        digest: None,
    }
}

fn place_anchor(
    root: &Dir<'_>,
    id: proto::identity::BookId,
) -> Option<proto::anchor::ContentAnchor> {
    files::read_place(root, id).map(|place| place.anchor)
}

fn book_id(seed: u8) -> proto::identity::BookId {
    proto::identity::BookId::from_bytes([seed; 16]).expect("a non-zero id")
}

/// The whole point of the format: a place belongs to the copy, so it is
/// legible after anything that used to lose it.
/// The case the anchor exists for, and the one a locator-derived source
/// identity would have thrown away: a proven move keeps the copy's id and
/// changes nothing about its content, so the place stays exact.
#[test]
fn a_move_leaves_a_place_exact() {
    let was = proto::nvm::PlaceSource {
        byte_size: 8_123_456,
        digest: Some([7u8; 32]),
    };
    // Same bytes, somewhere else on the card. Nothing about content moved.
    let moved = was;
    assert!(was.describes(&moved), "a move is not a source change");

    // A different edition of the same length, read, so both sides have a
    // hash to compare.
    let replaced = proto::nvm::PlaceSource {
        byte_size: 8_123_456,
        digest: Some([9u8; 32]),
    };
    assert!(
        !was.describes(&replaced),
        "a same-length replacement is caught by the recorded bytes"
    );

    // Neither side read: the library identity PRD's R4 accepts this, because
    // nothing on the device can tell the two apart.
    let unread = proto::nvm::PlaceSource {
        byte_size: 8_123_456,
        digest: None,
    };
    assert!(unread.describes(&proto::nvm::PlaceSource {
        byte_size: 8_123_456,
        digest: None,
    }));
    assert!(
        !unread.describes(&proto::nvm::PlaceSource {
            byte_size: 9_000_000,
            digest: None,
        }),
        "and a different length settles it without any hash"
    );
}

/// A place with no trustworthy fraction is still a place. The anchor is the
/// valuable half and it is exact; the progression is the guess for a source
/// that changed.
#[test]
fn a_place_stores_its_anchor_before_it_knows_the_books_length() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let id = book_id(31);
    let anchor = proto::anchor::ContentAnchor::at(6, 1_200);

    files::write_place(&root, id, anchor, source(), None).expect("the place stores");
    let place = files::read_place(&root, id).expect("it reads back");
    assert_eq!(place.anchor, anchor, "the anchor is there");
    assert_eq!(
        place.progression, None,
        "and the fraction is honestly absent"
    );
}

/// R13 and R15: a place written against other bytes keeps its progression,
/// and says plainly that the anchor is no longer evidence.
#[test]
fn a_place_survives_a_replacement_without_claiming_to_be_exact() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let id = book_id(21);
    let anchor = proto::anchor::ContentAnchor::at(4, 2_048);
    // Two thirds of the way through: the part of a place that survives an edit.
    let progression = (u16::MAX / 3) * 2;

    files::write_place(&root, id, anchor, source(), Some(progression)).expect("the place stores");
    let place = files::read_place(&root, id).expect("it reads back");
    assert!(
        place.describes(&source()),
        "against the bytes it was written for"
    );
    assert_eq!(place.anchor, anchor, "so the anchor is exact");

    // A managed replacement keeps the id and changes the bytes, which shows
    // up in the length whatever else it changes.
    let replaced = proto::nvm::PlaceSource {
        byte_size: IDENTITY.1 + 4_096,
        digest: None,
    };
    assert!(
        !place.describes(&replaced),
        "the same place against other bytes is a guess"
    );
    assert_eq!(
        place.progression,
        Some(progression),
        "and the guess is made from the progression"
    );
    assert_eq!(
        place.id, id,
        "the copy is the same copy; only what it holds changed"
    );
}

#[test]
fn a_place_belongs_to_the_copy_rather_than_to_a_file() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let id = book_id(7);
    let anchor = proto::anchor::ContentAnchor::at(3, 4_096);

    assert_eq!(place_anchor(&root, id), None, "nothing stored yet");
    files::write_place(&root, id, anchor, source(), Some(0)).expect("the place stores");
    assert_eq!(place_anchor(&root, id), Some(anchor));

    // Nothing about where the book's file sits reached that write, so nothing
    // about its locator can invalidate it. A second copy keeps its own.
    let twin = book_id(9);
    assert_eq!(place_anchor(&root, twin), None, "a twin starts fresh");
    let twin_anchor = proto::anchor::ContentAnchor::at(0, 12);
    files::write_place(&root, twin, twin_anchor, source(), Some(0))
        .expect("the twin stores its own");
    assert_eq!(
        place_anchor(&root, id),
        Some(anchor),
        "and not over this one"
    );
    assert_eq!(place_anchor(&root, twin), Some(twin_anchor));
}

#[test]
fn a_place_is_rewritten_in_place_and_survives_the_write() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let id = book_id(4);
    for page in 0..6u32 {
        let anchor = proto::anchor::ContentAnchor::at(1, page * 700);
        files::write_place(&root, id, anchor, source(), Some(0)).expect("saves");
        assert_eq!(
            place_anchor(&root, id),
            Some(anchor),
            "each save reads back, so the two generations alternate cleanly"
        );
    }
}

/// A place written by a build whose content stream differs cannot be read as
/// content, and opening at the start beats opening somewhere wrong.
#[test]
fn a_place_from_another_content_stream_is_not_believed() {
    let mut bytes = proto::nvm::PlaceRecord {
        id: book_id(2),
        anchor: proto::anchor::ContentAnchor::at(5, 50),
        source: source(),
        progression: Some(0),
    }
    .encode();
    assert!(proto::nvm::PlaceRecord::decode(&bytes).is_some());
    bytes[5] = bytes[5].wrapping_add(1);
    assert_eq!(
        proto::nvm::PlaceRecord::decode(&bytes),
        None,
        "a stream version this build does not index the same way"
    );

    let mut torn = proto::nvm::PlaceRecord {
        id: book_id(2),
        anchor: proto::anchor::ContentAnchor::at(5, 50),
        source: source(),
        progression: Some(0),
    }
    .encode();
    torn[12] ^= 0xFF;
    assert_eq!(
        proto::nvm::PlaceRecord::decode(&torn),
        None,
        "and a torn one fails its checksum"
    );
}

#[test]
fn a_position_saved_under_the_old_key_survives_the_re_key() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // What old firmware left behind: a position under the display-path key.
    // Seeded through the current writer, whose claim the legacy reader
    // ignores the way it ignores everything pre-claim firmware did not
    // write.
    let display = "/books/dune.epub";
    let size = 4_096u32;
    let old_key = proto::cache::legacy_position_cache_key(display, size)
        .expect("a direct shelf book has a legacy key");
    let old_owner = proto::cache::CacheOwner {
        key: old_key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "dune.epub",
    };
    files::write_position_file(&root, &old_owner, 17, 3).expect("seed the old position");

    // The book's current key holds nothing yet; the lookup must fall back.
    let owner = proto::cache::CacheOwner {
        key: "E0000001",
        root: proto::library_path::BookRoot::Library,
        locator: "dune.epub",
    };
    assert_eq!(
        files::read_position_file_or_legacy(&root, &owner, display, size),
        Some((17, 3)),
        "an upgrade must not strand the reader's place under the old key"
    );

    // The next ordinary save lands under the current key, which then wins.
    files::write_position_file(&root, &owner, 21, 9).expect("save under the current key");
    assert_eq!(
        files::read_position_file_or_legacy(&root, &owner, display, size),
        Some((21, 9)),
        "a position saved under the current key must shadow the legacy one"
    );

    // A nested display shape has no legacy key: old firmware could not have
    // written one, and a lookalike must not adopt the flat book's position.
    let nested = proto::cache::CacheOwner {
        key: "E0000002",
        root: proto::library_path::BookRoot::Library,
        locator: "fiction/dune.epub",
    };
    assert_eq!(
        files::read_position_file_or_legacy(&root, &nested, "/books/fiction/dune.epub", size),
        None,
        "a nested book must not inherit a flat book's old position"
    );
}

/// A full-hash twin shares the key, the identity, and therefore every
/// `(hash, size)` check, so the directory claim is the only thing keeping
/// it out of the other book's cache. The pair is the verified same-domain
/// collision from `proto::catalog`'s tests.
#[test]
fn a_full_hash_twin_cannot_touch_the_other_books_cache() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let size = 1_234_567u32;
    let hash = proto::cache::source_hash_at(
        proto::library_path::BookRoot::Library,
        "Fiction/zIx6RBhQEK.epub",
        size,
    );
    assert_eq!(
        hash,
        proto::cache::source_hash_at(
            proto::library_path::BookRoot::Library,
            "Fiction/nTfOyBwYzX.epub",
            size,
        ),
        "the collision this test exists for",
    );
    let key = proto::cache::cache_key_from(hash);
    let owner_a = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/zIx6RBhQEK.epub",
    };
    let twin_b = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/nTfOyBwYzX.epub",
    };

    files::ensure_v2_cache_dirs(&root, &owner_a).expect("A claims its directory");
    files::write_position_file(&root, &owner_a, 5, 7).expect("A writes its position");

    // Every hash agrees for B; the claim alone must hold the line, in both
    // directions.
    assert!(
        files::ensure_v2_cache_dirs(&root, &twin_b).is_err(),
        "the twin cannot claim the directory"
    );
    assert!(
        files::write_position_file(&root, &twin_b, 9, 9).is_err(),
        "the twin cannot write into it"
    );
    assert!(
        files::open_v2_book_dir(&root, &twin_b).is_none(),
        "the twin cannot open it, so no artifact under it can load"
    );
    assert_eq!(
        files::read_position_file(&root, &twin_b),
        None,
        "the twin reads no position from it"
    );

    // The owner is untouched by the refusals.
    assert!(files::open_v2_book_dir(&root, &owner_a).is_some());
    assert_eq!(files::read_position_file(&root, &owner_a), Some((5, 7)));
}

/// Clearing a book's cache keeps its position, so it must keep the claim
/// that proves whose position it is: with the claim gone, the surviving
/// place would read as anyone's, and the full-hash twin would inherit it.
#[test]
fn a_cleared_cache_keeps_its_claim_with_its_position() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let size = 1_234_567u32;
    let hash = proto::cache::source_hash_at(
        proto::library_path::BookRoot::Library,
        "Fiction/zIx6RBhQEK.epub",
        size,
    );
    let key = proto::cache::cache_key_from(hash);
    let owner_a = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/zIx6RBhQEK.epub",
    };
    let twin_b = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/nTfOyBwYzX.epub",
    };

    files::ensure_v2_cache_dirs(&root, &owner_a).expect("A claims");
    files::write_position_file(&root, &owner_a, 5, 7).expect("A writes its position");
    files::empty_cache_dir(&root, key.as_str());

    // The ordinary supported clear ran; the twin still gets nothing.
    assert_eq!(
        files::read_position_file(&root, &twin_b),
        None,
        "the cleared cache must not surrender A's position to the twin"
    );
    assert!(
        files::write_position_file(&root, &twin_b, 9, 9).is_err(),
        "nor may the twin adopt the directory"
    );
    // A itself still owns the place it kept.
    assert_eq!(files::read_position_file(&root, &owner_a), Some((5, 7)));
}

/// The sweep releases a departed owner's claim rather than leaving it armed
/// or deleting it. The evidence keeps naming the owner: the owner returning
/// resumes its position, while a twin adopting the key knows the surviving
/// place is not its own and starts clean.
#[test]
fn a_released_claim_lets_the_owner_resume_and_a_twin_adopt_clean() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let size = 1_234_567u32;
    let hash = proto::cache::source_hash_at(
        proto::library_path::BookRoot::Library,
        "Fiction/zIx6RBhQEK.epub",
        size,
    );
    let key = proto::cache::cache_key_from(hash);
    let owner_a = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/zIx6RBhQEK.epub",
    };
    let twin_b = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/nTfOyBwYzX.epub",
    };

    // The owner-returns half: release, then the owner comes back.
    files::ensure_v2_cache_dirs(&root, &owner_a).expect("A claims");
    files::write_position_file(&root, &owner_a, 5, 7).expect("A writes its position");
    assert!(files::release_book_dir_claim(&root, key.as_str()));
    assert_eq!(
        files::read_position_file(&root, &owner_a),
        Some((5, 7)),
        "a released claim still names A, so A still reads its place"
    );
    files::ensure_v2_cache_dirs(&root, &owner_a).expect("A reactivates its released claim");
    assert_eq!(files::read_position_file(&root, &owner_a), Some((5, 7)));

    // The twin-adopts half: release again, then the twin takes the key.
    assert!(files::release_book_dir_claim(&root, key.as_str()));
    files::ensure_v2_cache_dirs(&root, &twin_b).expect("a released key is adoptable");
    assert_eq!(
        files::read_position_file(&root, &twin_b),
        None,
        "the adopted directory starts clean: A's place was provably not B's"
    );
    files::write_position_file(&root, &twin_b, 9, 9).expect("B owns the key now");
    assert_eq!(files::read_position_file(&root, &twin_b), Some((9, 9)));
    // And A is the refused twin from here on.
    assert!(files::ensure_v2_cache_dirs(&root, &owner_a).is_err());
}

/// Failing to read WHO.BIN is not evidence that there is no owner: a
/// transient card error must refuse the writer rather than authorize it to
/// erase and adopt a directory somebody may hold.
#[test]
fn an_unreadable_claim_refuses_adoption_rather_than_granting_it() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let size = 1_234_567u32;
    let hash = proto::cache::source_hash_at(
        proto::library_path::BookRoot::Library,
        "Fiction/zIx6RBhQEK.epub",
        size,
    );
    let key = proto::cache::cache_key_from(hash);
    let owner_a = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/zIx6RBhQEK.epub",
    };
    let twin_b = proto::cache::CacheOwner {
        key: key.as_str(),
        root: proto::library_path::BookRoot::Library,
        locator: "Fiction/nTfOyBwYzX.epub",
    };
    files::ensure_v2_cache_dirs(&root, &owner_a).expect("A claims");
    files::write_position_file(&root, &owner_a, 5, 7).expect("A writes its position");

    // Every read from here on fails, wherever in the claim path it lands.
    disk.fault.fail_read_in.set(Some(0));
    assert!(
        files::write_position_file(&root, &twin_b, 9, 9).is_err(),
        "a card that would not answer authorizes nothing"
    );
    disk.fault.fail_read_in.set(None);

    // A's ownership and place survived the twin's faulted attempt.
    assert_eq!(files::read_position_file(&root, &owner_a), Some((5, 7)));
    assert!(files::open_v2_book_dir(&root, &owner_a).is_some());
}

// ---------------------------------------------------------------------------
// Moving through the folder tree, against a card that stops answering.
//
// The whole contract is one sentence: a move either lands with a page of rows
// in front of the reader, or browsing is exactly where it was. The app keeps
// its own depth and rows on that word, so a move that reported success after
// a read it could not make would leave the two halves describing different
// folders, with the screen naming one and every later command landing on the
// other.

/// Descend by long name, since making a directory hands back nothing to
/// descend through and the alias need not be the name.
fn open_child<'a>(dir: &Dir<'a>, name: &str) -> Dir<'a> {
    let entry = upload_store::library::entry_in(dir, name)
        .expect("read")
        .expect("present");
    dir.open_dir(entry.alias).expect("open")
}

/// A shelf with `Fiction/` under it, holding `books` books.
fn seed_shelf(root: &Dir<'_>, books: usize) {
    root.make_dir_in_dir("BOOKS").expect("mkdir");
    let shelf = open_child(root, "BOOKS");
    let top = shelf.create_file_in_dir_lfn("Top.epub").expect("create");
    top.write(b"top").expect("write");
    top.close().expect("close");
    shelf.make_dir_in_dir_lfn("Fiction").expect("mkdir");
    let fiction = open_child(&shelf, "Fiction");
    for index in 0..books {
        let file = fiction
            .create_file_in_dir_lfn(&std::format!("Book {index:03}.epub"))
            .expect("create");
        file.write(b"x").expect("write");
        file.close().expect("close");
    }
}

/// At the library root with its listing loaded, which is where every move
/// below starts.
fn at_library_root(root: &Dir<'_>) -> Box<ReaderStore> {
    let mut store = Box::new(ReaderStore::new());
    reader_cache::browse::list_here(&mut store, root, true).expect("the root lists");
    store
}

/// The move that motivated the checkpoint. Every read the move makes is
/// failed in turn, and each outcome has to be one of exactly two things: the
/// move landed with the page it was supposed to validate itself against, or
/// browsing is where it started. The middle case is the bug: a count that
/// answered, a page that did not, and both halves committed to a folder with
/// no rows in it.
#[test]
fn a_move_either_lands_with_its_rows_or_does_not_move() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    seed_shelf(&root, 4);

    let folder = {
        let store = at_library_root(&root);
        let folder = store.browse().count() - 1;
        assert_eq!(
            store.folder_row(folder as usize).map(|row| row.is_dir),
            Some(true),
            "the last row is the folder"
        );
        folder
    };

    let mut refusals = 0usize;
    let mut arrivals = 0usize;
    for probe in 0..64 {
        let mut attempt = at_library_root(&root);
        let was_at = attempt.browse().path().clone();
        let was_count = attempt.browse().count();

        disk.fault.fail_read_in.set(Some(probe));
        let outcome = reader_cache::browse::choose_row(&mut attempt, &root, folder, true);
        disk.fault.fail_read_in.set(None);

        match outcome {
            reader_cache::browse::RowChoice::Entered(listing) => {
                arrivals += 1;
                assert_eq!(attempt.browse().path().as_str(), "Fiction");
                assert!(listing.count > 0, "probe {probe}: the folder has books");
                assert!(
                    !attempt.folder_rows().is_empty(),
                    "probe {probe}: entered a folder whose page was never read",
                );
            }
            reader_cache::browse::RowChoice::Failed => {
                refusals += 1;
                assert_eq!(
                    attempt.browse().path().as_str(),
                    was_at.as_str(),
                    "probe {probe}: a refused move moved anyway",
                );
                assert_eq!(attempt.browse().count(), was_count, "probe {probe}");
            }
            other => panic!("probe {probe}: {other:?}"),
        }
    }
    assert!(refusals > 0, "no read in the move could be failed");
    assert!(arrivals > 0, "no read in the move was survivable");
}

/// The same sweep for Back, whose parent walk spans several pages. A
/// departure that lands has to have walked the whole parent: one that stopped
/// early could pass over the very name the cursor was going back to, and
/// report a shorter folder than the card holds.
#[test]
fn a_departure_either_walks_the_whole_parent_or_does_not_move() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    // More books than one page holds, so the parent walk takes several.
    seed_shelf(&root, 40);

    let (folder, root_rows) = {
        let store = at_library_root(&root);
        (store.browse().count() - 1, store.browse().count())
    };

    let mut refusals = 0usize;
    let mut arrivals = 0usize;
    for probe in 0..96 {
        let mut attempt = at_library_root(&root);
        assert!(matches!(
            reader_cache::browse::choose_row(&mut attempt, &root, folder, true),
            reader_cache::browse::RowChoice::Entered(_)
        ));
        let inside = attempt.browse().path().clone();
        let inside_count = attempt.browse().count();

        disk.fault.fail_read_in.set(Some(probe));
        let left = reader_cache::browse::leave_folder(&mut attempt, &root, true);
        disk.fault.fail_read_in.set(None);

        match left {
            Some(listing) => {
                arrivals += 1;
                assert!(attempt.browse().is_root(), "probe {probe}");
                assert_eq!(
                    listing.count, root_rows,
                    "probe {probe}: a departure that landed walked a short parent",
                );
                assert_eq!(
                    listing.selection,
                    root_rows - 1,
                    "probe {probe}: and lost the folder it was going back to",
                );
            }
            None => {
                refusals += 1;
                assert_eq!(
                    attempt.browse().path().as_str(),
                    inside.as_str(),
                    "probe {probe}: a refused departure left anyway",
                );
                assert_eq!(attempt.browse().count(), inside_count, "probe {probe}");
            }
        }
    }
    assert!(refusals > 0, "no read in the departure could be failed");
    assert!(arrivals > 0, "no read in the departure was survivable");
}

/// The relist a scan owes, under the same oracle as a move. A card that
/// answered the scan and then would not answer for the rows must not come
/// back looking like a card with no books on it: those are different states,
/// and the row count alone cannot tell them apart.
#[test]
fn a_post_scan_relist_either_lists_or_reports_it_could_not() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    seed_shelf(&root, 4);

    // What the root really holds, read cleanly.
    let truth = {
        let mut store = Box::new(ReaderStore::new());
        reader_cache::browse::relist_root(&mut store, &root, true).expect("a readable root lists")
    };
    assert!(truth.count > 0, "the card has books and a folder on it");

    let mut refusals = 0usize;
    let mut arrivals = 0usize;
    for probe in 0..64 {
        let mut store = Box::new(ReaderStore::new());
        // Somewhere else entirely, the way a scan finds browsing.
        reader_cache::browse::list_here(&mut store, &root, true).expect("root lists");
        let folder = store.browse().count() - 1;
        assert!(matches!(
            reader_cache::browse::choose_row(&mut store, &root, folder, true),
            reader_cache::browse::RowChoice::Entered(_)
        ));

        disk.fault.fail_read_in.set(Some(probe));
        let listed = reader_cache::browse::relist_root(&mut store, &root, true);
        disk.fault.fail_read_in.set(None);

        match listed {
            Some(listing) => {
                arrivals += 1;
                assert_eq!(
                    listing, truth,
                    "probe {probe}: a relist that landed described a different root",
                );
                assert!(
                    !store.folder_rows().is_empty(),
                    "probe {probe}: listed a root whose page was never read",
                );
            }
            None => {
                refusals += 1;
                assert!(
                    store.browse().is_root(),
                    "probe {probe}: a scan puts browsing at the root either way",
                );
                assert_eq!(
                    store.browse().count(),
                    0,
                    "probe {probe}: a root that could not be read holds no rows",
                );
                assert!(store.folder_rows().is_empty(), "probe {probe}");
            }
        }
    }
    assert!(refusals > 0, "no read in the relist could be failed");
    assert!(arrivals > 0, "no read in the relist was survivable");
}

/// The relist retires every row number picked before it, whether or not the
/// catalog moved. A scan whose recovery is unfinished rebuilds nothing, so
/// the catalog epoch stands, and browsing goes back to the root anyway: a row
/// A page the card would not read leaves no rows behind.
///
/// `ensure_page` runs before every Library paint and slides the resident
/// page over the rows about to be drawn. It is best-effort by contract: a
/// card that stalls costs that paint its rows. What it must not do is keep
/// serving the page the reader has just scrolled off, so both ways of
/// failing, the folder that would not open and the page that would not
/// read, have to leave the same nothing behind.
///
/// Both ways end in the same empty page, so the sweep alone cannot say
/// which one it exercised, and a later change that made resolution cost
/// more reads could leave it testing only the open. Each probe is therefore
/// run twice on two freshly built cards, once through `open_listing` alone
/// to learn whether that probe faults the opening, and once through
/// `ensure_page`. A fresh card per run makes the read sequence after the
/// arming point identical between the two, so the first run is an oracle
/// for the second, and the count below is of failures proven to be inside
/// the paging.
#[test]
fn a_page_that_would_not_read_leaves_no_rows_behind() {
    /// Build a card, enter the seeded folder, and hand the caller the disk,
    /// the card root, the store and the last row number.
    fn in_the_folder(f: impl FnOnce(&SharedDisk, &Dir<'_>, Box<ReaderStore>, u16)) {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        seed_shelf(&root, 40);
        let mut store = Box::new(ReaderStore::new());
        reader_cache::browse::list_here(&mut store, &root, true).expect("root lists");
        let folder = store.browse().count() - 1;
        assert!(matches!(
            reader_cache::browse::choose_row(&mut store, &root, folder, true),
            reader_cache::browse::RowChoice::Entered(_)
        ));
        let far = store.browse().count() - 1;
        f(&disk, &root, store, far);
    }

    let mut page_failures = 0usize;
    let mut open_failures = 0usize;
    for probe in 0..48 {
        // The oracle: with this probe armed, does the opening survive?
        let mut opened = false;
        in_the_folder(|disk, root, store, _| {
            let path = store.browse().path().clone();
            disk.fault.fail_read_in.set(Some(probe));
            opened = matches!(
                upload_store::library::open_listing(root, &path),
                Ok(Some(_))
            );
            disk.fault.fail_read_in.set(None);
        });

        in_the_folder(|disk, root, mut store, far| {
            assert!(
                store.folder_row(0).is_some(),
                "the folder came with its first page",
            );
            disk.fault.fail_read_in.set(Some(probe));
            let touched = reader_cache::browse::ensure_page(&mut store, root, far, true);
            disk.fault.fail_read_in.set(None);
            if !touched || store.folder_row(far as usize).is_some() {
                // Either no read was owed, or the read got through despite
                // the armed fault. Neither is this test's case.
                return;
            }
            if opened {
                page_failures += 1;
            } else {
                open_failures += 1;
            }
            assert_eq!(
                store.folder_rows().len(),
                0,
                "a page the card would not read leaves the resident page \
                 empty, not holding the rows the reader scrolled away from",
            );
        });
    }
    assert!(
        page_failures > 0,
        "the sweep has to have failed at least one read inside the paging, \
         not only inside the opening ({open_failures} of those)",
    );
}

/// picked in the folder it left names a different child of a different place.
#[test]
fn a_relist_retires_the_rows_picked_before_it() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    seed_shelf(&root, 4);

    let mut store = Box::new(ReaderStore::new());
    reader_cache::browse::list_here(&mut store, &root, true).expect("root lists");
    let before = store.browse_epoch();
    let catalog_before = store.catalog_epoch();

    let folder = store.browse().count() - 1;
    assert!(matches!(
        reader_cache::browse::choose_row(&mut store, &root, folder, true),
        reader_cache::browse::RowChoice::Entered(_)
    ));
    assert_eq!(
        store.browse_epoch(),
        before,
        "a move the reader asked for is not a reposition",
    );

    reader_cache::browse::relist_root(&mut store, &root, true).expect("root lists");
    assert!(store.browse().is_root());
    assert_ne!(
        store.browse_epoch(),
        before,
        "the reposition retires the rows the reader was looking at",
    );
    assert_eq!(
        store.catalog_epoch(),
        catalog_before,
        "and it does so without the catalog having moved at all",
    );
}

/// A failed relist retires them too: it has left the folder either way, so a
/// row picked there is just as stale.
#[test]
fn a_relist_that_could_not_read_still_retires_them() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    seed_shelf(&root, 4);

    let mut refused = 0usize;
    for probe in 0..64 {
        let mut store = Box::new(ReaderStore::new());
        reader_cache::browse::list_here(&mut store, &root, true).expect("root lists");
        let folder = store.browse().count() - 1;
        assert!(matches!(
            reader_cache::browse::choose_row(&mut store, &root, folder, true),
            reader_cache::browse::RowChoice::Entered(_)
        ));
        let before = store.browse_epoch();

        disk.fault.fail_read_in.set(Some(probe));
        let listed = reader_cache::browse::relist_root(&mut store, &root, true);
        disk.fault.fail_read_in.set(None);

        assert_ne!(
            store.browse_epoch(),
            before,
            "probe {probe}: the folder was left whether or not the root read",
        );
        if listed.is_none() {
            refused += 1;
        }
    }
    assert!(refused > 0, "no read in the relist could be failed");
}

// ---------------------------------------------------------------------------
// Recognising a moved book.
//
// A cache key is derived from the locator, so a reader who tidies a book into
// a folder re-keys it away from its own position. The position survives on
// the card and becomes unreachable. The claim records what the file *is* so
// the sweep can put the two back together.

/// A digest of real bytes. `SourceDigest` is only constructible by hashing a
/// stream, which is the point of it: the writers below take that type
/// precisely so a caller cannot reach them with a value read off a card.
fn hashed(bytes: &[u8]) -> proto::source::SourceDigest {
    let mut hasher = proto::source::SourceHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// A book at `locator`, keyed as the firmware keys it, so the tests move real
/// keys rather than made-up ones.
fn moved_owner(locator: &str, size: u32) -> (String, proto::library_path::BookRoot) {
    let hash = proto::cache::source_hash_at(proto::library_path::BookRoot::Library, locator, size);
    (
        proto::cache::cache_key_from(hash).as_str().to_string(),
        proto::library_path::BookRoot::Library,
    )
}

/// A listing far longer than any number of batches somebody might write
/// down, walked one name at a time.
///
/// The point is the count of batches, so the batch is one and the listing is
/// thousands: what ends the walk has to be the end of the listing. A test
/// that merely runs deeper than one batch proves only that the ceiling is
/// higher than the test, which is how the ceiling this replaces survived
/// its own regression.
#[test]
fn no_number_of_batches_is_what_ends_the_walk() {
    let listing: Vec<String> = (0..4_000).map(|i| std::format!("E{i:07x}")).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut batches = 0usize;

    files::walk_in_batches(
        1,
        |skip, want, keys| {
            for name in listing.iter().skip(skip).take(want) {
                keys.push(heapless::String::try_from(name.as_str()).expect("8 bytes"))
                    .expect("within the batch");
            }
        },
        |_| true,
        |keys| {
            batches += 1;
            for key in keys {
                seen.push(key.as_str().to_string());
            }
        },
    );

    assert_eq!(
        batches,
        listing.len(),
        "one name per batch, and all of them"
    );
    assert_eq!(seen, listing, "in order, none skipped");
}

/// A listing that keeps naming an entry it reports as gone would be handed
/// over forever. The walk stops instead, on the batch that neither moved the
/// cursor nor changed the name it starts with.
#[test]
fn a_listing_that_contradicts_itself_stops_the_walk() {
    let listing: Vec<String> = (0..8).map(|i| std::format!("E{i:07x}")).collect();
    let mut handed = 0usize;

    files::walk_in_batches(
        2,
        |skip, want, keys| {
            for name in listing.iter().skip(skip).take(want) {
                keys.push(heapless::String::try_from(name.as_str()).expect("8 bytes"))
                    .expect("within the batch");
            }
        },
        // Everything is reported gone, and nothing leaves the listing.
        |_| false,
        |keys| {
            handed += keys.len();
            // Without the guard this walk does not end. Say so as a failure
            // rather than as a test run that hangs.
            assert!(
                handed <= 8,
                "the walk is handing the same names over forever"
            );
        },
    );

    assert_eq!(
        handed, 4,
        "two batches of the same names, and then it gives up",
    );
}

/// A folder with more rows than a cursor can address is refused, not
/// truncated. Clamping would publish a listing that stops at 65,535 and
/// leave every child after it unreachable, which is the failure the catalog
/// refuses an oversized library to avoid. Folders count their
/// subdirectories as rows, so a card whose catalog is comfortably legal can
/// still hold a folder past this.
#[test]
fn a_folder_past_the_cursor_is_refused_rather_than_shortened() {
    assert_eq!(reader_cache::browse::addressable_rows(0), Some(0));
    assert_eq!(
        reader_cache::browse::addressable_rows(u16::MAX as usize),
        Some(u16::MAX),
        "exactly full still lists"
    );
    assert_eq!(
        reader_cache::browse::addressable_rows(u16::MAX as usize + 1),
        None,
        "one past it hides a child, so it refuses"
    );
}

/// Every cache directory is reached, including the ones past a batch.
///
/// The walk holds a few dozen names at a time and has to restart
/// enumeration for each batch, because files cannot be opened while a
/// directory iteration holds the lock. Without a cursor it hands over the
/// same first names on every scan, and the sweep that consumes it is what
/// reconciles a moved book: a directory sitting past the first batch behind
/// perfectly healthy ones would be reconciled on no scan at all, and its
/// reader's place would stay stranded under the old key for the life of the
/// card.
#[test]
fn the_walk_reaches_past_its_first_batch() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    // Several batches deep, not one. A walk capped at any fixed number of
    // batches is the same starvation further out, so what this pins is that
    // the count of batches is not what stops it.
    let wanted = files::CACHE_SWEEP_BATCH * 5 + 1;
    let mut made: Vec<String> = Vec::new();
    for index in 0..wanted {
        let locator = std::format!("Book {index:03}.epub");
        let (key, book_root) = moved_owner(&locator, 4096);
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: book_root,
            locator: &locator,
        };
        // A position makes the directory one nothing reclaims, which is the
        // shape that starves the walk: healthy directories that stay.
        files::write_position_file(&root, &owner, 1, 1).expect("seed");
        made.push(key);
    }

    let mut seen: Vec<String> = Vec::new();
    let mut batches = 0;
    files::for_each_cache_dir(&root, files::CACHE_SWEEP_BATCH, |keys| {
        batches += 1;
        for key in keys {
            seen.push(key.as_str().to_string());
        }
    });

    assert!(batches > 1, "the point is that it took more than one batch");
    seen.sort();
    made.sort();
    assert_eq!(seen.len(), wanted, "every directory was handed over once");
    assert_eq!(seen, made, "and they were the ones on the card");
}

/// A caller that reclaims directories shortens the listing behind it. The
/// cursor counts what survived rather than what it asked about, or the
/// entries that moved up into the gap would be stepped over.
#[test]
fn reclaiming_inside_the_walk_does_not_skip_what_moves_up() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let wanted = files::CACHE_SWEEP_BATCH + 3;
    for index in 0..wanted {
        let locator = std::format!("Book {index:03}.epub");
        let (key, book_root) = moved_owner(&locator, 4096);
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: book_root,
            locator: &locator,
        };
        // A claim and no place to keep, so reclaiming takes the whole
        // directory rather than leaving one behind for its position.
        files::record_cache_evidence(&root, &owner, Some(9), Some(hashed(b"x"))).expect("seed");
    }

    // Reclaim each directory as it is handed over, the way the sweep does
    // for a cache whose book is gone. Every one leaves the listing, so a
    // cursor counting what it asked about rather than what survived would
    // walk straight past the entries that moved up.
    let mut seen = 0usize;
    files::for_each_cache_dir(&root, files::CACHE_SWEEP_BATCH, |keys| {
        seen += keys.len();
        for key in keys {
            assert!(
                files::empty_cache_dir(&root, key.as_str()),
                "a claim-only directory is reclaimable"
            );
            assert!(
                !files::book_dir_exists(&root, key.as_str()),
                "and it left the listing"
            );
        }
    });

    assert_eq!(
        seen, wanted,
        "every directory was handed over even as the listing shrank under the walk",
    );
}

/// A book that is sitting right there, probed through a card that stumbles.
///
/// This answer decides whether a directory enters move reconciliation, and
/// reconciliation ends by retiring a claim. Worse, the search that follows
/// excludes the owner's own locator, so a live book falsely called gone
/// cannot be found again; what can be found is a byte-identical copy
/// somewhere else, which is the state merge this layer exists to refuse. So
/// no read fault may turn a book that is present into one that left.
#[test]
fn a_read_fault_cannot_report_a_present_book_as_departed() {
    let mut stumbled = 0;
    // The probe costs three reads. The range runs well past that so a change
    // that adds one is still covered; the extra rounds fault nothing and
    // assert the clean answer.
    for probe in 0..24 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        seed_shelf(&root, 1);

        let locator = "Fiction/Book 000.epub";
        let size = 1;
        let key = proto::cache::cache_key_from(proto::cache::source_hash_at(
            proto::library_path::BookRoot::Library,
            locator,
            size,
        ));

        // With no fault at all it is plainly there, or the probe below
        // proves nothing about faults.
        assert_eq!(
            files::claimant_place(
                &root,
                proto::library_path::BookRoot::Library,
                locator,
                key.as_str()
            ),
            files::ClaimantPlace::Live,
            "the seeded book reads as present",
        );

        disk.fault.fail_read_in.set(Some(probe));
        let seen = files::claimant_place(
            &root,
            proto::library_path::BookRoot::Library,
            locator,
            key.as_str(),
        );
        disk.fault.fail_read_in.set(None);

        assert_ne!(
            seen,
            files::ClaimantPlace::Gone,
            "probe {probe}: a book on the card reported as departed",
        );
        if seen == files::ClaimantPlace::Unreadable {
            stumbled += 1;
        }
    }
    assert!(
        stumbled > 0,
        "no probe made the card stumble, so this proves nothing",
    );
}

/// A book that really is not there is not a card that stumbled. Reclaiming
/// a deleted book's cache depends on saying so.
#[test]
fn a_book_that_is_gone_reads_as_gone() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    seed_shelf(&root, 1);

    assert_eq!(
        files::claimant_place(
            &root,
            proto::library_path::BookRoot::Library,
            "Fiction/Not Here.epub",
            "E0000000"
        ),
        files::ClaimantPlace::Gone,
    );
}

/// A move whose destination lands on the departed book's own cache key.
///
/// A key is 28 bits of a hash of a place, so two locators can share one, and
/// `proto` carries a pair that collide outright. Then the place the reader
/// left is already in the directory the moved book will use, and there is
/// nothing to carry: what is wrong is the claim, which still names a locator
/// that is gone. Left alone, the next open reads that claim as another
/// book's, adopts the directory, and clears the position on the way in.
///
/// So the carry re-attributes in place. The gate that would refuse this,
/// seeing a stranger, is the one the confirmed digest exists to answer.
#[test]
fn a_move_onto_its_own_key_keeps_the_place_by_changing_whose_it_is() {
    let size = 1_234_567;
    let from_locator = "Fiction/zIx6RBhQEK.epub";
    let to_locator = "Fiction/nTfOyBwYzX.epub";
    let (from_key, book_root) = moved_owner(from_locator, size);
    let (to_key, _) = moved_owner(to_locator, size);
    assert_eq!(
        from_key, to_key,
        "the collision this test exists for; proto pins the pair"
    );

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let from = proto::cache::CacheOwner {
        key: from_key.as_str(),
        root: book_root,
        locator: from_locator,
    };
    let to = proto::cache::CacheOwner {
        key: to_key.as_str(),
        root: book_root,
        locator: to_locator,
    };
    files::write_position_file(&root, &from, 12, 340).expect("a place to keep");

    assert_eq!(
        files::carry_position(&root, &from, &to, hashed(b"the book"), Some(9)),
        Ok(true),
    );

    assert_eq!(
        files::read_position_file(&root, &to),
        Some((12, 340)),
        "the moved book reads the place it left",
    );
    match files::read_book_dir_claimant(&root, to_key.as_str()) {
        files::DirClaimant::Claimed {
            locator, released, ..
        } => {
            assert_eq!(
                locator.as_str(),
                to_locator,
                "the directory is the new one's"
            );
            assert!(!released, "and it is held, not retired");
        }
        other => panic!("the directory should be claimed by the mover, got {other:?}"),
    }

    // The evidence goes with it, or the next move has no witness.
    let files::DirClaimant::Claimed { evidence, .. } =
        files::read_book_dir_claimant(&root, to_key.as_str())
    else {
        unreachable!()
    };
    assert!(evidence.digest.is_some(), "a witness for the next move");
}

/// The repair, end to end: a position written under one locator is readable
/// under the other, and the evidence goes with it so a second move still has
/// a witness. Losing that on the first carry is how this works once and then
/// silently stops.
#[test]
fn a_carried_position_arrives_with_the_evidence_that_found_it() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let (from_key, book_root) = moved_owner("Dune.epub", 4096);
    let (to_key, _) = moved_owner("Fiction/Dune.epub", 4096);
    let from = proto::cache::CacheOwner {
        key: from_key.as_str(),
        root: book_root,
        locator: "Dune.epub",
    };
    let to = proto::cache::CacheOwner {
        key: to_key.as_str(),
        root: book_root,
        locator: "Fiction/Dune.epub",
    };
    assert_ne!(
        from.key, to.key,
        "a move re-keys the book, which is the bug"
    );

    files::write_position_file(&root, &from, 12, 340).expect("seed a position");
    let digest = hashed(b"the bytes this copy holds");
    assert!(files::carry_position(&root, &from, &to, digest, Some(77)).expect("carry"));
    assert_eq!(
        files::read_position_file(&root, &to),
        Some((12, 340)),
        "the reader resumes where they left off, under the new key",
    );

    match files::read_book_dir_claimant(&root, to.key) {
        files::DirClaimant::Claimed {
            locator, evidence, ..
        } => {
            assert_eq!(locator.as_str(), "Fiction/Dune.epub");
            assert_eq!(evidence.cluster, Some(77));
            assert_eq!(
                evidence.digest,
                Some(CachedSourceDigest::new(digest)),
                "a second move still has a witness"
            );
        }
        other => panic!("the carried directory must be claimed: {other:?}"),
    }
}

/// A departed directory with nothing in it is the ordinary case, and it is
/// not a failure. Reporting one as carried would have the sweep believe it
/// repaired something.
#[test]
fn a_departed_directory_with_no_position_carries_nothing() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let (from_key, book_root) = moved_owner("Dune.epub", 4096);
    let (to_key, _) = moved_owner("Fiction/Dune.epub", 4096);
    let from = proto::cache::CacheOwner {
        key: from_key.as_str(),
        root: book_root,
        locator: "Dune.epub",
    };
    let to = proto::cache::CacheOwner {
        key: to_key.as_str(),
        root: book_root,
        locator: "Fiction/Dune.epub",
    };
    assert!(!files::carry_position(&root, &from, &to, hashed(b"anything"), None).expect("carry"));
    assert_eq!(files::read_position_file(&root, &to), None);
}

/// Evidence arrives in two pieces at two different times: the chain when the
/// book is resolved, the digest when it is read. Recording one must not erase
/// the other, and neither must an ordinary position write.
#[test]
fn evidence_accumulates_and_ordinary_writes_leave_it_alone() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    files::record_cache_evidence(&root, &OWNER, Some(512), None).expect("chain first");
    let digest = hashed(b"some other bytes");
    files::record_cache_evidence(&root, &OWNER, None, Some(digest)).expect("digest after");

    let held = match files::read_book_dir_claimant(&root, OWNER.key) {
        files::DirClaimant::Claimed { evidence, .. } => evidence,
        other => panic!("claimed: {other:?}"),
    };
    assert_eq!(
        held.cluster,
        Some(512),
        "the digest did not erase the chain"
    );
    assert_eq!(held.digest, Some(CachedSourceDigest::new(digest)));

    // A position write re-claims the directory. It is about ownership, not
    // about what the file is, and must carry the evidence through untouched.
    files::write_position_file(&root, &OWNER, 4, 9).expect("position");
    let after = match files::read_book_dir_claimant(&root, OWNER.key) {
        files::DirClaimant::Claimed { evidence, .. } => evidence,
        other => panic!("claimed: {other:?}"),
    };
    assert_eq!(after, held, "an ordinary claim write is not an erasure");
}

/// The release is where the evidence has to survive. The sweep releases a
/// departed owner's claim, and that release rewrites the claim: if it wrote
/// an empty record it would destroy the witness in the very pass that needs
/// it, and a book that moved twice would be unrecognisable the second time.
#[test]
fn releasing_a_claim_keeps_the_evidence_the_sweep_reads() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let digest = hashed(b"bytes worth recognising later");
    files::record_cache_evidence(&root, &OWNER, Some(4_211), Some(digest)).expect("record");
    files::write_position_file(&root, &OWNER, 8, 100).expect("position");

    assert!(files::release_book_dir_claim(&root, OWNER.key), "release");

    match files::read_book_dir_claimant(&root, OWNER.key) {
        files::DirClaimant::Claimed {
            released, evidence, ..
        } => {
            assert!(released, "the sweep released it");
            assert_eq!(evidence.cluster, Some(4_211), "and kept what the file is");
            assert_eq!(evidence.digest, Some(CachedSourceDigest::new(digest)));
        }
        other => panic!("a released claim still names its owner: {other:?}"),
    }
    assert_eq!(
        files::read_position_file(&root, &OWNER),
        Some((8, 100)),
        "and the place it was protecting",
    );
}

/// A read fault during the release must not be read as a claim that had no
/// evidence. The release rewrites the claim in the very pass that wants to
/// recognise a move, so one unanswered read there would destroy the witness
/// on the way past, and the rewrite would report success having done it.
#[test]
fn a_read_fault_during_release_cannot_erase_the_evidence() {
    let digest = hashed(b"bytes worth recognising later");
    for probe in 0..30 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        files::record_cache_evidence(&root, &OWNER, Some(4_211), Some(digest)).expect("record");

        disk.fault.fail_read_in.set(Some(probe));
        let released = files::release_book_dir_claim(&root, OWNER.key);
        disk.fault.fail_read_in.set(None);

        // A fault can tear the claim mid-write, and a torn claim is not a
        // claim: the sweep reads it as unclaimed and nothing inherits from
        // it. What must not happen is a well-formed claim that survived with
        // its evidence stripped, because that reads as authoritative.
        if let files::DirClaimant::Claimed { evidence, .. } =
            files::read_book_dir_claimant(&root, OWNER.key)
        {
            assert_eq!(
                (evidence.cluster, evidence.digest),
                (Some(4_211), Some(CachedSourceDigest::new(digest))),
                "probe {probe}: released={released} left a valid claim with no witness",
            );
        }
    }
}

/// A departed book coming back reactivates its released claim, and that
/// rewrite is the one that has to re-read the evidence rather than being
/// handed it. A read fault there must refuse, not write a well-formed active
/// claim with the witness stripped out.
#[test]
fn a_returning_owner_does_not_reactivate_over_its_own_evidence() {
    let digest = hashed(b"the bytes this copy holds");
    for probe in 0..30 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        files::record_cache_evidence(&root, &OWNER, Some(4_211), Some(digest)).expect("record");
        assert!(files::release_book_dir_claim(&root, OWNER.key), "depart");

        // Coming back: the position write reclaims, which reactivates.
        disk.fault.fail_read_in.set(Some(probe));
        let back = files::write_position_file(&root, &OWNER, 3, 4);
        disk.fault.fail_read_in.set(None);

        if let files::DirClaimant::Claimed { evidence, .. } =
            files::read_book_dir_claimant(&root, OWNER.key)
        {
            assert_eq!(
                (evidence.cluster, evidence.digest),
                (Some(4_211), Some(CachedSourceDigest::new(digest))),
                "probe {probe}: back={back:?} reactivated over the witness",
            );
        }
    }
}

/// Adding one half of the evidence must not erase the other when the read of
/// what is already there fails.
#[test]
fn a_read_fault_while_accumulating_refuses_rather_than_forgets() {
    let digest = hashed(b"the second half");
    for probe in 0..30 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        files::record_cache_evidence(&root, &OWNER, Some(4_211), None).expect("chain");

        disk.fault.fail_read_in.set(Some(probe));
        let added = files::record_cache_evidence(&root, &OWNER, None, Some(digest));
        disk.fault.fail_read_in.set(None);

        if let files::DirClaimant::Claimed { evidence, .. } =
            files::read_book_dir_claimant(&root, OWNER.key)
        {
            assert_eq!(
                evidence.cluster,
                Some(4_211),
                "probe {probe}: added={added:?} left a valid claim missing the chain \
                 it was adding to",
            );
        }
    }
}

/// The readback has to prove the transition, not merely the evidence. A
/// release keeps its evidence identical by design, so a verifier comparing
/// evidence alone cannot tell whether the released bit ever landed.
#[test]
fn a_release_that_does_not_land_is_not_reported_as_landed() {
    let digest = hashed(b"unchanged across the release");
    for probe in 0..30 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        files::record_cache_evidence(&root, &OWNER, Some(7), Some(digest)).expect("record");

        disk.fault.fail_write_in.set(Some(probe));
        let released = files::release_book_dir_claim(&root, OWNER.key);
        disk.fault.fail_write_in.set(None);

        if released {
            match files::read_book_dir_claimant(&root, OWNER.key) {
                files::DirClaimant::Claimed {
                    released: on_card, ..
                } => assert!(
                    on_card,
                    "probe {probe}: reported a release the card did not take",
                ),
                other => panic!("probe {probe}: reported a release, card says {other:?}"),
            }
        }
    }
}

/// The presence question asks the directory, not the claim over it, and has
/// to answer in both directions: a built book that has not been read to a
/// place yet is a directory a carry may write, and one that remembers a
/// place is not.
#[test]
fn a_directory_holds_a_position_only_once_one_is_written() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let (key, book_root) = moved_owner("Dune.epub", 4096);
    let owner = proto::cache::CacheOwner {
        key: key.as_str(),
        root: book_root,
        locator: "Dune.epub",
    };

    assert_eq!(
        files::book_dir_position_presence(&root, key.as_str()),
        files::PositionPresence::Absent,
        "no directory at all"
    );

    files::record_cache_evidence(&root, &owner, Some(9), Some(hashed(b"the book")))
        .expect("claim the directory");
    assert_eq!(
        files::book_dir_position_presence(&root, key.as_str()),
        files::PositionPresence::Absent,
        "claimed, and nothing yet to lose"
    );

    files::write_position_file(&root, &owner, 12, 340).expect("a place to keep");
    assert_eq!(
        files::book_dir_position_presence(&root, key.as_str()),
        files::PositionPresence::Present,
        "a place a carry would destroy"
    );
}

/// A generation the card hands over whole, that is not a valid record, is
/// no place to return to. It reads as absent so a carry may clear it, which
/// is what lets a destination whose own carry tore half way be carried to
/// again. Only a card that would not answer is kept apart from absence.
#[test]
fn a_generation_that_reads_but_does_not_decode_is_no_place() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);

    let (key, book_root) = moved_owner("Dune.epub", 4096);
    let owner = proto::cache::CacheOwner {
        key: key.as_str(),
        root: book_root,
        locator: "Dune.epub",
    };
    files::write_position_file(&root, &owner, 12, 340).expect("a place to keep");
    assert_eq!(
        files::book_dir_position_presence(&root, key.as_str()),
        files::PositionPresence::Present,
    );

    // Rewrite the generation at its own length, so it is whole and wrong
    // rather than short. A write fault leaves the short kind; this is the
    // kind a write fault cannot make.
    {
        let mut book = root.open_dir("READER").expect("reader dir");
        book.change_dir("CACHE2").expect("cache dir");
        book.change_dir(key.as_str()).expect("book dir");
        let len = book
            .open_file_in_dir("POSA.BIN", embedded_sdmmc::Mode::ReadOnly)
            .expect("the generation just written")
            .length() as usize;
        let file = book
            .open_file_in_dir("POSA.BIN", embedded_sdmmc::Mode::ReadWriteCreateOrTruncate)
            .expect("rewrite it");
        file.write(&vec![0xAA; len])
            .expect("same length, no record");
    }

    assert_eq!(
        files::book_dir_position_presence(&root, key.as_str()),
        files::PositionPresence::Absent,
        "whole, and not a record anyone can return to",
    );
}

/// The answer that matters is the one given about a place that is really
/// there. Reconciliation reads "no position" as permission to write, and
/// writing adopts the directory, which deletes what was in it. So a card
/// that stumbles while being asked has to say so: a position that exists is
/// allowed to read as Present or as Unreadable, and not as Absent.
///
/// Read faults rather than write faults, because the destructive path here
/// opens with a read. Nothing is written in this test at all.
#[test]
fn a_read_fault_cannot_report_a_position_that_exists_as_absent() {
    let mut stumbled = 0;
    for probe in 0..24 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);

        let (key, book_root) = moved_owner("Dune.epub", 4096);
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: book_root,
            locator: "Dune.epub",
        };
        files::write_position_file(&root, &owner, 12, 340).expect("a place to keep");

        disk.fault.fail_read_in.set(Some(probe));
        let seen = files::book_dir_position_presence(&root, key.as_str());
        disk.fault.fail_read_in.set(None);

        assert_ne!(
            seen,
            files::PositionPresence::Absent,
            "probe {probe}: a place that is on the card read as no place at all",
        );
        if seen == files::PositionPresence::Unreadable {
            stumbled += 1;
        }
    }
    assert!(
        stumbled > 0,
        "no probe made the card stumble, so this proves nothing",
    );
}

/// A carry that does not land has to leave the retry possible, and the
/// destination is where that can quietly stop being true. The claim is
/// written before the position, so an interruption in between leaves a
/// claimed directory holding nothing. Reconciliation used to read any claim
/// as somebody's place and skip the row, which made a torn carry lock out
/// the only path that could finish it, on a card where the reader's place
/// is the one thing that cannot be rebuilt.
///
/// So every landing converges: the destination holds exactly the carried
/// place, or it holds none and is open to being carried to again.
#[test]
fn an_interrupted_carry_leaves_its_destination_open_to_the_retry() {
    let mut trap = 0;
    for probe in 0..40 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);

        let (from_key, book_root) = moved_owner("Dune.epub", 4096);
        let (to_key, _) = moved_owner("Fiction/Dune.epub", 4096);
        let from = proto::cache::CacheOwner {
            key: from_key.as_str(),
            root: book_root,
            locator: "Dune.epub",
        };
        let to = proto::cache::CacheOwner {
            key: to_key.as_str(),
            root: book_root,
            locator: "Fiction/Dune.epub",
        };
        files::write_position_file(&root, &from, 12, 340).expect("seed");

        disk.fault.fail_write_in.set(Some(probe));
        let carried = files::carry_position(&root, &from, &to, hashed(b"the book"), Some(9));
        disk.fault.fail_write_in.set(None);

        let landed = files::read_position_file(&root, &to);
        let occupied = files::book_dir_position_presence(&root, to_key.as_str())
            != files::PositionPresence::Absent;
        assert!(
            landed == Some((12, 340)) || !occupied,
            "probe {probe}: destination neither carried nor open to a retry",
        );

        // The state the skip-any-claim rule used to strand: claimed, and
        // empty of any place to keep.
        if carried != Ok(true)
            && !occupied
            && matches!(
                files::read_book_dir_claimant(&root, to_key.as_str()),
                files::DirClaimant::Claimed { .. }
            )
        {
            trap += 1;
        }
    }
    assert!(
        trap > 0,
        "no probe produced a claimed destination with no position, so this proves nothing",
    );
}

/// Under a fault anywhere in the carry, reporting success has to mean the
/// position really is readable at the new key. Reporting failure is always
/// safe: nothing here deletes the old copy, so the place is still where it
/// was and the next scan can try again.
#[test]
fn a_carry_that_reports_success_really_carried() {
    for probe in 0..40 {
        let disk = new_card();
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);

        let (from_key, book_root) = moved_owner("Dune.epub", 4096);
        let (to_key, _) = moved_owner("Fiction/Dune.epub", 4096);
        let from = proto::cache::CacheOwner {
            key: from_key.as_str(),
            root: book_root,
            locator: "Dune.epub",
        };
        let to = proto::cache::CacheOwner {
            key: to_key.as_str(),
            root: book_root,
            locator: "Fiction/Dune.epub",
        };
        files::write_position_file(&root, &from, 12, 340).expect("seed");

        disk.fault.fail_write_in.set(Some(probe));
        let carried = files::carry_position(&root, &from, &to, hashed(b"the book"), Some(9));
        disk.fault.fail_write_in.set(None);

        if carried == Ok(true) {
            assert_eq!(
                files::read_position_file(&root, &to),
                Some((12, 340)),
                "probe {probe}: reported a carry that did not land",
            );
        }
        assert_eq!(
            files::read_position_file(&root, &from),
            Some((12, 340)),
            "probe {probe}: the carry is not allowed to be destructive",
        );
    }
}

/// Two locators of one size whose cache keys agree, found by searching
/// rather than made up: the key is 28 bits of a hash of the place, so a
/// pair turns up among a few tens of thousands of names.
fn colliding_locators(size: u32) -> (std::string::String, std::string::String) {
    use std::collections::HashMap;
    let mut seen: HashMap<std::string::String, std::string::String> = HashMap::new();
    for n in 0..400_000u32 {
        let locator = std::format!("Book{n:06}.epub");
        let key = moved_owner(&locator, size).0;
        if let Some(first) = seen.insert(key, locator.clone()) {
            return (first, locator);
        }
    }
    panic!("no pair of locators shared a cache key");
}

/// A move whose two places share a cache directory is declined rather than
/// carried.
///
/// Carrying it would mean re-attributing that one directory to the new
/// locator, and the claim on it is where a book nobody uploaded records
/// what its bytes were. This runs before the ledger has written the move
/// down, on purpose, so that a reset retries it. Rewriting the claim first
/// would leave a ledger naming the old place and a claim naming the new
/// one, and the retry would find no evidence and mint the copy afresh. The
/// reading place is lost instead, which the bridge already allows for.
#[test]
fn a_move_that_shares_a_cache_directory_leaves_the_evidence_alone() {
    let size = 3_000u32;
    let (was_locator, now_locator) = colliding_locators(size);
    let (key, root_kind) = moved_owner(&was_locator, size);
    assert_eq!(
        key,
        moved_owner(&now_locator, size).0,
        "the search found a pair that share a directory"
    );

    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let was = proto::cache::CacheOwner {
        key: key.as_str(),
        root: root_kind,
        locator: was_locator.as_str(),
    };
    let now = proto::cache::CacheOwner {
        key: key.as_str(),
        root: root_kind,
        locator: now_locator.as_str(),
    };

    let digest = hashed(b"what this copy is");
    files::record_cache_evidence(&root, &was, None, Some(digest)).expect("record");
    files::write_position_file(&root, &was, 4, 40).expect("position");

    assert_eq!(
        files::carry_position_for_move(&root, &was, &now),
        Ok(false),
        "nothing is carried where one directory serves both places"
    );

    match files::read_book_dir_claimant(&root, was.key) {
        files::DirClaimant::Claimed {
            locator, evidence, ..
        } => {
            assert_eq!(
                locator.as_str(),
                was_locator.as_str(),
                "the claim still names the copy the ledger names"
            );
            assert_eq!(
                evidence.digest,
                Some(CachedSourceDigest::new(digest)),
                "and still says what its bytes were, for the scan that retries"
            );
        }
        other => panic!("the claim is where it was: {other:?}"),
    }
}
