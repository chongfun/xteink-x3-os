//! The library ledger against a real FAT filesystem.
//!
//! The format is unit-tested in `proto::identity`. What is checked here is
//! the part that only a card can answer: that a generation lands whole or
//! not at all, that a power cut anywhere from the first ledger write to the
//! catalog's own commit leaves one of the two legal states and nothing in
//! between, that a committed generation that does not read back whole is
//! refused rather than fallen back from, that missing copies are retained
//! for a bounded number of scans, and that the join between a freshly
//! written catalog and the ledger gives each row the id it had and mints
//! for the rest.
//!
//! The card model is the one `install_disk.rs` uses: a RAM image whose
//! writes can be cut off from a chosen point onward, which is the one thing
//! a reset guarantees and the only thing a test may assume.

use std::cell::RefCell;
use std::rc::Rc;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, Mode, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use proto::cache::{source_hash_at, CACHE_ROOT_DIR, CATALOG_FILE};
use proto::catalog::{
    catalog_record_book_id, decode_catalog_header, encode_catalog_header,
    encode_catalog_placeholder_header, encode_catalog_record, CATALOG_HEADER_BYTES,
    CATALOG_RECORD_BYTES,
};
use proto::identity::{
    classify_ledger_journal, BookId, LedgerJournal, LedgerJournalReading, LedgerRecord,
    LEDGER_HEADER_BYTES, LEDGER_JOURNAL_BYTES, LEDGER_JOURNAL_FILE_BYTES, LEDGER_JOURNAL_SLOTS,
    LEDGER_JOURNAL_SLOT_BYTES, LEDGER_RECORD_BYTES, MISSING_SCANS_RETAINED, ROW_KEY_BYTES,
};
use proto::library_path::BookRoot;
use upload_store::ledger::{
    self, Assignment, Carry, Kept, LedgerFault, LEDGER_FILES, LEDGER_JOURNAL,
};

const BLOCK_BYTES: usize = 512;
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

struct RamDisk {
    data: RefCell<Vec<u8>>,
    /// Writes from this number onward do nothing and report failure.
    fail_writes_from: RefCell<Option<u32>>,
    writes_seen: RefCell<u32>,
    /// The `n`th write lands only its first `k` bytes and then the power
    /// goes: a sector torn in the middle of being written, which is the one
    /// thing a checksum on a single-sector record exists to catch.
    tear_write_at: RefCell<Option<(u32, usize)>>,
    /// The first block of every write since the last arming, in order, so
    /// a test can find which writes touched a given file.
    written_blocks: RefCell<Vec<u32>>,
}

#[derive(Debug)]
struct DiskError;

impl core::fmt::Display for DiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "injected disk error")
    }
}

impl std::error::Error for DiskError {}

#[derive(Clone)]
struct SharedDisk(Rc<RamDisk>);

impl SharedDisk {
    fn image(&self) -> Vec<u8> {
        self.0.data.borrow().clone()
    }

    fn restore(&self, image: &[u8]) {
        self.0.data.borrow_mut().copy_from_slice(image);
    }

    /// Cut the power before the `n`th write from now on.
    fn cut_writes_from(&self, n: Option<u32>) {
        *self.0.writes_seen.borrow_mut() = 0;
        *self.0.fail_writes_from.borrow_mut() = n;
        *self.0.tear_write_at.borrow_mut() = None;
        self.0.written_blocks.borrow_mut().clear();
    }

    /// Tear the `n`th write from now on after `k` bytes of its first block,
    /// and cut the power there.
    fn tear_write_at(&self, n: u32, k: usize) {
        self.cut_writes_from(None);
        *self.0.tear_write_at.borrow_mut() = Some((n, k));
    }

    fn writes_seen(&self) -> u32 {
        *self.0.writes_seen.borrow()
    }

    fn written_blocks(&self) -> Vec<u32> {
        self.0.written_blocks.borrow().clone()
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        let data = self.0.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        let seen = {
            let mut seen = self.0.writes_seen.borrow_mut();
            *seen += 1;
            *seen
        };
        self.0.written_blocks.borrow_mut().push(start.0);
        if let Some((n, k)) = *self.0.tear_write_at.borrow() {
            if seen == n {
                let mut data = self.0.data.borrow_mut();
                let at = start.0 as usize * BLOCK_BYTES;
                data[at..at + k].copy_from_slice(&blocks[0][..k]);
                *self.0.fail_writes_from.borrow_mut() = Some(seen);
                return Err(DiskError);
            }
        }
        if let Some(from) = *self.0.fail_writes_from.borrow() {
            if seen >= from {
                return Err(DiskError);
            }
        }
        let mut data = self.0.data.borrow_mut();
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
type CardFile<'a> = embedded_sdmmc::File<'a, SharedDisk, StaticTime, 8, 8, 1>;

fn format_disk() -> Vec<u8> {
    let mut disk = vec![0u8; DISK_BLOCKS as usize * BLOCK_BYTES];
    let part_blocks = DISK_BLOCKS - PART_START_BLOCK;
    let mut partition = vec![0u8; part_blocks as usize * BLOCK_BYTES];
    fatfs::format_volume(
        std::io::Cursor::new(partition.as_mut_slice()),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat16),
    )
    .expect("format");
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

fn new_card() -> SharedDisk {
    SharedDisk(Rc::new(RamDisk {
        data: RefCell::new(format_disk()),
        fail_writes_from: RefCell::new(None),
        writes_seen: RefCell::new(0),
        tear_write_at: RefCell::new(None),
        written_blocks: RefCell::new(Vec::new()),
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

/// A deterministic word source for minting. Not random, and that is the
/// point: the tests assert on which ids come back, not on entropy.
fn entropy() -> impl FnMut() -> u32 {
    let mut state = 0x2545_F491u32;
    move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state
    }
}

/// One row of the catalog a scan would write: where the book is and how big.
type Row = (BookRoot, &'static str, u32);

fn read_exact(file: &CardFile<'_>, mut out: &mut [u8]) -> bool {
    while !out.is_empty() {
        let Ok(read) = file.read(out) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        let rest = out;
        out = &mut rest[read..];
    }
    true
}

/// The production scan, as far as the ledger is concerned: write `rows` as
/// a fresh, uncommitted `CATALOG.BIN`, call `arm` (which is where a test
/// cuts the power), open the ledger, run the identity join over the rows
/// with a scratch of `scratch_len` bytes, and then commit the catalog header
/// the way `write_catalog_streaming` does. Hands back what the join reported
/// and the id every row ended up carrying. Any write refused after `arm` is
/// reported as the card, the way the firmware would see it.
fn scan(
    root: &Dir<'_>,
    rows: &[Row],
    scratch_len: usize,
    random: &mut impl FnMut() -> u32,
    arm: impl FnOnce(),
) -> Result<(Assignment, Vec<Option<BookId>>), LedgerFault> {
    if root.open_dir(CACHE_ROOT_DIR).is_err() {
        root.make_dir_in_dir(CACHE_ROOT_DIR).expect("mkdir READER");
    }
    let cache_root = root.open_dir(CACHE_ROOT_DIR).expect("open READER");
    let file = cache_root
        .open_file_in_dir(CATALOG_FILE, Mode::ReadWriteCreateOrTruncate)
        .expect("create catalog");
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    encode_catalog_placeholder_header(&mut header);
    file.write(&header).expect("placeholder");
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    for (at, locator, size) in rows {
        encode_catalog_record(
            &mut record,
            locator,
            *at,
            locator,
            "",
            "",
            *size,
            source_hash_at(*at, locator, *size),
        );
        file.write(&record).expect("row");
    }
    arm();
    let ledger = ledger::open(root)?;
    let mut scratch = vec![0u8; scratch_len];
    let assigned = ledger::assign_book_ids(
        root,
        &file,
        rows.len() as u16,
        &mut scratch,
        random,
        ledger,
        &mut |_| {},
    )?;
    encode_catalog_header(rows.len() as u16, &mut header);
    file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
    file.write(&header).map_err(|_| LedgerFault::Device)?;
    file.flush().map_err(|_| LedgerFault::Device)?;
    let mut ids = Vec::with_capacity(rows.len());
    for index in 0..rows.len() {
        file.seek_from_start((CATALOG_HEADER_BYTES + index * CATALOG_RECORD_BYTES) as u32)
            .expect("seek row");
        assert!(read_exact(&file, &mut record), "row {index} reads back");
        ids.push(catalog_record_book_id(&record));
    }
    Ok((assigned, ids))
}

const ARENA: usize = 16 * 1024;

/// Every record of the live generation, in ledger order.
fn records(root: &Dir<'_>) -> Vec<(BookRoot, String, u32, BookId, u8)> {
    let mut out = Vec::new();
    if let Some(live) = ledger::open(root).expect("open ledger") {
        ledger::for_each_record(root, &live, &mut |_, record: &LedgerRecord<'_>| {
            out.push((
                record.root,
                record.locator.to_owned(),
                record.byte_size,
                record.id,
                record.misses,
            ));
            Ok(())
        })
        .expect("read ledger");
    }
    out
}

fn ids_of(records: &[(BookRoot, String, u32, BookId, u8)]) -> Vec<BookId> {
    records.iter().map(|record| record.3).collect()
}

/// The whole of one side's file, for asserting a scan left it alone.
fn ledger_file_bytes(root: &Dir<'_>, side: usize) -> Vec<u8> {
    let cache_root = root.open_dir(CACHE_ROOT_DIR).expect("open READER");
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[side], Mode::ReadOnly)
        .expect("open ledger side");
    let mut out = vec![0u8; file.length() as usize];
    assert!(read_exact(&file, &mut out));
    out
}

fn generation(root: &Dir<'_>) -> Option<(u32, u16, &'static str)> {
    ledger::open(root)
        .expect("open ledger")
        .map(|live| (live.generation, live.count, live.file_name()))
}

/// What the journal says: the newest entry that reads, or `None` for no
/// journal.
fn journal(root: &Dir<'_>) -> Option<LedgerJournal> {
    ledger::read_journal(root).expect("read journal")
}

/// What each of the journal's slots says, read raw.
fn journal_slots(root: &Dir<'_>) -> [LedgerJournalReading; LEDGER_JOURNAL_SLOTS] {
    let bytes = journal_bytes(root);
    assert_eq!(bytes.len(), LEDGER_JOURNAL_FILE_BYTES);
    let mut slots = [LedgerJournalReading::Blank; LEDGER_JOURNAL_SLOTS];
    for (index, slot) in slots.iter_mut().enumerate() {
        let at = index * LEDGER_JOURNAL_SLOT_BYTES;
        let mut entry = [0u8; LEDGER_JOURNAL_BYTES];
        entry.copy_from_slice(&bytes[at..at + LEDGER_JOURNAL_BYTES]);
        *slot = classify_ledger_journal(&entry);
    }
    slots
}

/// Overwrite the journal's bytes in place, as damage would.
fn overwrite_journal(root: &Dir<'_>, at: usize, bytes: &[u8]) {
    let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
    let file = cache_root
        .open_file_in_dir(LEDGER_JOURNAL, Mode::ReadWriteAppend)
        .unwrap();
    file.seek_from_start(at as u32).unwrap();
    file.write(bytes).unwrap();
    file.close().unwrap();
}

fn journal_bytes(root: &Dir<'_>) -> Vec<u8> {
    let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
    let file = cache_root
        .open_file_in_dir(LEDGER_JOURNAL, Mode::ReadOnly)
        .unwrap();
    let mut out = vec![0u8; file.length() as usize];
    assert!(read_exact(&file, &mut out));
    out
}

/// The ids in the committed catalog, or `None` while its header is still the
/// placeholder.
fn committed_catalog_ids(root: &Dir<'_>) -> Option<Vec<Option<BookId>>> {
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let file = cache_root
        .open_file_in_dir(CATALOG_FILE, Mode::ReadOnly)
        .ok()?;
    // What `with_catalog_file` accepts: a header that decodes over a file
    // exactly as long as it says. Anything else is a rebuild, not a catalog.
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    if !read_exact(&file, &mut header) {
        return None;
    }
    let count = decode_catalog_header(&header)?;
    if file.length() as usize != proto::catalog::catalog_file_len(count) {
        return None;
    }
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    let mut ids = Vec::with_capacity(count as usize);
    for _ in 0..count {
        assert!(read_exact(&file, &mut record));
        ids.push(catalog_record_book_id(&record));
    }
    Some(ids)
}

const SHELF: [Row; 5] = [
    (BookRoot::Library, "Dune.epub", 900_001),
    (BookRoot::Library, "Fiction/Dune.epub", 900_001),
    (BookRoot::CardRoot, "Dune.epub", 900_001),
    (BookRoot::Library, "History/Rome/SPQR.epub", 1_234_567),
    (BookRoot::Library, "Reference/Atlas.epub", 40_000_000),
];

#[test]
fn a_fresh_card_has_no_ledger_and_an_empty_scan_makes_none() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    assert_eq!(generation(&root), None);
    let (assigned, ids) = scan(&root, &[], ARENA, &mut entropy(), || {}).unwrap();
    assert_eq!(assigned, Assignment::default());
    assert!(ids.is_empty());
    assert_eq!(generation(&root), None, "nothing to adopt writes nothing");
}

/// The first scan mints; identical bytes at two places, and the same
/// locator under two roots, are separate books. The next scan of the same
/// card finds every row named and writes nothing.
#[test]
fn a_first_scan_mints_distinct_ids_and_a_second_keeps_them_without_writing() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();

    let (assigned, ids) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        assigned,
        Assignment {
            minted: 5,
            ..Assignment::default()
        }
    );
    let ids: Vec<BookId> = ids
        .into_iter()
        .map(|id| id.expect("every row adopted"))
        .collect();
    let mut distinct = ids.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 5, "three same-sized Dunes are three books");
    assert_eq!(generation(&root), Some((1, 5, LEDGER_FILES[0])));
    assert_eq!(committed_catalog_ids(&root).unwrap().len(), 5);
    let written = ledger_file_bytes(&root, 0);

    // The same card, listed in another order.
    let mut shuffled = SHELF;
    shuffled.reverse();
    let (again, again_ids) = scan(&root, &shuffled, ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        again,
        Assignment {
            matched: 5,
            ..Assignment::default()
        }
    );
    for (index, id) in again_ids.into_iter().enumerate() {
        assert_eq!(id, Some(ids[4 - index]), "row {index} kept its id");
    }
    // Only the catalog changed; not one ledger byte did, and no second
    // side was started.
    assert_eq!(generation(&root), Some((1, 5, LEDGER_FILES[0])));
    assert_eq!(ledger_file_bytes(&root, 0), written);
    assert!(root
        .open_dir(CACHE_ROOT_DIR)
        .unwrap()
        .open_file_in_dir(LEDGER_FILES[1], Mode::ReadOnly)
        .is_err());
    assert_eq!(
        ids_of(&records(&root)),
        ids,
        "the ledger names the copies in adoption order"
    );
}

/// A row the ledger does not name by place and size is a new copy to this
/// milestone. The record of the copy that is no longer where it was stays,
/// one scan older, so the reconciliation that recognises a move has
/// something to match.
#[test]
fn a_moved_or_resized_copy_is_minted_afresh_and_the_old_record_stays() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    let (_, before) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();

    let mut edited = SHELF;
    edited[3].1 = "History/SPQR.epub";
    edited[4].2 += 1;
    let (assigned, after) = scan(&root, &edited, ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        assigned,
        Assignment {
            matched: 3,
            minted: 2,
            missing: 2,
            ..Assignment::default()
        }
    );
    let (kept, fresh) = after.split_at(3);
    assert_eq!(kept, &before[..3], "untouched rows kept their ids");
    for (index, id) in fresh.iter().enumerate() {
        assert!(id.is_some());
        assert!(
            !before.contains(id),
            "row {} is a copy nobody has seen",
            index + 3
        );
    }
    assert_eq!(generation(&root), Some((2, 7, LEDGER_FILES[1])));
    let live = records(&root);
    assert_eq!(live.len(), 7);
    let departed = live
        .iter()
        .find(|(_, locator, _, id, _)| {
            locator == "History/Rome/SPQR.epub" && Some(*id) == before[3]
        })
        .expect("the departed copy's record stands");
    assert_eq!(departed.4, 1, "one scan missing");
    assert!(
        live.iter()
            .filter(|(_, _, _, id, _)| before[..3].contains(&Some(*id)))
            .all(|record| record.4 == 0),
        "live copies carry no misses"
    );
}

/// Two legal locators share a 32-bit place hash. The hash is the join's
/// candidate filter, and the locator is what decides.
#[test]
fn colliding_place_hashes_are_told_apart_by_the_locator() {
    let size = 1_234_567;
    let twins: [Row; 2] = [
        (BookRoot::Library, "Fiction/zIx6RBhQEK.epub", size),
        (BookRoot::Library, "Fiction/nTfOyBwYzX.epub", size),
    ];
    assert_eq!(
        source_hash_at(twins[0].0, twins[0].1, size),
        source_hash_at(twins[1].0, twins[1].1, size),
        "the collision this test exists for"
    );
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    let (_, first) = scan(&root, &twins, ARENA, &mut random, || {}).unwrap();
    assert_ne!(first[0], first[1]);

    let mut swapped = twins;
    swapped.reverse();
    let (assigned, second) = scan(&root, &swapped, ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.matched, 2);
    assert_eq!(assigned.minted, 0);
    assert_eq!(second[0], first[1]);
    assert_eq!(second[1], first[0]);
}

/// The scratch bounds the slice, not the library: a bitmap byte and two
/// keys' worth of scratch joins five rows in three passes over the ledger.
/// Too little for even that is refused rather than looped over.
#[test]
fn rows_beyond_one_scratch_slice_are_still_joined() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    let (_, first) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();

    let tiny = 1 + 2 * ROW_KEY_BYTES;
    let (assigned, second) = scan(&root, &SHELF, tiny, &mut random, || {}).unwrap();
    assert_eq!(assigned.matched, 5);
    assert_eq!(assigned.minted, 0);
    assert_eq!(second, first);

    for too_small in [0, 1, ROW_KEY_BYTES] {
        assert_eq!(
            scan(&root, &SHELF, too_small, &mut random, || {}).err(),
            Some(LedgerFault::Scratch),
            "{too_small} bytes of scratch"
        );
    }
}

/// A power cut at every write from the first ledger write through the
/// catalog's own commit. Whatever the cut left is one of two legal states:
/// the generation that stood and a catalog still uncommitted, or a whole
/// new generation. A committed catalog names no id the ledger does not
/// hold, and once the power is back a scan finishes the job.
#[test]
fn a_power_cut_anywhere_through_the_catalog_commit_leaves_a_legal_state() {
    let disk = new_card();
    let mut random = entropy();
    let mut grown = SHELF.to_vec();
    grown.push((BookRoot::Library, "Poetry/Odes.epub", 77_000));
    grown.push((BookRoot::Library, "Poetry/Epodes.epub", 78_000));

    let (base, standing) = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
        (disk.image(), records(&root))
    };
    assert_eq!(standing.len(), 5);

    // How many writes the rewrite and the catalog commit take when nothing
    // cuts them.
    let uncut = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let (assigned, _) = scan(&root, &grown, ARENA, &mut random, || {
            disk.cut_writes_from(None)
        })
        .unwrap();
        assert_eq!(assigned.minted, 2);
        disk.writes_seen()
    };
    assert!(
        uncut > 4,
        "a rewrite that takes {uncut} writes proves little"
    );

    // The four states a cut can leave the journal and the target in, each
    // of which must be seen at least once for the sweep to have covered the
    // protocol: the rewrite not begun, begun and not committed, committed
    // and not yet journalled, and journalled.
    let mut seen = [false; 4];

    for cut in 1..=uncut {
        disk.restore(&base);
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let outcome = scan(&root, &grown, ARENA, &mut random, || {
            disk.cut_writes_from(Some(cut))
        });
        disk.cut_writes_from(None);
        drop(root);
        drop(mgr);

        // Power is back.
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let live = records(&root);
        match outcome {
            Ok((assigned, _)) => {
                assert_eq!(assigned.minted, 2, "cut at write {cut}");
                assert_eq!(live.len(), 7, "cut at write {cut}");
            }
            Err(fault) => {
                // The catalog may still have landed: its header is one block
                // and the report of failure can come from the flush after
                // it. What must hold either way is checked below.
                assert_eq!(fault, LedgerFault::Device, "cut at write {cut}");
                assert!(
                    live == standing || live.len() == 7,
                    "cut at write {cut}: the ledger is one of the two legal generations"
                );
            }
        }
        match journal(&root) {
            Some(LedgerJournal::Committed { side: 0, .. }) => {
                assert_eq!(live, standing, "cut at write {cut}: not begun");
                seen[0] = true;
            }
            Some(LedgerJournal::Rewriting {
                target: 1,
                standing: Some(_),
            }) => {
                if live == standing {
                    seen[1] = true;
                } else {
                    assert_eq!(
                        live.len(),
                        7,
                        "cut at write {cut}: committed, not journalled"
                    );
                    seen[2] = true;
                }
            }
            Some(LedgerJournal::Committed { side: 1, .. }) => {
                assert_eq!(live.len(), 7, "cut at write {cut}: journalled");
                seen[3] = true;
            }
            other => panic!("cut at write {cut}: the journal reads {other:?}"),
        }
        let ledger_ids = ids_of(&live);
        if let Some(committed) = committed_catalog_ids(&root) {
            assert_eq!(committed.len(), 7, "cut at write {cut}");
            for (row, id) in committed.iter().enumerate() {
                let id = id.unwrap_or_else(|| panic!("cut at write {cut}: row {row} has an id"));
                assert!(
                    ledger_ids.contains(&id),
                    "cut at write {cut}: row {row} names an id the ledger holds"
                );
            }
        }
        // The next scan finishes what the cut interrupted, over whichever
        // side the cut left uncommitted.
        let (assigned, ids) = scan(&root, &grown, ARENA, &mut random, || {}).unwrap();
        assert_eq!(assigned.matched + assigned.minted, 7, "cut at write {cut}");
        assert!(ids.iter().all(Option::is_some), "cut at write {cut}");
        for (index, (_, _, _, id, _)) in standing.iter().enumerate() {
            assert_eq!(
                ids[index],
                Some(*id),
                "cut at write {cut}: row {index} kept its id"
            );
        }
        assert_eq!(records(&root).len(), 7, "cut at write {cut}");
    }
    assert_eq!(
        seen, [true; 4],
        "not begun, begun, committed, journalled: each must have been cut into"
    );
}

/// Damage one byte inside a record of the given side, as bit rot would.
fn damage_record(root: &Dir<'_>, side: usize, index: usize) {
    let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[side], Mode::ReadWriteAppend)
        .unwrap();
    // Inside the record's locator.
    file.seek_from_start((LEDGER_HEADER_BYTES + index * LEDGER_RECORD_BYTES + 25) as u32)
        .unwrap();
    file.write(&[0xFF]).unwrap();
    file.close().unwrap();
}

/// A committed generation with one bad record is damaged durable state, not
/// an interrupted write. It is refused, the older generation is not taken
/// in its place, and nothing on either side is touched: the ids the damaged
/// generation added would otherwise be re-minted, and every intact record
/// beside the bad one is still there for an explicit salvage.
#[test]
fn a_damaged_committed_generation_is_refused_rather_than_fallen_back_from() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF[..3], ARENA, &mut random, || {}).unwrap();
    scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));

    // The fourth record, one of the two the newer generation added. The
    // fifth is intact, and would have been re-minted by a fallback.
    damage_record(&root, 1, 3);
    let older = ledger_file_bytes(&root, 0);
    let damaged = ledger_file_bytes(&root, 1);

    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    assert_eq!(
        scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged)
    );
    assert_eq!(
        scan(&root, &SHELF[..3], ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged),
        "not even the rows the older generation could answer for"
    );
    assert_eq!(ledger_file_bytes(&root, 0), older, "the older side stands");
    assert_eq!(
        ledger_file_bytes(&root, 1),
        damaged,
        "and so does the damaged one"
    );
    assert_eq!(
        committed_catalog_ids(&root),
        None,
        "a refused scan commits no catalog"
    );
}

/// A committed header over a file that is not as long as it says is the
/// same damage: the header lands only after the length is final, so no
/// power cut produces this, and it is refused the same way.
#[test]
fn a_committed_generation_of_the_wrong_length_is_refused() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    let whole = ledger_file_bytes(&root, 0);
    {
        let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[0], Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        file.write(&whole[..whole.len() - LEDGER_RECORD_BYTES])
            .unwrap();
        file.close().unwrap();
    }
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    assert_eq!(
        scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged)
    );
}

/// Overwrite bytes of one side's header, as bit rot or another build would.
fn overwrite_header(root: &Dir<'_>, side: usize, at: usize, bytes: &[u8]) {
    let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[side], Mode::ReadWriteAppend)
        .unwrap();
    file.seek_from_start(at as u32).unwrap();
    file.write(bytes).unwrap();
    file.close().unwrap();
}

/// The header is one block earlier than the records, and the same rule
/// holds there: the journal names the live side and the header it carries,
/// and a live side whose header reads as anything else is refused, with
/// nothing written over either file. The side the journal does not name is
/// not consulted, so damage there costs nothing until the next rewrite goes
/// over it.
#[test]
fn a_damaged_header_on_the_live_side_is_refused_and_on_the_other_side_ignored() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF[..3], ARENA, &mut random, || {}).unwrap();
    let (_, ids) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));
    let older = ledger_file_bytes(&root, 0);
    let newer = ledger_file_bytes(&root, 1);

    // The live side, which is the one whose ids a fallback would re-mint.
    for (at, byte) in [(0usize, b'Y'), (5, 1u8), (9, 0xFF), (15, 0x00)] {
        let mut damaged = newer.clone();
        damaged[at] = byte;
        if damaged[at] == newer[at] {
            damaged[at] ^= 0x01;
        }
        overwrite_header(&root, 1, 0, &damaged[..LEDGER_HEADER_BYTES]);
        assert_eq!(
            ledger::open(&root).err(),
            Some(LedgerFault::Damaged),
            "header byte {at}"
        );
        assert_eq!(
            scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
            Some(LedgerFault::Damaged),
            "header byte {at}"
        );
        assert_eq!(ledger_file_bytes(&root, 0), older, "header byte {at}");
        assert_eq!(
            ledger_file_bytes(&root, 1),
            damaged,
            "header byte {at}: not written over either"
        );
        overwrite_header(&root, 1, 0, &newer[..LEDGER_HEADER_BYTES]);
    }
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));

    // The other side: not the ledger's problem.
    overwrite_header(&root, 0, 2, b"?");
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));
    let mut grown = SHELF.to_vec();
    grown.push((BookRoot::Library, "Poetry/Odes.epub", 77_000));
    let (assigned, after) = scan(&root, &grown, ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.matched, 5);
    assert_eq!(assigned.minted, 1);
    assert_eq!(&after[..5], &ids[..]);
    assert_eq!(generation(&root), Some((3, 6, LEDGER_FILES[0])));
}

/// The live generation losing its header, its length, or its file is the
/// loss this ledger exists to refuse. Each of those looks exactly like the
/// side a rewrite was interrupted on, and the journal is what says it was
/// not one: it names the side as live, so the side had better be.
#[test]
fn a_live_generation_that_loses_its_header_or_file_is_refused() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF[..3], ARENA, &mut random, || {}).unwrap();
    scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));
    let older = ledger_file_bytes(&root, 0);
    let newer = ledger_file_bytes(&root, 1);
    let recorded = journal_bytes(&root);

    // Its header becomes the placeholder.
    overwrite_header(&root, 1, 0, &[0u8; LEDGER_HEADER_BYTES]);
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    assert_eq!(
        scan(&root, &SHELF[..3], ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged),
        "not even for the rows the older generation could answer for"
    );
    assert_eq!(ledger_file_bytes(&root, 0), older);
    assert_eq!(journal_bytes(&root), recorded);
    overwrite_header(&root, 1, 0, &newer[..LEDGER_HEADER_BYTES]);
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));

    // Its file becomes empty.
    {
        let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[1], Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        file.close().unwrap();
    }
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    assert_eq!(
        scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged)
    );
    assert_eq!(ledger_file_bytes(&root, 0), older);
    assert_eq!(journal_bytes(&root), recorded);

    // Its file is gone.
    {
        let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
        cache_root.delete_entry_in_dir(LEDGER_FILES[1]).unwrap();
    }
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    assert_eq!(
        scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged)
    );
    assert_eq!(ledger_file_bytes(&root, 0), older);
    assert_eq!(journal_bytes(&root), recorded);
}

/// The same placeholder, left by a rewrite the power actually cut, is
/// explained by the journal, and the generation that stood answers. The next
/// scan finishes the rewrite over the side the cut left.
#[test]
fn an_interrupted_rewrite_resumes_from_the_generation_that_stood() {
    let disk = new_card();
    let mut random = entropy();
    let (base, three) = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let (_, three) = scan(&root, &SHELF[..3], ARENA, &mut random, || {}).unwrap();
        (disk.image(), three)
    };

    // The first cut that lands after the journal has said a rewrite began
    // and before the new header has: the placeholder on the target side.
    let mut interrupted = None;
    for cut in 1..64 {
        disk.restore(&base);
        {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            let _ = scan(&root, &SHELF, ARENA, &mut random, || {
                disk.cut_writes_from(Some(cut))
            });
            disk.cut_writes_from(None);
        }
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        if let Some(LedgerJournal::Rewriting {
            target: 1,
            standing: Some(_),
        }) = journal(&root)
        {
            let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
            let target = cache_root.open_file_in_dir(LEDGER_FILES[1], Mode::ReadOnly);
            let uncommitted = match target {
                Ok(file) => {
                    let mut header = [0u8; LEDGER_HEADER_BYTES];
                    file.length() == 0
                        || (read_exact(&file, &mut header) && header == [0u8; LEDGER_HEADER_BYTES])
                }
                Err(_) => true,
            };
            if uncommitted {
                interrupted = Some(cut);
                break;
            }
        }
    }
    let cut =
        interrupted.expect("some cut leaves the target uncommitted under a journal that says why");

    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    assert_eq!(
        generation(&root),
        Some((1, 3, LEDGER_FILES[0])),
        "cut at write {cut}"
    );
    assert_eq!(
        ids_of(&records(&root)),
        three.iter().map(|id| id.unwrap()).collect::<Vec<_>>()
    );

    let (assigned, ids) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.matched, 3, "cut at write {cut}");
    assert_eq!(assigned.minted, 2, "cut at write {cut}");
    assert_eq!(&ids[..3], &three[..]);
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));
    assert!(matches!(
        journal(&root),
        Some(LedgerJournal::Committed { side: 1, .. })
    ));
}

/// Damage on the side that is not live is harmless on its own, and a cut
/// rewrite is recoverable on its own. Composed, they used to wedge: the
/// journal announced the rewrite before the damaged side was touched, so a
/// cut in between left a journal pointing at a target whose header was
/// garbage, and every mount after refused. The journal now names the
/// standing side until the target holds the placeholder, so at every cut of
/// a rewrite over a damaged side the ledger opens, and the next scan
/// finishes the job.
#[test]
fn a_damaged_non_live_side_stays_harmless_through_a_cut_rewrite() {
    let disk = new_card();
    let mut random = entropy();
    let mut grown = SHELF.to_vec();
    grown.push((BookRoot::Library, "Poetry/Odes.epub", 77_000));

    let (base, standing) = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        scan(&root, &SHELF[..3], ARENA, &mut random, || {}).unwrap();
        scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
        assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));
        // The obsolete side, the next rewrite's target, rots.
        overwrite_header(&root, 0, 2, b"?");
        assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));
        (disk.image(), records(&root))
    };

    let uncut = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let (assigned, _) = scan(&root, &grown, ARENA, &mut random, || {
            disk.cut_writes_from(None)
        })
        .unwrap();
        assert_eq!(assigned.minted, 1);
        disk.writes_seen()
    };

    for cut in 1..=uncut {
        disk.restore(&base);
        {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            let _ = scan(&root, &grown, ARENA, &mut random, || {
                disk.cut_writes_from(Some(cut))
            });
            disk.cut_writes_from(None);
        }
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        // `records` opens the ledger and panics on a refusal, which is the
        // wedge this test exists to rule out.
        let live = records(&root);
        assert!(
            live == standing || live.len() == 6,
            "cut at write {cut}: one of the two legal generations"
        );
        let (assigned, ids) = scan(&root, &grown, ARENA, &mut random, || {}).unwrap();
        assert_eq!(assigned.matched + assigned.minted, 6, "cut at write {cut}");
        for (index, (_, _, _, id, _)) in standing.iter().enumerate() {
            assert_eq!(ids[index], Some(*id), "cut at write {cut}: row {index}");
        }
        assert_eq!(generation(&root).map(|(_, count, _)| count), Some(6));
    }
}

/// The journal's own write is the one durable write the protocol has left,
/// and a sector can tear inside it. Two slots written alternately keep the
/// entry before, and falling back one entry is safe by construction. Tear
/// the write of each transition at every length that leaves a partial
/// entry, and at lengths past it, and check that the ledger opens, names a
/// legal generation, and that the next scan finishes with the standing ids.
#[test]
fn a_torn_journal_write_leaves_an_entry_that_still_reads() {
    let disk = new_card();
    let mut random = entropy();
    let mut grown = SHELF.to_vec();
    grown.push((BookRoot::Library, "Poetry/Odes.epub", 77_000));

    let (base, standing) = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
        (disk.image(), records(&root))
    };

    // Which writes of an uncut rewrite land on the journal's blocks: the
    // first publishes Rewriting into the second slot, the second publishes
    // Committed into the first.
    let journal_writes: Vec<u32> = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        scan(&root, &grown, ARENA, &mut random, || {
            disk.cut_writes_from(None)
        })
        .unwrap();
        let image = disk.image();
        let journal_blocks: Vec<u32> = (0..DISK_BLOCKS)
            .filter(|block| {
                let at = *block as usize * BLOCK_BYTES;
                image[at..at + 4] == proto::identity::LEDGER_JOURNAL_MAGIC
            })
            .collect();
        assert_eq!(journal_blocks.len(), 2, "both slots hold an entry by now");
        disk.written_blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| journal_blocks.contains(block))
            .map(|(index, _)| index as u32 + 1)
            .collect()
    };
    assert_eq!(journal_writes.len(), 2, "one write per transition");

    let mut saw_torn = [false; 2];
    for (transition, write) in journal_writes.iter().enumerate() {
        for landed in (0..=LEDGER_JOURNAL_BYTES).chain([100, LEDGER_JOURNAL_SLOT_BYTES - 1]) {
            disk.restore(&base);
            {
                let mgr = open_mgr(&disk);
                let root = open_root(&mgr);
                let _ = scan(&root, &grown, ARENA, &mut random, || {
                    disk.tear_write_at(*write, landed)
                });
                disk.cut_writes_from(None);
            }
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            // At most the slot being written tore, and the other still
            // reads. A tear that landed only bytes the old entry already
            // had leaves that entry intact, which is the write not having
            // happened.
            let slots = journal_slots(&root);
            let torn = slots
                .iter()
                .filter(|slot| **slot == LedgerJournalReading::Damaged)
                .count();
            let intact = slots
                .iter()
                .filter(|slot| matches!(slot, LedgerJournalReading::Entry { .. }))
                .count();
            assert!(
                torn <= 1 && intact >= 1 && torn + intact == 2,
                "transition {transition}, {landed} bytes landed: {slots:?}"
            );
            if torn == 1 {
                saw_torn[transition] = true;
            }
            // `records` opens the ledger and panics on a refusal.
            let live = records(&root);
            assert!(
                live == standing || live.len() == 6,
                "transition {transition}, {landed} bytes landed: a legal generation"
            );
            let (assigned, ids) = scan(&root, &grown, ARENA, &mut random, || {}).unwrap();
            assert_eq!(
                assigned.matched + assigned.minted,
                6,
                "transition {transition}, {landed} bytes landed"
            );
            for (index, (_, _, _, id, _)) in standing.iter().enumerate() {
                assert_eq!(
                    ids[index],
                    Some(*id),
                    "transition {transition}, {landed} bytes landed: row {index}"
                );
            }
        }
    }
    assert_eq!(
        saw_torn, [true; 2],
        "both transitions were torn in earnest at least once"
    );
}

/// The first generation is the one path that announces itself before its
/// file exists, so it gets a sweep of its own: a cut at every write of a
/// first scan leaves a card that opens, and the next scan adopts every book.
#[test]
fn a_fresh_card_survives_a_cut_anywhere_in_its_first_scan() {
    let disk = new_card();
    let mut random = entropy();
    let base = disk.image();
    let uncut = {
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        scan(&root, &SHELF, ARENA, &mut random, || {
            disk.cut_writes_from(None)
        })
        .unwrap();
        disk.writes_seen()
    };
    for cut in 1..=uncut {
        disk.restore(&base);
        {
            let mgr = open_mgr(&disk);
            let root = open_root(&mgr);
            let _ = scan(&root, &SHELF, ARENA, &mut random, || {
                disk.cut_writes_from(Some(cut))
            });
            disk.cut_writes_from(None);
        }
        let mgr = open_mgr(&disk);
        let root = open_root(&mgr);
        let live = records(&root);
        assert!(
            live.is_empty() || live.len() == 5,
            "cut at write {cut}: nothing yet, or everything"
        );
        let (assigned, ids) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
        assert_eq!(assigned.matched + assigned.minted, 5, "cut at write {cut}");
        assert!(ids.iter().all(Option::is_some), "cut at write {cut}");
        assert_eq!(records(&root).len(), 5, "cut at write {cut}");
    }
}

/// Ledger files with no journal to account for them, or a journal that does
/// not read, are refused: the journal is the only thing that says which side
/// is live, and guessing is what this file exists to avoid.
#[test]
fn a_ledger_without_a_readable_journal_is_refused() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    let written = ledger_file_bytes(&root, 0);
    let recorded = journal_bytes(&root);
    assert_eq!(recorded.len(), LEDGER_JOURNAL_FILE_BYTES);
    // A first scan announces the generation and then names it live: two
    // entries, one per slot, the second the newer.
    assert!(
        matches!(
            journal_slots(&root),
            [
                LedgerJournalReading::Entry {
                    entry: LedgerJournal::Rewriting { .. },
                    sequence: 1
                },
                LedgerJournalReading::Entry {
                    entry: LedgerJournal::Committed { .. },
                    sequence: 2
                }
            ]
        ),
        "{:?}",
        journal_slots(&root)
    );

    // One damaged slot is a torn write, and the other entry answers: here
    // the announcement, whose target committed whole.
    let newer = LEDGER_JOURNAL_SLOT_BYTES;
    overwrite_journal(&root, newer + 7, &[recorded[newer + 7] ^ 0x01]);
    assert_eq!(generation(&root), Some((1, 5, LEDGER_FILES[0])));

    // Both damaged, then both blank, then a file of the wrong length: no
    // entry reads, and the ledger files beside it are unaccounted for.
    overwrite_journal(&root, 7, &[recorded[7] ^ 0x01]);
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    overwrite_journal(&root, 0, &[0u8; LEDGER_JOURNAL_BYTES]);
    overwrite_journal(&root, newer, &[0u8; LEDGER_JOURNAL_BYTES]);
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    assert_eq!(
        scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged)
    );
    assert_eq!(ledger_file_bytes(&root, 0), written, "nothing written over");
    {
        let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
        let file = cache_root
            .open_file_in_dir(LEDGER_JOURNAL, Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        file.write(&recorded[..LEDGER_JOURNAL_FILE_BYTES - 1])
            .unwrap();
        file.close().unwrap();
    }
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));

    // Another build's journal.
    {
        let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
        let file = cache_root
            .open_file_in_dir(LEDGER_JOURNAL, Mode::ReadWriteCreateOrTruncate)
            .unwrap();
        let mut other = recorded.clone();
        other[4] = proto::identity::LEDGER_JOURNAL_VERSION + 1;
        file.write(&other).unwrap();
        file.close().unwrap();
    }
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Unreadable));

    // Gone.
    {
        let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
        cache_root.delete_entry_in_dir(LEDGER_JOURNAL).unwrap();
    }
    assert_eq!(ledger::open(&root).err(), Some(LedgerFault::Damaged));
    assert_eq!(
        scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
        Some(LedgerFault::Damaged)
    );
    assert_eq!(ledger_file_bytes(&root, 0), written);

    // Put back, all is as it was.
    {
        let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap();
        let file = cache_root
            .open_file_in_dir(LEDGER_JOURNAL, Mode::ReadWriteCreate)
            .unwrap();
        file.write(&recorded).unwrap();
        file.close().unwrap();
    }
    assert_eq!(generation(&root), Some((1, 5, LEDGER_FILES[0])));
}

/// A committed header of a version this build does not read is another
/// build's ledger, and it is refused the way damage is: the ids in it
/// cannot come back from the card.
#[test]
fn a_ledger_of_a_version_this_build_does_not_read_is_refused() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    let written = ledger_file_bytes(&root, 0);

    for version in [proto::identity::LEDGER_VERSION + 1, u8::MAX] {
        overwrite_header(&root, 0, 4, &[version]);
        assert_eq!(
            ledger::open(&root).err(),
            Some(LedgerFault::Unreadable),
            "version {version}"
        );
        assert_eq!(
            scan(&root, &SHELF, ARENA, &mut random, || {}).err(),
            Some(LedgerFault::Unreadable),
            "version {version}"
        );
        assert_eq!(
            ledger_file_bytes(&root, 0)[LEDGER_HEADER_BYTES..],
            written[LEDGER_HEADER_BYTES..],
            "version {version}: the records are untouched"
        );
    }
    overwrite_header(&root, 0, 0, &written[..LEDGER_HEADER_BYTES]);
    assert_eq!(generation(&root), Some((1, 5, LEDGER_FILES[0])));
}

/// A card with every book taken off it is a scan with no rows, and that
/// scan still counts against every record, so the ledger does not keep a
/// removed library forever.
#[test]
fn an_empty_scan_still_ages_and_retires_missing_copies() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();

    for scans_missing in 1..=MISSING_SCANS_RETAINED {
        let (assigned, ids) = scan(&root, &[], ARENA, &mut random, || {}).unwrap();
        assert!(ids.is_empty());
        assert_eq!(
            assigned,
            Assignment {
                missing: 5,
                ..Assignment::default()
            },
            "empty scan {scans_missing}"
        );
        let live = records(&root);
        assert_eq!(live.len(), 5, "empty scan {scans_missing}");
        assert!(
            live.iter().all(|record| record.4 == scans_missing),
            "empty scan {scans_missing}: every record one scan older"
        );
    }
    let (assigned, _) = scan(&root, &[], ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        assigned,
        Assignment {
            retired: 5,
            ..Assignment::default()
        }
    );
    assert_eq!(records(&root), vec![], "past the bound, all let go");
    let settled = generation(&root);
    assert_eq!(settled.map(|(_, count, _)| count), Some(0));
    scan(&root, &[], ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        generation(&root),
        settled,
        "an empty ledger has nothing to age"
    );

    let (assigned, _) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.minted, 5, "the books come back as new copies");
}

/// Damage on the side that is not live is not the ledger's problem: the live
/// generation answers, and the next rewrite goes over the damaged side.
#[test]
fn a_damaged_older_side_is_ignored_and_written_over() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    scan(&root, &SHELF[..3], ARENA, &mut random, || {}).unwrap();
    let (_, ids) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));

    damage_record(&root, 0, 1);
    assert_eq!(generation(&root), Some((2, 5, LEDGER_FILES[1])));
    assert_eq!(
        ids_of(&records(&root)),
        ids.iter().map(|id| id.unwrap()).collect::<Vec<_>>()
    );

    let mut grown = SHELF.to_vec();
    grown.push((BookRoot::Library, "Poetry/Odes.epub", 77_000));
    let (assigned, after) = scan(&root, &grown, ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.matched, 5);
    assert_eq!(assigned.minted, 1);
    assert_eq!(generation(&root), Some((3, 6, LEDGER_FILES[0])));
    assert_eq!(&after[..5], &ids[..]);
}

/// A copy that leaves the card is remembered for a bounded number of scans
/// and then let go. One that comes back within the bound is the copy it
/// was, and stops ageing.
#[test]
fn a_missing_copy_is_retained_for_a_bounded_number_of_scans() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    let (_, ids) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    let atlas = ids[4].unwrap();
    let without_atlas = &SHELF[..4];

    for scans_missing in 1..=MISSING_SCANS_RETAINED {
        let (assigned, _) = scan(&root, without_atlas, ARENA, &mut random, || {}).unwrap();
        assert_eq!(
            assigned,
            Assignment {
                matched: 4,
                missing: 1,
                ..Assignment::default()
            },
            "scan {scans_missing} without the atlas"
        );
        let live = records(&root);
        assert_eq!(
            live.len(),
            5,
            "scan {scans_missing}: the record is retained"
        );
        let record = live.iter().find(|record| record.3 == atlas).unwrap();
        assert_eq!(
            record.4, scans_missing,
            "scan {scans_missing}: one scan older"
        );
    }
    let (assigned, _) = scan(&root, without_atlas, ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        assigned,
        Assignment {
            matched: 4,
            retired: 1,
            ..Assignment::default()
        }
    );
    assert_eq!(
        records(&root).len(),
        4,
        "past the bound, the record is gone"
    );
    let before = generation(&root);
    let (assigned, _) = scan(&root, without_atlas, ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.matched, 4);
    assert_eq!(
        generation(&root),
        before,
        "nothing left to change writes nothing"
    );

    // The atlas comes back: new to the ledger now, so a new copy.
    let (assigned, after) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.minted, 1);
    assert_ne!(after[4], Some(atlas), "let go means let go");
    let atlas = after[4].unwrap();

    // Gone for three scans and back: the same copy, and no longer missing.
    for _ in 0..3 {
        scan(&root, without_atlas, ARENA, &mut random, || {}).unwrap();
    }
    assert_eq!(
        records(&root)
            .iter()
            .find(|record| record.3 == atlas)
            .unwrap()
            .4,
        3
    );
    let (assigned, back) = scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        assigned,
        Assignment {
            matched: 5,
            ..Assignment::default()
        }
    );
    assert_eq!(back[4], Some(atlas), "the copy it was");
    let live = records(&root);
    assert!(
        live.iter().all(|record| record.4 == 0),
        "coming back resets the count, which is a change the ledger records"
    );
    let settled = generation(&root);
    scan(&root, &SHELF, ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        generation(&root),
        settled,
        "and then there is nothing to write"
    );
}

/// Two live records naming one place is a ledger this writer does not
/// produce: one file sits at a place, and the copy there has one id. The
/// first in ledger order takes the row, every time, and the caller is told;
/// the record that lost is not treated as naming anything, so it ages out
/// like any other record whose place is not there and the ledger comes back
/// to one id per copy on its own. Carrying both as live was the earlier
/// behaviour, and it left two ids answering for one file for ever, which is
/// two owners for whatever state hangs from an id.
#[test]
fn a_place_named_twice_takes_the_first_record_and_ages_out_the_other() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let first = BookId::from_bytes([1; 16]).unwrap();
    let second = BookId::from_bytes([2; 16]).unwrap();
    let row = SHELF[3];
    ledger::write_generation(
        &root,
        None,
        &mut |_, record| Carry::Keep(Kept::of(record)),
        |writer| {
            for id in [first, second] {
                writer.append(&LedgerRecord {
                    id,
                    root: row.0,
                    locator: row.1,
                    byte_size: row.2,
                    misses: 0,
                    source: None,
                })?;
            }
            Ok(())
        },
    )
    .unwrap();

    let mut random = entropy();
    let (assigned, ids) = scan(&root, &[row], ARENA, &mut random, || {}).unwrap();
    assert_eq!(
        assigned,
        Assignment {
            matched: 1,
            duplicates: 1,
            missing: 1,
            ..Assignment::default()
        }
    );
    assert_eq!(ids, vec![Some(first)]);
    assert_eq!(records(&root).len(), 2, "the other is a scan older");

    // And the shadowed record has no place to give while it lasts. Its
    // locator is the winner's file, so answering with it would resolve one
    // copy's state against another's book, which is the merge that costs
    // more than the copy.
    let live = ledger::open(&root).unwrap().unwrap();
    let winner = ledger::find_by_id(&root, &live, first).unwrap().unwrap();
    assert_eq!(winner.locator(), Some(row.1), "the row's id names the file");
    assert_eq!(winner.misses, 0);
    let loser = ledger::find_by_id(&root, &live, second).unwrap().unwrap();
    assert_eq!(loser.place, None, "the place it names is not its own");
    assert_eq!(
        loser.byte_size, row.2,
        "the record is still there to salvage"
    );
    assert_eq!(loser.misses, 1);

    // Every scan reads it the same way and ages the shadowed record once
    // more, until the retention window runs out.
    for _ in 1..MISSING_SCANS_RETAINED {
        let (assigned, ids) = scan(&root, &[row], ARENA, &mut random, || {}).unwrap();
        assert_eq!(assigned.duplicates, 1);
        assert_eq!(ids, vec![Some(first)], "the row's id stays where it was");
        let live = ledger::open(&root).unwrap().unwrap();
        assert_eq!(
            ledger::find_by_id(&root, &live, second)
                .unwrap()
                .unwrap()
                .place,
            None,
            "and still has no place to give"
        );
    }
    assert_eq!(records(&root).len(), 2);

    let (assigned, ids) = scan(&root, &[row], ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.retired, 1, "the last scan it is carried for");
    assert_eq!(assigned.duplicates, 1, "read once more on its way out");
    assert_eq!(ids, vec![Some(first)]);
    assert_eq!(ids_of(&records(&root)), vec![first], "one copy, one id");
    let live = ledger::open(&root).unwrap().unwrap();
    assert_eq!(
        ledger::find_by_id(&root, &live, second).unwrap(),
        None,
        "the id it carried names nothing at all now"
    );
    assert_eq!(
        ledger::find_by_id(&root, &live, first)
            .unwrap()
            .and_then(|copy| copy.locator().map(str::to_owned)),
        Some(row.1.to_owned()),
        "and the copy that kept the place still answers with it"
    );

    // And the card settles: nothing shadows the row, and nothing is written
    // for a card that has not changed.
    let settled = generation(&root);
    let (assigned, ids) = scan(&root, &[row], ARENA, &mut random, || {}).unwrap();
    assert_eq!(assigned.duplicates, 0);
    assert_eq!(ids, vec![Some(first)]);
    assert_eq!(generation(&root), settled);
}

/// An id is what per-copy state hangs from, so the ledger has to answer the
/// question that state asks: which file is this id, now. A rename moves the
/// answer without changing the question, which is the whole point of
/// keeping ids off the path.
#[test]
fn an_id_answers_with_the_copy_it_names_wherever_that_copy_has_moved() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let rows = [
        (BookRoot::Library, "Dune.epub", 3_000),
        (BookRoot::Library, "Emma.epub", 2_000),
    ];
    let (_, ids) = scan(&root, &rows, ARENA, &mut entropy(), || {}).expect("scan");
    let dune = ids[0].expect("adopted");
    let emma = ids[1].expect("adopted");

    let live = ledger::open(&root).expect("open").expect("a ledger");
    let copy = ledger::find_by_id(&root, &live, dune)
        .expect("read")
        .expect("the id names a copy");
    assert_eq!(
        copy.place.as_ref().map(|(at, _)| *at),
        Some(BookRoot::Library)
    );
    assert_eq!(copy.locator(), Some("Dune.epub"));
    assert_eq!(copy.byte_size, 3_000);
    assert_eq!(copy.misses, 0, "the scan just saw it");
    assert_eq!(copy.source, None, "a sideloaded copy's bytes were not read");
    assert_eq!(
        ledger::find_by_id(&root, &live, BookId::from_bytes([7u8; 16]).unwrap()).expect("read"),
        None,
        "an id nothing carries names nothing"
    );

    // The copy moves into a folder. Its record follows the file; the id it
    // is asked about does not change.
    let live = ledger::relocate_record(&root, live, dune, BookRoot::Library, "Fiction/Dune.epub")
        .expect("relocate");
    let copy = ledger::find_by_id(&root, &live, dune)
        .expect("read")
        .expect("the same id, the new place");
    assert_eq!(copy.locator(), Some("Fiction/Dune.epub"));
    assert_eq!(copy.byte_size, 3_000);
    let other = ledger::find_by_id(&root, &live, emma)
        .expect("read")
        .expect("the other copy is where it was");
    assert_eq!(other.locator(), Some("Emma.epub"));
}

/// A card can be taken out, edited, and put back, so a copy the last scan
/// did not find is not a copy whose state is gone. Its id still answers,
/// and says how many scans have missed it.
#[test]
fn a_copy_the_scan_missed_still_answers_to_its_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let root = open_root(&mgr);
    let mut random = entropy();
    let rows = [
        (BookRoot::Library, "Dune.epub", 3_000),
        (BookRoot::Library, "Emma.epub", 2_000),
    ];
    let (_, ids) = scan(&root, &rows, ARENA, &mut random, || {}).expect("scan");
    let dune = ids[0].expect("adopted");

    // The next scan runs with the book off the card.
    scan(
        &root,
        &[(BookRoot::Library, "Emma.epub", 2_000)],
        ARENA,
        &mut random,
        || {},
    )
    .expect("scan");
    let live = ledger::open(&root).expect("open").expect("a ledger");
    let copy = ledger::find_by_id(&root, &live, dune)
        .expect("read")
        .expect("a missing copy still answers");
    assert_eq!(copy.locator(), Some("Dune.epub"), "where it last was");
    assert_eq!(copy.misses, 1);
}
