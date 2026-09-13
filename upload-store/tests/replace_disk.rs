//! Managed replacement against a real FAT filesystem.
//!
//! The rules are unit-tested in `proto::identity`. What is checked here is
//! the protocol: that an upload landing over an adopted copy leaves the
//! copy's id in place with the new size and digest, at whatever spelling the
//! install used; that a power cut at every write from the intent's
//! publication through the ledger's rewrite and the intent's clear leaves a
//! card whose recovery, in the order the identity design fixes, ends with
//! the same id naming whichever bytes the destination holds, under whichever
//! spelling; that a predecessor replaced on a computer between transactions
//! is still recognised; and that a destination holding neither legal
//! landing is refused rather than guessed at.
//!
//! The card model is the one `ledger_disk.rs` uses: a RAM image whose writes
//! can be cut off from a chosen point onward, or torn inside one sector.

use std::cell::RefCell;
use std::rc::Rc;

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, Mode, TimeSource, Timestamp, VolumeIdx,
    VolumeManager,
};
use proto::cache::{cache_key_from, source_hash_at, CACHE_ROOT_DIR, CATALOG_FILE};
use proto::catalog::{
    catalog_record_book_id, encode_catalog_header, encode_catalog_placeholder_header,
    encode_catalog_record, CATALOG_HEADER_BYTES, CATALOG_RECORD_BYTES,
};
use proto::identity::{BookId, Landing, LedgerRecord, Predecessor, REPLACE_JOURNAL_MAGIC};
use proto::library_path::{BookRoot, LibraryPath};
use proto::source::{digest_of, CachedSourceDigest, SourceDigest};
use upload_store::install::{self, InstallError, StagedUpload};
use upload_store::ledger::{self, Assignment, Carry, Kept, LedgerFault, LEDGER_MAX_RECORDS};
use upload_store::replace::{self, PredecessorSeen, Recovery};
use upload_store::{library, reclaim};

const BLOCK_BYTES: usize = 512;
/// 16 MiB, the size every other disk test uses.
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

struct RamDisk {
    data: RefCell<Vec<u8>>,
    blocks: u32,
    fail_writes_from: RefCell<Option<u32>>,
    writes_seen: RefCell<u32>,
    tear_write_at: RefCell<Option<(u32, usize)>>,
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
        Ok(BlockCount(self.0.blocks))
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

fn format_disk(blocks: u32) -> Vec<u8> {
    let mut disk = vec![0u8; blocks as usize * BLOCK_BYTES];
    let part_blocks = blocks - PART_START_BLOCK;
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
    new_card_of(DISK_BLOCKS)
}

fn new_card_of(blocks: u32) -> SharedDisk {
    SharedDisk(Rc::new(RamDisk {
        data: RefCell::new(format_disk(blocks)),
        blocks,
        fail_writes_from: RefCell::new(None),
        writes_seen: RefCell::new(0),
        tear_write_at: RefCell::new(None),
        written_blocks: RefCell::new(Vec::new()),
    }))
}

fn open_mgr(disk: &SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk.clone(), StaticTime, 5000)
}

/// The card root and the shelf, made if it is not there yet.
fn open_dirs(mgr: &Mgr) -> (Dir<'_>, Dir<'_>) {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("open volume");
    let raw_root = mgr
        .open_root_dir(volume.to_raw_volume())
        .expect("open root");
    let root = Directory::new(raw_root, mgr);
    if root.open_dir("BOOKS").is_err() {
        root.make_dir_in_dir("BOOKS").expect("make BOOKS");
    }
    let books = root.open_dir("BOOKS").expect("open BOOKS");
    (root, books)
}

/// A deterministic word source for minting. The tests assert on which ids
/// survive, not on entropy.
fn words() -> impl FnMut() -> u32 {
    let mut state = 0x2545_F491u32;
    move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state
    }
}

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

const BOOK: &str = "Dune.epub";
/// A book at the card root, where nothing nests and no shelf is needed.
const LOOSE: &str = "Loose.epub";
/// The same name as the card matches it, spelled another way.
const BOOK_RESPELLED: &str = "dune.epub";

/// A body of `len` bytes, distinct per `seed`. Big enough to span clusters.
fn body(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31) ^ seed)
        .collect()
}

/// Put a book on the shelf the way a computer does: under its long name,
/// with no record anywhere.
fn sideload(books: &Dir<'_>, name: &str, bytes: &[u8]) {
    let file = books.create_file_in_dir_lfn(name).expect("create book");
    file.write(bytes).expect("write book");
    file.close().expect("close book");
}

/// What the shelf holds under `name`, spelled exactly so, or `None`.
fn shelf_bytes(root: &Dir<'_>, name: &str) -> Option<Vec<u8>> {
    let path = LibraryPath::parse(name).unwrap();
    library::with_book_at(root, BookRoot::Library, &path, |dir, alias| {
        let mut alias_text = heapless::String::<12>::new();
        use core::fmt::Write as _;
        write!(alias_text, "{}", alias).unwrap();
        let file = dir
            .open_file_in_dir(alias_text.as_str(), Mode::ReadOnly)
            .expect("open book");
        let mut out = vec![0u8; file.length() as usize];
        assert!(read_exact(&file, &mut out));
        out
    })
    .expect("shelf readable")
}

/// Overwrite what the shelf holds under `name`, as a computer would while
/// the card was in it.
fn overwrite_shelf(root: &Dir<'_>, name: &str, bytes: &[u8]) {
    overwrite_at(root, BookRoot::Library, name, bytes);
}

/// The same, at either root: a loose book answers to the card root.
fn overwrite_at(root: &Dir<'_>, at: BookRoot, name: &str, bytes: &[u8]) {
    let path = LibraryPath::parse(name).unwrap();
    library::with_book_at(root, at, &path, |dir, alias| {
        let mut alias_text = heapless::String::<12>::new();
        use core::fmt::Write as _;
        write!(alias_text, "{}", alias).unwrap();
        let file = dir
            .open_file_in_dir(alias_text.as_str(), Mode::ReadWriteCreateOrTruncate)
            .expect("open book");
        file.write(bytes).expect("write book");
        file.close().expect("close book");
    })
    .expect("shelf readable")
    .expect("the book is there to overwrite");
}

thread_local! {
    /// What the scan reported finding again, most recent scan last: the id
    /// that was kept, where it was, and where it is now.
    static FOUND_AGAIN: RefCell<Vec<(BookId, String, String)>> = const { RefCell::new(Vec::new()) };
}

/// What the scans since the last call reported, and clear it.
fn found_again() -> Vec<(BookId, String, String)> {
    FOUND_AGAIN.with(|seen| core::mem::take(&mut *seen.borrow_mut()))
}

/// One row of the catalog a scan would write.
type Row<'a> = (BookRoot, &'a str, u32);

/// The production scan, as far as the ledger is concerned: rows into an
/// uncommitted `CATALOG.BIN`, the ledger opened and joined, the header
/// committed.
fn scan(
    root: &Dir<'_>,
    rows: &[Row<'_>],
) -> Result<(Assignment, Vec<Option<BookId>>), LedgerFault> {
    scan_minting(root, rows, &mut words())
}

/// [`scan`], minting from `random` rather than the fixed word source: two
/// scans of one card that both mint must not draw the same id twice.
fn scan_minting(
    root: &Dir<'_>,
    rows: &[Row<'_>],
    random: &mut impl FnMut() -> u32,
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
    let live = ledger::open(root)?;
    let mut scratch = vec![0u8; 16 * 1024];
    let assigned = ledger::assign_book_ids(
        root,
        &file,
        rows.len() as u16,
        &mut scratch,
        random,
        live,
        &mut |found| {
            FOUND_AGAIN.with(|seen| {
                seen.borrow_mut()
                    .push((found.id, found.was.1.to_owned(), found.now.1.to_owned()))
            });
        },
    )?;
    encode_catalog_header(rows.len() as u16, &mut header);
    file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
    file.write(&header).map_err(|_| LedgerFault::Device)?;
    file.flush().map_err(|_| LedgerFault::Device)?;
    let mut ids = Vec::with_capacity(rows.len());
    for index in 0..rows.len() {
        file.seek_from_start((CATALOG_HEADER_BYTES + index * CATALOG_RECORD_BYTES) as u32)
            .expect("seek row");
        assert!(read_exact(&file, &mut record));
        ids.push(catalog_record_book_id(&record));
    }
    Ok((assigned, ids))
}

/// The ledger record with `id`: its exact locator, size, and what it says
/// about the bytes.
fn record_by_id(root: &Dir<'_>, id: BookId) -> Option<(String, u32, Option<CachedSourceDigest>)> {
    let live = ledger::open(root).expect("open ledger")?;
    let mut found = None;
    ledger::for_each_record(root, &live, &mut |_, record: &LedgerRecord<'_>| {
        if record.id == id {
            found = Some((record.locator.to_owned(), record.byte_size, record.source));
        }
        Ok(())
    })
    .expect("read ledger");
    found
}

/// The ledger record naming `name` on the shelf, spelled exactly so.
fn record_for(root: &Dir<'_>, name: &str) -> Option<(BookId, u32, Option<CachedSourceDigest>)> {
    let live = ledger::open(root).expect("open ledger")?;
    let mut found = None;
    ledger::for_each_record(root, &live, &mut |_, record: &LedgerRecord<'_>| {
        if found.is_none() && record.root == BookRoot::Library && record.locator == name {
            found = Some((record.id, record.byte_size, record.source));
        }
        Ok(())
    })
    .expect("read ledger");
    found
}

fn record_count(root: &Dir<'_>) -> usize {
    ledger::open(root)
        .expect("open ledger")
        .map_or(0, |live| live.count as usize)
}

/// Stage and install `bytes` under `name`, the way the upload session does.
/// `arm` runs between staging and the install, which is where a test cuts
/// the power.
fn upload(
    root: &Dir<'_>,
    books: &Dir<'_>,
    name: &str,
    bytes: &[u8],
    arm: impl FnOnce(),
) -> Result<Option<install::Landed>, InstallError> {
    let mut staged = StagedUpload::begin(root, books, name, None)?;
    staged.write(bytes)?;
    arm();
    staged.install(root, books, &mut words())
}

/// Recovery in the order the identity design fixes: the reclaim journal,
/// the install journal, then the library intent. Hands back what the
/// library step found.
fn recover(root: &Dir<'_>, books: &Dir<'_>) -> Recovery {
    reclaim::recover(root, Some(books)).expect("reclaim settles");
    let outcome = install::recover_installs(root, books);
    assert!(outcome.complete, "the install journal settles: {outcome:?}");
    replace::recover(root).expect("the library intent resolves or refuses")
}

fn digest_agrees(recorded: Option<CachedSourceDigest>, bytes: &[u8]) -> bool {
    recorded.is_some_and(|recorded| recorded.agrees_with(&digest_of(bytes)))
}

/// An upload over an adopted copy is the same copy: its id stays, and its
/// record takes the size and digest of what landed. A second upload over it
/// does the same again.
#[test]
fn a_replacement_keeps_the_id_and_records_what_landed() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload(&books, BOOK, &old);
    let (assigned, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
    assert_eq!(assigned.minted, 1);
    let id = ids[0].unwrap();
    assert_eq!(record_for(&root, BOOK), Some((id, old.len() as u32, None)));

    let new = body(2, 4_100);
    let landed = upload(&root, &books, BOOK, &new, || {})
        .unwrap()
        .expect("the book lands");
    assert_eq!(landed.source, digest_of(&new));
    assert_eq!(shelf_bytes(&root, BOOK).as_deref(), Some(&new[..]));
    let (found, size, source) = record_for(&root, BOOK).expect("still a record");
    assert_eq!(found, id, "the copy kept its id");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new), "and took the new digest");
    assert_eq!(record_count(&root), 1, "one copy, one record");
    assert_eq!(replace::read(&root).unwrap(), None, "the intent is cleared");
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent
    );

    // The next scan sees the new size at the old place and does not mint.
    let (assigned, ids) = scan(&root, &[(BookRoot::Library, BOOK, new.len() as u32)]).unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id));

    let newer = body(3, 2_500);
    upload(&root, &books, BOOK, &newer, || {})
        .unwrap()
        .expect("lands again");
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!(found, id);
    assert_eq!(size, newer.len() as u32);
    assert!(digest_agrees(source, &newer));
    assert_eq!(record_count(&root), 1);
}

/// The card matches names by FAT's rules and the ledger exactly. An upload
/// spelled another way replaces the copy the installer found under the
/// name, and the copy's record moves to the new spelling with its id.
#[test]
fn a_replacement_spelled_another_way_keeps_the_id_and_respells_the_record() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload(&books, BOOK, &old);
    let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
    let id = ids[0].unwrap();

    let new = body(2, 4_100);
    upload(&root, &books, BOOK_RESPELLED, &new, || {})
        .unwrap()
        .expect("lands");
    assert_eq!(shelf_bytes(&root, BOOK), None, "the old spelling is gone");
    assert_eq!(
        shelf_bytes(&root, BOOK_RESPELLED).as_deref(),
        Some(&new[..]),
        "the shelf spells it as typed"
    );
    let (locator, size, source) = record_by_id(&root, id).expect("the id survives");
    assert_eq!(locator, BOOK_RESPELLED, "and its record spells it the same");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert_eq!(
        record_count(&root),
        1,
        "one copy, one record, not a stale twin"
    );
    let (assigned, ids) = scan(
        &root,
        &[(BookRoot::Library, BOOK_RESPELLED, new.len() as u32)],
    )
    .unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id));
}

/// An upload to a name nothing holds adopts the copy with its digest, so
/// the next scan matches it and does not mint.
#[test]
fn a_fresh_upload_is_adopted_with_its_digest() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let new = body(5, 2_048);
    upload(&root, &books, BOOK, &new, || {})
        .unwrap()
        .expect("lands");
    let (id, size, source) = record_for(&root, BOOK).expect("adopted at install");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert_eq!(replace::read(&root).unwrap(), None);

    let (assigned, ids) = scan(&root, &[(BookRoot::Library, BOOK, new.len() as u32)]).unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id));
}

/// What a cut replacement must look like once recovered: the copy's id
/// names whichever body the destination holds, under whichever spelling it
/// holds it, with a size that agrees and a digest that agrees or was left
/// as it was.
struct Expectation<'a> {
    id: BookId,
    old_name: &'a str,
    old: &'a [u8],
    /// What the ledger said of the old copy's bytes before the upload.
    old_source: Option<CachedSourceDigest>,
    new_name: &'a str,
    new: &'a [u8],
}

/// Check the card after a recovered cut, and say which body it held.
fn check_recovered(root: &Dir<'_>, expected: &Expectation<'_>, label: &str) -> bool {
    assert_eq!(
        replace::read(root).unwrap(),
        None,
        "{label}: the intent is cleared"
    );
    assert_eq!(
        install::read_intent(root).unwrap(),
        install::IntentState::Absent,
        "{label}"
    );
    let under_new = shelf_bytes(root, expected.new_name);
    let under_old = if expected.old_name == expected.new_name {
        None
    } else {
        shelf_bytes(root, expected.old_name)
    };
    let (spelled, held) = match (under_new, under_old) {
        (Some(held), None) => (expected.new_name, held),
        (None, Some(held)) => (expected.old_name, held),
        (new, old) => panic!("{label}: exactly one spelling holds a file: {new:?} {old:?}"),
    };
    let (locator, size, source) =
        record_by_id(root, expected.id).unwrap_or_else(|| panic!("{label}: the id survives"));
    assert_eq!(
        locator, spelled,
        "{label}: the record spells the place as the card does"
    );
    assert_eq!(
        size,
        held.len() as u32,
        "{label}: the record's size is the file's"
    );
    assert_eq!(record_count(root), 1, "{label}: one copy, one record");
    let landed_new = held == expected.new;
    if landed_new {
        assert!(
            digest_agrees(source, expected.new),
            "{label}: the new digest"
        );
    } else {
        assert_eq!(held, expected.old, "{label}: one of the two bodies");
        assert_eq!(
            source, expected.old_source,
            "{label}: the record's digest as it was"
        );
    }
    let (assigned, ids) = scan(root, &[(BookRoot::Library, spelled, held.len() as u32)]).unwrap();
    assert_eq!(assigned.matched, 1, "{label}");
    assert_eq!(assigned.minted, 0, "{label}");
    assert_eq!(ids[0], Some(expected.id), "{label}");
    landed_new
}

/// A power cut at every write from before the intent is published through
/// the install, the ledger's rewrite and the intent's clear, for a base
/// prepared by `prepare`, which returns the copy's id and what the ledger
/// says of its bytes. `old_name` is the spelling on the shelf; the upload is
/// spelled `new_name`. Both sides of the swap must be reached.
fn sweep_replacement(
    prepare: impl Fn(&Dir<'_>, &Dir<'_>) -> (BookId, Option<CachedSourceDigest>),
    old_name: &str,
    old: &[u8],
    new_name: &str,
    new: &[u8],
) {
    let disk = new_card();
    let (base, id, old_source) = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        let (id, old_source) = prepare(&root, &books);
        (disk.image(), id, old_source)
    };
    let expected = Expectation {
        id,
        old_name,
        old,
        old_source,
        new_name,
        new,
    };

    let uncut = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        upload(&root, &books, new_name, new, || disk.cut_writes_from(None))
            .unwrap()
            .expect("lands");
        disk.writes_seen()
    };
    assert!(
        uncut > 8,
        "a replacement that takes {uncut} writes proves little"
    );

    let mut saw = [false; 2];
    for cut in 1..=uncut {
        disk.restore(&base);
        {
            let mgr = open_mgr(&disk);
            let (root, books) = open_dirs(&mgr);
            let _ = upload(&root, &books, new_name, new, || {
                disk.cut_writes_from(Some(cut))
            });
            disk.cut_writes_from(None);
        }
        // Power is back.
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        let label = format!("cut at write {cut}");
        let recovered = recover(&root, &books);
        assert_ne!(recovered, Recovery::Refused, "{label}");
        let landed_new = check_recovered(&root, &expected, &label);
        saw[usize::from(landed_new)] = true;
    }
    assert_eq!(saw, [true; 2], "cuts landed on both sides of the swap");
}

#[test]
fn a_power_cut_anywhere_in_a_replacement_keeps_the_id_and_a_consistent_record() {
    let old = body(1, 3_000);
    let new = body(2, 4_100);
    sweep_replacement(
        |root, books| {
            sideload(books, BOOK, &old);
            let (_, ids) = scan(root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
            (ids[0].unwrap(), None)
        },
        BOOK,
        &old,
        BOOK,
        &new,
    );
}

/// A rollback puts the predecessor back under the spelling that was typed,
/// so a cut replacement spelled another way can leave the old bytes under
/// the new name. The record follows the file either way.
#[test]
fn a_power_cut_in_a_replacement_spelled_another_way_keeps_the_id_under_either_spelling() {
    let old = body(1, 3_000);
    let new = body(2, 4_100);
    sweep_replacement(
        |root, books| {
            sideload(books, BOOK, &old);
            let (_, ids) = scan(root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
            (ids[0].unwrap(), None)
        },
        BOOK,
        &old,
        BOOK_RESPELLED,
        &new,
    );
}

/// Between transactions a computer may replace a book with another of the
/// same size, which the ledger cannot see. The next upload over it must
/// still recover from any cut: the ledger's digest is evidence about bytes
/// that are gone, and the intent does not carry it as the predecessor's.
#[test]
fn a_predecessor_replaced_on_a_computer_is_still_recognised_after_a_cut() {
    let uploaded = body(1, 3_000);
    let swapped_in = body(7, 3_000);
    let new = body(2, 4_100);
    assert_eq!(
        uploaded.len(),
        swapped_in.len(),
        "same size, the case the ledger cannot see"
    );
    sweep_replacement(
        |root, books| {
            upload(root, books, BOOK, &uploaded, || {})
                .unwrap()
                .expect("lands");
            let (id, _, source) = record_for(root, BOOK).unwrap();
            assert!(
                digest_agrees(source, &uploaded),
                "the ledger recorded the upload"
            );
            overwrite_shelf(root, BOOK, &swapped_in);
            // The record stands, describing bytes that are no longer there.
            (id, source)
        },
        BOOK,
        &swapped_in,
        BOOK,
        &new,
    );
}

/// The intent's own writes, torn inside the sector, at every length that
/// leaves a partial entry. Two slots keep the entry before, and recovery
/// ends the same way.
#[test]
fn a_torn_intent_write_still_resolves() {
    let disk = new_card();
    let old = body(1, 3_000);
    let new = body(2, 4_100);
    let (base, id) = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        sideload(&books, BOOK, &old);
        let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
        (disk.image(), ids[0].unwrap())
    };
    let expected = Expectation {
        id,
        old_name: BOOK,
        old: &old,
        old_source: None,
        new_name: BOOK,
        new: &new,
    };

    // Which writes of an uncut replacement land on the intent's blocks: the
    // journal's creation, the publication and the clear. An entry spans two
    // sectors, so only the first of each slot carries the magic.
    let intent_writes: Vec<u32> = {
        let mgr = open_mgr(&disk);
        let (root, books) = open_dirs(&mgr);
        upload(&root, &books, BOOK, &new, || disk.cut_writes_from(None))
            .unwrap()
            .expect("lands");
        let image = disk.image();
        let blocks: Vec<u32> = (0..DISK_BLOCKS)
            .filter(|block| {
                let at = *block as usize * BLOCK_BYTES;
                image[at..at + 4] == REPLACE_JOURNAL_MAGIC
            })
            .collect();
        assert_eq!(blocks.len(), 2, "both slots hold an entry by now");
        disk.written_blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| blocks.contains(block))
            .map(|(index, _)| index as u32 + 1)
            .collect()
    };
    assert!(
        intent_writes.len() >= 2,
        "a publication and a clear at least: {intent_writes:?}"
    );

    for (transition, write) in intent_writes.iter().enumerate() {
        for landed in [1usize, 4, 5, 8, 24, 100, 300, 511] {
            disk.restore(&base);
            {
                let mgr = open_mgr(&disk);
                let (root, books) = open_dirs(&mgr);
                let _ = upload(&root, &books, BOOK, &new, || {
                    disk.tear_write_at(*write, landed)
                });
                disk.cut_writes_from(None);
            }
            let mgr = open_mgr(&disk);
            let (root, books) = open_dirs(&mgr);
            let label = format!("transition {transition}, {landed} bytes landed");
            assert_ne!(recover(&root, &books), Recovery::Refused, "{label}");
            check_recovered(&root, &expected, &label);
        }
    }
}

/// A destination holding neither the bytes that were meant to land nor a
/// predecessor whose digest was read in this session is nothing this can
/// resolve: the intent stands, the ledger is left alone, and no other change
/// to the shelf begins until the card is looked at. Putting the predecessor
/// back is looking at it.
#[test]
fn a_destination_holding_neither_landing_is_refused_until_looked_at() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    upload(&root, &books, BOOK, &old, || {})
        .unwrap()
        .expect("lands");
    let (id, _, source) = record_for(&root, BOOK).unwrap();
    assert!(digest_agrees(source, &old));

    // An intent whose caller read the predecessor, published and never acted
    // on. Then a stranger arrives at the place, which the sole-writer
    // contract forbids and which this therefore does not read as either.
    let new = body(2, 4_100);
    let standing = replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        Some(PredecessorSeen {
            locator: BOOK,
            byte_size: old.len() as u32,
            digest: Some(digest_of(&old)),
        }),
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    assert_eq!(standing.id, id);
    assert!(matches!(standing.predecessor, Predecessor::Known(_)));
    let stranger = body(9, 3_000);
    overwrite_shelf(&root, BOOK, &stranger);

    assert_eq!(recover(&root, &books), Recovery::Refused);
    assert!(replace::read(&root).unwrap().is_some(), "the intent stands");
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!((found, size), (id, old.len() as u32));
    assert!(digest_agrees(source, &old), "the ledger is untouched");
    assert!(
        matches!(
            StagedUpload::begin(&root, &books, "Other.epub", None).err(),
            Some(InstallError::Busy)
        ),
        "no other change to the shelf begins beside it"
    );
    assert_eq!(
        replace::begin(
            &root,
            BookRoot::Library,
            BOOK,
            None,
            digest_of(&new),
            &mut words()
        )
        .err(),
        Some(LedgerFault::Busy)
    );

    // The predecessor put back is the old landing.
    overwrite_shelf(&root, BOOK, &old);
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::Old));
    assert_eq!(replace::read(&root).unwrap(), None);
    upload(&root, &books, "Other.epub", &body(4, 1_000), || {})
        .unwrap()
        .expect("the shelf takes changes again");
}

/// A card with no shelf still has a library transaction to answer for.
///
/// The intent stands in /READER, not on the shelf, and it can name a copy at
/// the card root. A mount that reads only the install journal sees nothing,
/// and one that refuses on sight leaves a resolvable intent standing.
#[test]
fn a_card_root_replacement_is_answered_on_a_card_with_no_shelf() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let volume = mgr.open_volume(VolumeIdx(0)).expect("open volume");
    let raw_root = mgr
        .open_root_dir(volume.to_raw_volume())
        .expect("open root");
    let root = Directory::new(raw_root, &mgr);
    assert!(
        matches!(library::open_library_root(&root), Ok(None)),
        "this card has no shelf at all"
    );
    assert_eq!(
        replace::read(&root).expect("a card with no journal reads clean"),
        None,
        "and nothing standing, so a blank card is not refused"
    );

    let old = body(1, 3_000);
    sideload(&root, LOOSE, &old);
    let (_, ids) = scan(&root, &[(BookRoot::CardRoot, LOOSE, old.len() as u32)])
        .expect("the scan adopts the loose book");
    let id = ids[0].expect("under an id of its own");

    let new = body(2, 4_100);
    let standing = replace::begin(
        &root,
        BookRoot::CardRoot,
        LOOSE,
        Some(PredecessorSeen {
            locator: LOOSE,
            byte_size: old.len() as u32,
            digest: Some(digest_of(&old)),
        }),
        digest_of(&new),
        &mut words(),
    )
    .expect("the intent publishes without a shelf");
    assert_eq!(standing.id, id);

    assert!(
        matches!(library::open_library_root(&root), Ok(None)),
        "there is still no shelf"
    );
    assert!(
        install::read_intent(&root).expect("journal") == install::IntentState::Absent,
        "and no install journal to report the copy in flight"
    );
    assert!(
        replace::read(&root).expect("the journal reads").is_some(),
        "so the library intent is the only thing that says one is"
    );

    // The copy it names is at the card root, which reads without a shelf, so
    // the landing can be established and the intent answered rather than
    // parked.
    overwrite_at(&root, BookRoot::CardRoot, LOOSE, &new);
    assert_eq!(
        replace::recover(&root),
        Ok(Recovery::Settled(Landing::New)),
        "the new bytes at the place are the new landing"
    );
    assert_eq!(
        replace::read(&root).expect("the journal reads"),
        None,
        "and the intent clears"
    );
    let (locator, size, source) = record_by_id(&root, id).expect("the record stands");
    assert_eq!((locator.as_str(), size), (LOOSE, new.len() as u32));
    assert!(
        digest_agrees(source, &new),
        "under the id it had, carrying the new bytes"
    );
}

/// The two journals disagree here, and the firmware has to read both.
///
/// A replacement intent stands, the install that carried it settled and
/// cleared, and the destination holds neither legal landing. `recover_installs`
/// then has nothing to report: no intent of its own, and complete. Only the
/// library intent knows the place is unresolved, so anything deciding on the
/// install outcome alone would go on serving a catalog row whose identity
/// nothing has vouched for.
#[test]
fn a_refused_replacement_leaves_the_install_journal_reporting_nothing() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    upload(&root, &books, BOOK, &old, || {})
        .unwrap()
        .expect("lands");

    let new = body(2, 4_100);
    replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        Some(PredecessorSeen {
            locator: BOOK,
            byte_size: old.len() as u32,
            digest: Some(digest_of(&old)),
        }),
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    overwrite_shelf(&root, BOOK, &body(9, 3_000));

    reclaim::recover(&root, Some(&books)).expect("reclaim settles");
    let outcome = install::recover_installs(&root, &books);
    assert!(
        outcome.complete,
        "the install journal has nothing in flight"
    );
    assert!(!outcome.had_intent, "and reports no intent of its own");
    assert!(!outcome.touched_shelf, "and changed nothing on the shelf");
    assert_eq!(
        replace::recover(&root),
        Ok(Recovery::Refused),
        "while the library intent cannot say what stands at the place"
    );
    assert!(
        replace::read(&root).unwrap().is_some(),
        "so it stays standing"
    );
}

/// The installer does not read the predecessor, so its intent says
/// "unknown" whatever the ledger recorded of it, and a stranger at the place
/// under a standing intent is read as the predecessor: the sole-writer
/// contract is what makes that sound, and the ledger's digest is left as
/// the evidence it is.
#[test]
fn the_installer_does_not_promote_the_ledgers_digest_to_the_predecessors() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    upload(&root, &books, BOOK, &old, || {})
        .unwrap()
        .expect("lands");
    let (id, _, source) = record_for(&root, BOOK).unwrap();
    assert!(digest_agrees(source, &old));

    let new = body(2, 4_100);
    let standing = replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        Some(PredecessorSeen {
            locator: BOOK,
            byte_size: old.len() as u32,
            digest: None,
        }),
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    assert_eq!(standing.id, id, "the id is the record's");
    assert_eq!(
        standing.predecessor,
        Predecessor::Unknown,
        "but the digest is not the record's"
    );
    overwrite_shelf(&root, BOOK, &body(9, 3_000));
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::Old));
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!((found, size), (id, old.len() as u32));
    assert!(
        digest_agrees(source, &old),
        "the ledger's evidence is left as it was"
    );
}

/// An intent for a place nothing held, whose install rolled back or was cut
/// before it began, resolves to the old landing on an empty destination and
/// leaves no record behind.
#[test]
fn where_nothing_stood_nothing_standing_is_the_old_landing() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let new = body(2, 4_100);
    replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        None,
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::Old));
    assert_eq!(record_for(&root, BOOK), None);
    assert_eq!(record_count(&root), 0);

    // But a stranger where nothing stood is not a landing.
    replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        None,
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    sideload(&books, BOOK, &body(9, 500));
    assert_eq!(recover(&root, &books), Recovery::Refused);
}

/// The new digest is decisive whatever was recorded about the predecessor.
/// Recovery that finds the new bytes on the shelf publishes the record even
/// when the install journal is long gone.
#[test]
fn the_new_bytes_at_the_destination_are_published_under_the_old_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload(&books, BOOK, &old);
    let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
    let id = ids[0].unwrap();

    let new = body(2, 4_100);
    replace::begin(
        &root,
        BookRoot::Library,
        BOOK,
        Some(PredecessorSeen {
            locator: BOOK,
            byte_size: old.len() as u32,
            digest: None,
        }),
        digest_of(&new),
        &mut words(),
    )
    .unwrap();
    overwrite_shelf(&root, BOOK, &new);
    assert_eq!(recover(&root, &books), Recovery::Settled(Landing::New));
    let (found, size, source) = record_for(&root, BOOK).unwrap();
    assert_eq!(found, id);
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert_eq!(replace::read(&root).unwrap(), None);
    assert_eq!(recover(&root, &books), Recovery::Nothing);
}

/// A ledger with no room for one more record lets a missing copy go to make
/// it, and refuses cleanly before anything is journalled when there is none
/// to let go of. A 64 MiB card, since the records alone are 21 MiB.
#[test]
fn a_full_ledger_yields_a_missing_copy_or_refuses_before_the_install_begins() {
    let disk = new_card_of(128 * 1024);
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    // Every record a live copy but one, which the last scan found missing.
    let ids: Vec<BookId> = (0..LEDGER_MAX_RECORDS)
        .map(|index| {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
            BookId::from_bytes(bytes).unwrap()
        })
        .collect();
    let missing_index = 777usize;
    ledger::write_generation(
        &root,
        None,
        &mut |_, record| Carry::Keep(Kept::of(record)),
        |writer| {
            for (index, id) in ids.iter().enumerate() {
                let mut locator = heapless::String::<32>::new();
                use core::fmt::Write as _;
                write!(locator, "B{index}.epub").unwrap();
                writer.append(&LedgerRecord {
                    id: *id,
                    root: BookRoot::Library,
                    locator: locator.as_str(),
                    byte_size: 1_000,
                    misses: u8::from(index == missing_index),
                    source: None,
                })?;
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);

    // A fresh upload lands and is adopted: the missing copy made room.
    let new = body(2, 2_048);
    upload(&root, &books, BOOK, &new, || {})
        .unwrap()
        .expect("lands");
    let (fresh, size, source) = record_for(&root, BOOK).expect("adopted");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert!(!ids.contains(&fresh));
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    assert_eq!(
        record_by_id(&root, ids[missing_index]),
        None,
        "the missing copy's record made the room"
    );
    assert!(replace::read(&root).unwrap().is_none());

    // With every record a live copy, a second fresh upload is refused before
    // any journal is written, and the shelf and the ledger are left as they
    // were.
    let refused = upload(&root, &books, "Other.epub", &body(3, 1_500), || {});
    assert!(
        matches!(refused, Err(InstallError::Ledger)),
        "refused by the ledger: {refused:?}"
    );
    assert_eq!(
        replace::read(&root).unwrap(),
        None,
        "no intent was published"
    );
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent,
        "and no install journal"
    );
    assert_eq!(shelf_bytes(&root, "Other.epub"), None);
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    // A replacement of an adopted copy still goes through: no record is
    // added.
    upload(&root, &books, BOOK, &body(4, 3_000), || {})
        .unwrap()
        .expect("a replacement needs no room");
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    assert_eq!(
        record_by_id(&root, fresh).map(|(_, size, _)| size),
        Some(3_000)
    );
}

/// A digest the install verified travels into the record; a stored digest
/// that disagrees with the file is what `SourceDigest` versus its cached form
/// exists to keep apart, and the record's type says which it holds.
#[test]
fn the_recorded_digest_is_evidence_that_agrees_with_the_bytes() {
    let bytes = body(7, 999);
    let fresh: SourceDigest = digest_of(&bytes);
    let cached = CachedSourceDigest::new(fresh);
    assert!(cached.agrees_with(&fresh));
    assert!(!cached.agrees_with(&digest_of(&body(8, 999))));
}

/// The byte offset of UTF-16 unit `i` inside a 32-byte LFN entry, whose
/// thirteen units live in three regions.
fn lfn_unit_offset(i: usize) -> usize {
    match i {
        0..=4 => 1 + 2 * i,
        5..=10 => 14 + 2 * (i - 5),
        _ => 28 + 2 * (i - 11),
    }
}

/// Rewrite a single-entry long name the driver created into another name of
/// the same or shorter length, directly in the image bytes.
///
/// The driver refuses to create two names differing only in case, which is
/// exactly the directory another operating system can leave behind, so the
/// tests forge one. The 8.3 alias and its checksum are untouched: an alias
/// unrelated to its long name is legal FAT, and is what any `~1` alias
/// already looks like.
fn rewrite_long_name(data: &mut [u8], created: &str, forged: &str) {
    let created_units: Vec<u16> = created.encode_utf16().collect();
    let forged_units: Vec<u16> = forged.encode_utf16().collect();
    assert!(created_units.len() <= 13, "one LFN entry holds 13 units");
    assert!(forged_units.len() <= created_units.len());
    let mut patched = 0;
    for at in (0..data.len().saturating_sub(31)).step_by(32) {
        let entry = &data[at..at + 32];
        // One terminal LFN entry: sequence 1 with the last-in-chain flag.
        if entry[0] != 0x41 || entry[11] != 0x0F || entry[12] != 0 {
            continue;
        }
        let unit_at = |i: usize| {
            let offset = lfn_unit_offset(i);
            u16::from_le_bytes([entry[offset], entry[offset + 1]])
        };
        let matches = (0..created_units.len()).all(|i| unit_at(i) == created_units[i])
            && (created_units.len() == 13 || unit_at(created_units.len()) == 0x0000);
        if !matches {
            continue;
        }
        for (i, unit) in forged_units.iter().enumerate() {
            let offset = lfn_unit_offset(i);
            data[at + offset..at + offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        // Terminator where the forged name ends, padding over the rest of
        // what the created name used.
        for i in forged_units.len()..=created_units.len().min(12) {
            let unit: u16 = if i == forged_units.len() {
                0x0000
            } else {
                0xFFFF
            };
            let offset = lfn_unit_offset(i);
            data[at + offset..at + offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        patched += 1;
    }
    assert_eq!(patched, 1, "exactly one directory entry should match");
}

/// A computer can leave two entries on the shelf that differ only by case,
/// which the card matches as one name. An upload to that name cannot land:
/// the shelf has one namespace with case ignored, so whichever twin the
/// install replaced, the other would refuse the landing and the rollback
/// alike, halfway through. It is refused before anything is journalled or
/// moved, whether the upload spells one twin exactly or neither, and
/// whichever twin the directory lists first. Once the shelf holds one, the
/// upload goes through as a respelling of that one.
#[test]
fn case_variant_twins_on_the_shelf_are_refused_before_anything_moves() {
    let disk = new_card();
    let a = body(1, 2_000);
    let b = body(2, 2_500);
    {
        let mgr = open_mgr(&disk);
        let (_, books) = open_dirs(&mgr);
        sideload(&books, "FooA.epub", &a);
        sideload(&books, "fooB.epub", &b);
    }
    {
        let mut data = disk.0.data.borrow_mut();
        rewrite_long_name(&mut data, "FooA.epub", "Foo.epub");
        rewrite_long_name(&mut data, "fooB.epub", "foo.epub");
    }
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    assert_eq!(shelf_bytes(&root, "Foo.epub").as_deref(), Some(&a[..]));
    assert_eq!(shelf_bytes(&root, "foo.epub").as_deref(), Some(&b[..]));
    let (assigned, ids) = scan(
        &root,
        &[
            (BookRoot::Library, "Foo.epub", a.len() as u32),
            (BookRoot::Library, "foo.epub", b.len() as u32),
        ],
    )
    .unwrap();
    assert_eq!(assigned.minted, 2);
    let (first, second) = (ids[0].unwrap(), ids[1].unwrap());

    // The exact spelling of the twin the directory lists second, and a
    // spelling that is neither.
    for name in ["foo.epub", "FOO.EPUB"] {
        let refused = upload(&root, &books, name, &body(3, 3_000), || {});
        assert!(
            matches!(refused, Err(InstallError::Ambiguous)),
            "{name}: two holders of the name are a refusal: {refused:?}"
        );
        assert_eq!(
            replace::read(&root).unwrap(),
            None,
            "{name}: no intent was published"
        );
        assert_eq!(
            install::read_intent(&root).unwrap(),
            install::IntentState::Absent,
            "{name}: and no install journal"
        );
        assert_eq!(
            shelf_bytes(&root, "Foo.epub").as_deref(),
            Some(&a[..]),
            "{name}: the shelf is as it was"
        );
        assert_eq!(shelf_bytes(&root, "foo.epub").as_deref(), Some(&b[..]));
        assert_eq!(
            record_for(&root, "Foo.epub").map(|(id, size, _)| (id, size)),
            Some((first, a.len() as u32)),
            "{name}: and so is the ledger"
        );
        assert_eq!(
            record_for(&root, "foo.epub").map(|(id, size, _)| (id, size)),
            Some((second, b.len() as u32))
        );
        assert_eq!(record_count(&root), 2);
    }

    // The twin taken away on a computer, the upload is a respelling of the
    // one left, which keeps its id.
    remove_from_shelf(&root, "Foo.epub");
    let c = body(4, 3_000);
    upload(&root, &books, "FOO.EPUB", &c, || {})
        .unwrap()
        .expect("one holder is a replacement");
    assert_eq!(shelf_bytes(&root, "FOO.EPUB").as_deref(), Some(&c[..]));
    assert_eq!(shelf_bytes(&root, "foo.epub"), None);
    let (locator, size, source) = record_by_id(&root, second).expect("the id survives");
    assert_eq!(locator, "FOO.EPUB");
    assert_eq!(size, c.len() as u32);
    assert!(digest_agrees(source, &c));
    assert_eq!(replace::read(&root).unwrap(), None);
}

/// A full ledger makes room by letting a missing copy go, but only one
/// whose place is empty now: a copy the last scan missed can have been put
/// back since, and its counter alone would evict a book that is on the
/// shelf. The candidate is chosen and verified in preflight, written into
/// the intent, and is what the publication evicts; with no such candidate
/// the install is refused before anything is journalled.
#[test]
fn only_a_missing_copy_verified_absent_makes_room_in_a_full_ledger() {
    let disk = new_card_of(128 * 1024);
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let ids: Vec<BookId> = (0..LEDGER_MAX_RECORDS)
        .map(|index| {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
            BookId::from_bytes(bytes).unwrap()
        })
        .collect();
    // Two records the last scan found missing. The first in ledger order
    // has been put back on the shelf since; the second is still gone.
    let back_index = 300usize;
    let gone_index = 900usize;
    let back = body(6, 1_000);
    sideload(&books, "Back.epub", &back);
    ledger::write_generation(
        &root,
        None,
        &mut |_, record| Carry::Keep(Kept::of(record)),
        |writer| {
            for (index, id) in ids.iter().enumerate() {
                let mut locator = heapless::String::<32>::new();
                use core::fmt::Write as _;
                if index == back_index {
                    locator.push_str("Back.epub").unwrap();
                } else if index == gone_index {
                    locator.push_str("Gone.epub").unwrap();
                } else {
                    write!(locator, "B{index}.epub").unwrap();
                }
                writer.append(&LedgerRecord {
                    id: *id,
                    root: BookRoot::Library,
                    locator: locator.as_str(),
                    byte_size: 1_000,
                    misses: match index {
                        _ if index == back_index => 1,
                        _ if index == gone_index => 2,
                        _ => 0,
                    },
                    source: None,
                })?;
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);

    // The copy that is gone makes the room, not the one that came first.
    let new = body(2, 2_048);
    upload(&root, &books, BOOK, &new, || {})
        .unwrap()
        .expect("lands");
    assert!(record_for(&root, BOOK).is_some(), "adopted");
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    assert_eq!(
        record_by_id(&root, ids[gone_index]),
        None,
        "the gone copy made the room"
    );
    assert_eq!(
        record_by_id(&root, ids[back_index]).map(|(locator, _, _)| locator),
        Some("Back.epub".to_owned()),
        "the copy that came back kept its record"
    );
    assert_eq!(shelf_bytes(&root, "Back.epub").as_deref(), Some(&back[..]));

    // With the only missing counter naming a copy that is on the shelf, a
    // fresh upload is refused before anything is journalled.
    let refused = replace::begin(
        &root,
        BookRoot::Library,
        "Other.epub",
        None,
        digest_of(&body(3, 1_500)),
        &mut words(),
    );
    assert!(
        matches!(refused, Err(LedgerFault::Full)),
        "a counter is not absence: {refused:?}"
    );
    let refused = upload(&root, &books, "Other.epub", &body(3, 1_500), || {});
    assert!(
        matches!(refused, Err(InstallError::Ledger)),
        "refused by the ledger: {refused:?}"
    );
    assert_eq!(replace::read(&root).unwrap(), None);
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent
    );
    assert_eq!(shelf_bytes(&root, "Other.epub"), None);
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    assert!(record_by_id(&root, ids[back_index]).is_some());

    // Taken away again on a computer, the same record is the candidate: the
    // intent names it, and the publication evicts exactly it.
    remove_from_shelf(&root, "Back.epub");
    let standing = replace::begin(
        &root,
        BookRoot::Library,
        "Other.epub",
        None,
        digest_of(&body(3, 1_500)),
        &mut words(),
    )
    .expect("room now");
    assert_eq!(standing.evict, Some(ids[back_index]));
    assert_eq!(
        replace::read(&root).unwrap().map(|standing| standing.evict),
        Some(Some(ids[back_index])),
        "the reservation is in the intent"
    );
    // Nothing stood and nothing landed: an old landing clears the intent
    // without publishing.
    replace::settle(&root, Landing::Old).unwrap();
    assert_eq!(replace::read(&root).unwrap(), None);
    assert!(record_by_id(&root, ids[back_index]).is_some());

    let other = body(3, 1_500);
    // Minted from further along the word source, so that this fresh copy
    // does not draw the id the first one did.
    let mut later = words();
    for _ in 0..4 {
        later();
    }
    upload_minting(&root, &books, "Other.epub", &other, &mut later)
        .unwrap()
        .expect("lands");
    let (_, size, source) = record_for(&root, "Other.epub").expect("adopted");
    assert_eq!(size, other.len() as u32);
    assert!(digest_agrees(source, &other));
    assert_eq!(record_count(&root), LEDGER_MAX_RECORDS);
    assert_eq!(record_by_id(&root, ids[back_index]), None);
    assert_eq!(replace::read(&root).unwrap(), None);
}

/// Take what the shelf holds under `name`, spelled exactly so, away, as a
/// computer would.
fn remove_from_shelf(root: &Dir<'_>, name: &str) {
    let path = LibraryPath::parse(name).unwrap();
    library::with_book_at(root, BookRoot::Library, &path, |dir, alias| {
        let mut alias_text = heapless::String::<12>::new();
        use core::fmt::Write as _;
        write!(alias_text, "{}", alias).unwrap();
        dir.delete_entry_in_dir(alias_text.as_str())
            .expect("delete book");
    })
    .expect("shelf readable")
    .expect("the book is there to take away");
}

/// [`upload`], minting from `random` rather than the fixed word source: a
/// test that adopts two fresh copies must not draw the same id for both.
fn upload_minting(
    root: &Dir<'_>,
    books: &Dir<'_>,
    name: &str,
    bytes: &[u8],
    random: &mut impl FnMut() -> u32,
) -> Result<Option<install::Landed>, InstallError> {
    let mut staged = StagedUpload::begin(root, books, name, None)?;
    staged.write(bytes)?;
    staged.install(root, books, random)
}

/// Put a book on the shelf the way a computer or an old build does: an 8.3
/// entry with no long name at all, which a listing shows, and a locator
/// names, by its rendered alias.
fn sideload_short_only(books: &Dir<'_>, alias: &str, bytes: &[u8]) {
    let file = books
        .open_file_in_dir(alias, Mode::ReadWriteCreate)
        .expect("create short-only book");
    file.write(bytes).expect("write book");
    file.close().expect("close book");
}

/// A book with no long name is a book the library adopts under its rendered
/// alias, so an upload landing on that name is a managed replacement like
/// any other and must keep the copy's id. The install would meet that entry
/// anyway: FAT gives a directory one namespace over long names and aliases
/// together, so a staged file moved in under a name an alias holds is
/// refused, halfway through a transaction whose intent said nothing stood
/// there.
#[test]
fn a_book_with_no_long_name_is_replaced_under_its_alias_and_keeps_its_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload_short_only(&books, "BOOK1234.EPU", &old);
    let (assigned, ids) = scan(
        &root,
        &[(BookRoot::Library, "BOOK1234.EPU", old.len() as u32)],
    )
    .unwrap();
    assert_eq!(assigned.minted, 1);
    let id = ids[0].unwrap();

    // Minted from further along the word source, so a fresh id would be a
    // different id and the assertion below means what it says.
    let mut later = words();
    for _ in 0..4 {
        later();
    }
    let new = body(2, 4_100);
    upload_minting(&root, &books, "BOOK1234.EPU", &new, &mut later)
        .unwrap()
        .expect("the book lands");
    assert_eq!(
        shelf_bytes(&root, "BOOK1234.EPU").as_deref(),
        Some(&new[..])
    );
    let (found, size, source) = record_for(&root, "BOOK1234.EPU").expect("still a record");
    assert_eq!(found, id, "the copy kept its id");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new), "and took the new digest");
    assert_eq!(record_count(&root), 1, "one copy, one record");
    assert_eq!(replace::read(&root).unwrap(), None, "the intent is cleared");
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent
    );
    let (assigned, ids) = scan(
        &root,
        &[(BookRoot::Library, "BOOK1234.EPU", new.len() as u32)],
    )
    .unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id));
}

/// A shelf can hold an entry with a long name and another with only an
/// alias that answer to the same name, which the ledger knows as two
/// records. Neither can be replaced while the other is there, so the upload
/// is refused before anything is journalled, as it is for two long names.
#[test]
fn an_alias_and_a_long_name_answering_alike_are_refused_before_anything_moves() {
    let disk = new_card();
    let alias_side = body(1, 2_000);
    let long_side = body(2, 2_500);
    {
        let mgr = open_mgr(&disk);
        let (_, books) = open_dirs(&mgr);
        sideload_short_only(&books, "BOOK1234.EPU", &alias_side);
        sideload(&books, "bookXXXXX.epu", &long_side);
    }
    {
        let mut data = disk.0.data.borrow_mut();
        rewrite_long_name(&mut data, "bookXXXXX.epu", "book1234.epu");
    }
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    assert_eq!(
        shelf_bytes(&root, "BOOK1234.EPU").as_deref(),
        Some(&alias_side[..])
    );
    assert_eq!(
        shelf_bytes(&root, "book1234.epu").as_deref(),
        Some(&long_side[..])
    );

    let refused = upload(&root, &books, "BOOK1234.EPU", &body(3, 3_000), || {});
    assert!(
        matches!(refused, Err(InstallError::Ambiguous)),
        "an alias and a long name answering alike are a refusal: {refused:?}"
    );
    assert_eq!(replace::read(&root).unwrap(), None, "no intent");
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent,
        "and no install journal"
    );
    assert_eq!(
        shelf_bytes(&root, "BOOK1234.EPU").as_deref(),
        Some(&alias_side[..]),
        "the shelf is as it was"
    );
    assert_eq!(
        shelf_bytes(&root, "book1234.epu").as_deref(),
        Some(&long_side[..])
    );
}

/// The books an old build uploaded are an alias with sidecars and no long
/// name, and a re-upload of one arrives under a long name that matches
/// nothing on the shelf. The identity sidecar finds it, and the copy keeps
/// its id as its place is respelled to the long name the shelf now shows.
#[test]
fn a_book_from_before_long_names_keeps_its_id_when_re_uploaded() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let client_name = b"Middlemarch.epub";
    let alias = proto::upload::sanitized_name(client_name);
    let identity = proto::upload::hash_identity(client_name);
    let old = body(1, 3_000);
    sideload_short_only(&books, alias.as_str(), &old);
    write_legacy_sidecars(&root, alias.as_str(), identity, "Middlemarch");
    let (assigned, ids) = scan(
        &root,
        &[(BookRoot::Library, alias.as_str(), old.len() as u32)],
    )
    .unwrap();
    assert_eq!(assigned.minted, 1);
    let id = ids[0].unwrap();

    let long_name = proto::upload::wireless_epub_filename(client_name);
    let mut later = words();
    for _ in 0..4 {
        later();
    }
    let new = body(2, 4_100);
    let mut staged = StagedUpload::begin(
        &root,
        &books,
        long_name.as_str(),
        Some(install::LegacyKey {
            alias: {
                let mut owned = heapless::String::<12>::new();
                owned.push_str(alias.as_str()).unwrap();
                owned
            },
            identity,
        }),
    )
    .expect("stage");
    staged.write(&new).expect("stream");
    staged
        .install(&root, &books, &mut later)
        .expect("install")
        .expect("the book lands");

    assert_eq!(
        shelf_bytes(&root, long_name.as_str()).as_deref(),
        Some(&new[..]),
        "the shelf shows the long name now"
    );
    assert_eq!(shelf_bytes(&root, alias.as_str()), None);
    let (locator, size, source) = record_by_id(&root, id).expect("the id survives");
    assert_eq!(locator, long_name.as_str(), "respelled to the long name");
    assert_eq!(size, new.len() as u32);
    assert!(digest_agrees(source, &new));
    assert_eq!(record_count(&root), 1, "one copy, one record");
    assert_eq!(replace::read(&root).unwrap(), None);
}

/// The sidecars an old build wrote beside a book: its identity, and the
/// display label the alias could not hold.
fn write_legacy_sidecars(root: &Dir<'_>, alias: &str, identity: u64, label: &str) {
    let cache_root = root.open_dir(CACHE_ROOT_DIR).unwrap_or_else(|_| {
        root.make_dir_in_dir(CACHE_ROOT_DIR).expect("make READER");
        root.open_dir(CACHE_ROOT_DIR).expect("open READER")
    });
    if cache_root.open_dir("LABELS").is_err() {
        cache_root.make_dir_in_dir("LABELS").expect("make LABELS");
    }
    let labels = cache_root.open_dir("LABELS").expect("labels");
    let stem = alias.split('.').next().expect("stem");

    let mut id_name = String::from(stem);
    id_name.push_str(".ID");
    let file = labels
        .open_file_in_dir(id_name.as_str(), Mode::ReadWriteCreate)
        .expect("identity sidecar");
    file.write(&identity.to_le_bytes()).expect("write identity");
    file.close().expect("close");

    let mut txt_name = String::from(stem);
    txt_name.push_str(".TXT");
    let file = labels
        .open_file_in_dir(txt_name.as_str(), Mode::ReadWriteCreate)
        .expect("label sidecar");
    file.write(label.as_bytes()).expect("write label");
    file.close().expect("close");
}

/// A folder can carry a book's name: unpacking an EPUB on a computer leaves
/// one, and a shelf tidied by hand can too. It is not a book to replace and
/// it cannot be parked, and FAT counts it in the same namespace as the
/// books, so the landing would be refused by it halfway through a
/// transaction whose intent had already said nothing stood there. Refused
/// at the preflight instead, with neither journal written.
#[test]
fn a_folder_holding_the_upload_name_is_refused_before_anything_moves() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    books
        .make_dir_in_dir_lfn(BOOK)
        .expect("a folder named like a book");

    let refused = upload(&root, &books, BOOK, &body(2, 4_100), || {});
    assert!(
        matches!(refused, Err(InstallError::Ambiguous)),
        "a folder holding the name is a refusal: {refused:?}"
    );
    assert_eq!(
        replace::read(&root).unwrap(),
        None,
        "no intent was published"
    );
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent,
        "and no install journal"
    );
    assert_eq!(record_count(&root), 0, "and no record");
    // Nothing was begun, so there is nothing owed: the shelf can still be
    // read and written. A wedge here would report an install it cannot
    // finish, on this mount and every one after it.
    let outcome = install::recover_installs(&root, &books);
    assert!(
        outcome.complete && !outcome.had_intent,
        "nothing to recover: {outcome:?}"
    );
    let other = body(3, 2_000);
    upload(&root, &books, "Other.epub", &other, || {})
        .unwrap()
        .expect("another book still installs");
    assert_eq!(
        shelf_bytes(&root, "Other.epub").as_deref(),
        Some(&other[..])
    );

    // The folder is where it was, which is why the same upload is refused
    // the same way.
    let refused = upload(&root, &books, BOOK, &body(4, 4_100), || {});
    assert!(
        matches!(refused, Err(InstallError::Ambiguous)),
        "still refused: {refused:?}"
    );
}

/// A folder answering to the book's name by FAT's rules blocks the landing
/// as surely as one spelled exactly, so the book beside it must not be
/// parked: it would come off the shelf for a landing that cannot happen.
#[test]
fn a_folder_answering_like_the_book_is_refused_before_the_book_is_parked() {
    let disk = new_card();
    let old = body(1, 3_000);
    {
        let mgr = open_mgr(&disk);
        let (_, books) = open_dirs(&mgr);
        sideload(&books, BOOK, &old);
        // Forged, as the twins are: the driver holds one namespace over
        // books and folders together, so it refuses to make this folder
        // beside that book. Another operating system need not.
        books.make_dir_in_dir_lfn("duneQ.epub").expect("mkdir");
    }
    {
        let mut data = disk.0.data.borrow_mut();
        rewrite_long_name(&mut data, "duneQ.epub", BOOK_RESPELLED);
    }
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, old.len() as u32)]).unwrap();
    let id = ids[0].unwrap();

    let refused = upload(&root, &books, BOOK, &body(2, 4_100), || {});
    assert!(
        matches!(refused, Err(InstallError::Ambiguous)),
        "a folder answering alike is a refusal: {refused:?}"
    );
    assert_eq!(replace::read(&root).unwrap(), None, "no intent");
    assert_eq!(
        install::read_intent(&root).unwrap(),
        install::IntentState::Absent,
        "and no install journal"
    );
    assert_eq!(
        shelf_bytes(&root, BOOK).as_deref(),
        Some(&old[..]),
        "the book is still on the shelf, not parked"
    );
    assert_eq!(
        record_for(&root, BOOK).map(|(found, size, _)| (found, size)),
        Some((id, old.len() as u32)),
        "and its record is as it was"
    );
    assert_eq!(record_count(&root), 1);
}

/// A reader may keep two copies of one book on purpose, and the two are
/// separate books to the library however identical their bytes: one id
/// each, one record each, and state that stays where it was put. Reading
/// one does not move the other, which today means their positions are filed
/// in cache directories of their own.
#[test]
fn two_identical_copies_are_two_ids_with_state_of_their_own() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let shared = body(1, 3_000);
    let size = shared.len() as u32;
    let twin = "Dune (2).epub";
    sideload(&books, BOOK, &shared);
    sideload(&books, twin, &shared);
    let (assigned, ids) = scan(
        &root,
        &[
            (BookRoot::Library, BOOK, size),
            (BookRoot::Library, twin, size),
        ],
    )
    .unwrap();
    assert_eq!(assigned.minted, 2);
    let (first, second) = (ids[0].unwrap(), ids[1].unwrap());
    assert_ne!(first, second, "identical bytes, two library entries");
    assert_eq!(
        digest_of(&shelf_bytes(&root, BOOK).unwrap()),
        digest_of(&shelf_bytes(&root, twin).unwrap()),
        "and one source digest between them"
    );

    let live = ledger::open(&root).unwrap().unwrap();
    let one = ledger::find_by_id(&root, &live, first).unwrap().unwrap();
    let other = ledger::find_by_id(&root, &live, second).unwrap().unwrap();
    assert_eq!(one.locator(), Some(BOOK), "each id names its own copy");
    assert_eq!(other.locator(), Some(twin));
    assert_ne!(
        cache_key_from(source_hash_at(BookRoot::Library, BOOK, size)),
        cache_key_from(source_hash_at(BookRoot::Library, twin, size)),
        "and their state is filed apart"
    );

    // Replacing one is a change to that copy alone: the other keeps its id,
    // its place, its size and what the ledger says of its bytes.
    let newer = body(2, 4_100);
    upload(&root, &books, BOOK, &newer, || {})
        .unwrap()
        .expect("lands");
    assert_eq!(
        record_for(&root, BOOK).map(|(id, size, _)| (id, size)),
        Some((first, newer.len() as u32)),
        "the copy that was replaced kept its id and took the new size"
    );
    let live = ledger::open(&root).unwrap().unwrap();
    let other = ledger::find_by_id(&root, &live, second).unwrap().unwrap();
    assert_eq!(other.locator(), Some(twin));
    assert_eq!(other.byte_size, size);
    assert_eq!(other.source, None, "nothing was said about the other copy");
    assert_eq!(
        shelf_bytes(&root, twin).as_deref(),
        Some(&shared[..]),
        "and its bytes are where they were"
    );

    // A third copy of those same bytes arriving as an upload is a third
    // entry rather than a match onto either: same digest, same size as the
    // twin, and an id of its own.
    // Two mints have already come off the fixture's word source, one per
    // copy, and an id is four draws.
    let mut later = words();
    for _ in 0..8 {
        later();
    }
    upload_minting(&root, &books, "Third.epub", &shared, &mut later)
        .unwrap()
        .expect("lands");
    let (third, third_size, third_source) = record_for(&root, "Third.epub").expect("adopted");
    assert!(![first, second].contains(&third), "a third id");
    assert_eq!(third_size, size);
    assert!(
        digest_agrees(third_source, &shared),
        "the bytes the twin holds, recorded under an id of its own"
    );
    assert_eq!(record_count(&root), 3);
}

/// Deleting a book on a computer and uploading it again is an ordinary
/// thing to do, and it used to leave the ledger with two ids for one file:
/// the record of the copy that was deleted still named the place, and the
/// install, finding nothing there to replace, adopted the new copy under a
/// fresh id at the same place. Both were live, so the scan's join picked
/// between them by ledger order, and the id the install had published lost
/// to the one it had never heard of. Publishing a record now takes the
/// place with it.
#[test]
fn a_book_deleted_and_uploaded_again_is_one_copy_under_the_id_the_install_gave_it() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    sideload(&books, BOOK, &bytes);
    let (_, ids) = scan(&root, &[(BookRoot::Library, BOOK, size)]).unwrap();
    let adopted = ids[0].unwrap();

    // The book goes away on a computer, and the next scan ages its record.
    remove_from_shelf(&root, BOOK);
    let (assigned, _) = scan(&root, &[]).unwrap();
    assert_eq!(assigned.missing, 1);

    // The reader uploads the same book again, to the same name.
    let mut later = words();
    for _ in 0..4 {
        later();
    }
    upload_minting(&root, &books, BOOK, &bytes, &mut later)
        .unwrap()
        .expect("lands");
    let (installed, installed_size, source) = record_for(&root, BOOK).expect("adopted");
    assert_ne!(installed, adopted, "nothing established continuity");
    assert_eq!(installed_size, size);
    assert!(digest_agrees(source, &bytes));
    assert_eq!(
        record_count(&root),
        1,
        "the deleted copy's claim on the place went with it"
    );
    let live = ledger::open(&root).unwrap().unwrap();
    assert_eq!(
        ledger::find_by_id(&root, &live, adopted).unwrap(),
        None,
        "and its id names nothing rather than the new copy's file"
    );

    // So the scan reads the file as the copy the install said it was.
    let (assigned, ids) = scan(&root, &[(BookRoot::Library, BOOK, size)]).unwrap();
    assert_eq!(
        assigned,
        Assignment {
            matched: 1,
            ..Assignment::default()
        }
    );
    assert_eq!(ids, vec![Some(installed)]);
}

/// A book replaced on a computer by one of another size is a new copy at an
/// old name: the row no longer matches the record that named it, so the row
/// is minted an id of its own and the old record is carried as missing, at
/// a name the new copy now holds. What the old id must not do is answer
/// with that name. Nothing established that the two copies are the same
/// book, and resolving the old id's state against the new copy's file is
/// the merge the whole model exists to prevent.
#[test]
fn a_copy_replaced_on_a_computer_leaves_the_old_id_with_no_place_to_give() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let old = body(1, 3_000);
    sideload(&books, BOOK, &old);
    // One word source across both scans, so the two mints are two ids.
    let mut random = words();
    let (_, ids) = scan_minting(
        &root,
        &[(BookRoot::Library, BOOK, old.len() as u32)],
        &mut random,
    )
    .unwrap();
    let first = ids[0].unwrap();

    // The card goes into a computer, which puts another book at that name.
    let other = body(2, 4_100);
    overwrite_shelf(&root, BOOK, &other);
    let (assigned, ids) = scan_minting(
        &root,
        &[(BookRoot::Library, BOOK, other.len() as u32)],
        &mut random,
    )
    .unwrap();
    assert_eq!(
        assigned.minted, 1,
        "an unexplained replacement is a new copy"
    );
    assert_eq!(assigned.missing, 1, "and the old record is carried a while");
    let second = ids[0].unwrap();
    assert_ne!(second, first);

    let live = ledger::open(&root).unwrap().unwrap();
    let new_copy = ledger::find_by_id(&root, &live, second).unwrap().unwrap();
    assert_eq!(new_copy.locator(), Some(BOOK), "the copy that is there");
    assert_eq!(new_copy.misses, 0);
    assert_eq!(new_copy.byte_size, other.len() as u32);

    let old_copy = ledger::find_by_id(&root, &live, first).unwrap().unwrap();
    assert_eq!(
        old_copy.place, None,
        "the name it knew is another copy's now"
    );
    assert_eq!(old_copy.misses, 1);
    assert_eq!(
        old_copy.byte_size,
        old.len() as u32,
        "and the record is still there to be matched by its bytes"
    );

    // The same holds however far the old record is from the new one in
    // ledger order, which is what decides nothing here.
    let third = body(3, 2_048);
    upload_minting(&root, &books, "Other.epub", &third, &mut random)
        .unwrap()
        .expect("lands");
    let live = ledger::open(&root).unwrap().unwrap();
    assert_eq!(
        ledger::find_by_id(&root, &live, first)
            .unwrap()
            .unwrap()
            .place,
        None
    );
}

/// Rename a book on the shelf, the way a computer does: the bytes stay put
/// and the name changes, so the scan sees a place it knew go missing and a
/// place it has not seen appear.
fn rename_on_shelf(root: &Dir<'_>, from: &str, to: &str) {
    let path = LibraryPath::parse(from).unwrap();
    library::with_book_at(root, BookRoot::Library, &path, |dir, alias| {
        let mut alias_text = heapless::String::<12>::new();
        use core::fmt::Write as _;
        write!(alias_text, "{}", alias).unwrap();
        dir.move_file_in_dir_lfn(alias_text.as_str(), dir, to)
            .expect("rename book");
    })
    .expect("shelf readable")
    .expect("the book is there to rename");
}

/// A book renamed on a computer is the same copy in a new place. The scan
/// finds the record that named the old place with no row, and the row with
/// no record, reads the file that appeared, and hands the copy its own id
/// back rather than adopting it as a stranger.
#[test]
fn a_renamed_copy_is_found_again_and_keeps_its_id() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    upload(&root, &books, BOOK, &bytes, || {})
        .unwrap()
        .expect("lands");
    let (id, _, source) = record_for(&root, BOOK).expect("adopted at install");
    assert!(
        digest_agrees(source, &bytes),
        "the install recorded its bytes"
    );
    let mut random = words();
    for _ in 0..4 {
        random();
    }
    let (assigned, ids) =
        scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();
    assert_eq!(assigned.matched, 1);
    assert_eq!(ids[0], Some(id));

    let renamed = "Dune (First Edition).epub";
    rename_on_shelf(&root, BOOK, renamed);
    let (assigned, ids) =
        scan_minting(&root, &[(BookRoot::Library, renamed, size)], &mut random).unwrap();
    assert_eq!(
        assigned.repaired, 1,
        "the copy was found again: {assigned:?}"
    );
    assert_eq!(assigned.hashed, 1, "one file read to prove it");
    assert_eq!(assigned.minted, 0, "and nothing adopted as a stranger");
    assert_eq!(assigned.missing, 0);
    assert_eq!(ids[0], Some(id), "the row carries the copy's own id");

    assert_eq!(record_count(&root), 1, "one copy, one record");
    let live = ledger::open(&root).unwrap().unwrap();
    let copy = ledger::find_by_id(&root, &live, id).unwrap().unwrap();
    assert_eq!(
        copy.locator(),
        Some(renamed),
        "the record followed the file"
    );
    assert_eq!(copy.misses, 0);
    assert!(
        digest_agrees(copy.source, &bytes),
        "and still says what its bytes are"
    );

    // The next scan has nothing to look for, so it reads no book at all.
    let (assigned, ids) =
        scan_minting(&root, &[(BookRoot::Library, renamed, size)], &mut random).unwrap();
    assert_eq!(
        assigned.hashed, 0,
        "a shelf that did not change reads nothing"
    );
    assert_eq!(assigned.matched, 1);
    assert_eq!(ids[0], Some(id));
}

/// Two copies of one book, both renamed at once, are two copies no file can
/// tell apart: whichever of them a file holds, its bytes are the same. The
/// search leaves both alone rather than joining one reader's place to the
/// other's book, and the files are adopted as new copies.
#[test]
fn two_missing_copies_of_one_book_are_left_alone() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let shared = body(1, 3_000);
    let size = shared.len() as u32;
    let twin = "Dune (2).epub";
    let mut random = words();
    upload_minting(&root, &books, BOOK, &shared, &mut random)
        .unwrap()
        .expect("lands");
    upload_minting(&root, &books, twin, &shared, &mut random)
        .unwrap()
        .expect("lands");
    let (first, _, _) = record_for(&root, BOOK).expect("adopted");
    let (second, _, _) = record_for(&root, twin).expect("adopted");
    assert_ne!(first, second);
    scan_minting(
        &root,
        &[
            (BookRoot::Library, BOOK, size),
            (BookRoot::Library, twin, size),
        ],
        &mut random,
    )
    .unwrap();

    rename_on_shelf(&root, BOOK, "One.epub");
    rename_on_shelf(&root, twin, "Two.epub");
    let (assigned, ids) = scan_minting(
        &root,
        &[
            (BookRoot::Library, "One.epub", size),
            (BookRoot::Library, "Two.epub", size),
        ],
        &mut random,
    )
    .unwrap();
    assert_eq!(assigned.repaired, 0, "neither copy is chosen: {assigned:?}");
    assert_eq!(assigned.ambiguous, 2, "and both are reported");
    assert_eq!(assigned.hashed, 0, "nothing is read to learn what is known");
    assert_eq!(
        assigned.minted, 2,
        "the files are copies in their own right"
    );
    assert_eq!(assigned.missing, 2, "and the old records wait a while");
    let fresh: Vec<BookId> = ids.iter().map(|id| id.expect("adopted")).collect();
    assert!(!fresh.contains(&first));
    assert!(!fresh.contains(&second));
    assert_eq!(record_count(&root), 4);
}

/// One copy that went missing and two files holding its bytes is the same
/// ambiguity from the other side: which of them is the copy the reader was
/// in cannot be told from the bytes, so neither takes its id.
#[test]
fn a_copy_that_could_be_either_of_two_files_is_left_alone() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    let mut random = words();
    upload_minting(&root, &books, BOOK, &bytes, &mut random)
        .unwrap()
        .expect("lands");
    let (id, _, _) = record_for(&root, BOOK).expect("adopted");
    scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();

    // The reader copies the book beside itself on a computer and renames
    // the original, so two files now hold what one record knows.
    rename_on_shelf(&root, BOOK, "One.epub");
    sideload(&books, "Two.epub", &bytes);
    let (assigned, ids) = scan_minting(
        &root,
        &[
            (BookRoot::Library, "One.epub", size),
            (BookRoot::Library, "Two.epub", size),
        ],
        &mut random,
    )
    .unwrap();
    assert_eq!(assigned.repaired, 0, "neither file takes it: {assigned:?}");
    assert_eq!(assigned.ambiguous, 1);
    assert_eq!(assigned.hashed, 2, "both were read before that was known");
    assert_eq!(assigned.minted, 2);
    assert_eq!(assigned.missing, 1, "the copy waits, still missing");
    assert!(!ids.contains(&Some(id)));
    let live = ledger::open(&root).unwrap().unwrap();
    let copy = ledger::find_by_id(&root, &live, id).unwrap().unwrap();
    assert_eq!(
        copy.locator(),
        Some(BOOK),
        "it still answers with the place it left, which nothing else holds"
    );
    assert_eq!(copy.misses, 1);
}

/// A copy nothing recorded the bytes of cannot be found again: a name and a
/// length are not a book, and the file that was there is gone. It is left
/// missing and the file that appeared is adopted in its own right.
#[test]
fn a_copy_with_no_recorded_bytes_is_not_matched() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    sideload(&books, BOOK, &bytes);
    let mut random = words();
    let (_, ids) = scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();
    let id = ids[0].unwrap();
    assert_eq!(
        record_for(&root, BOOK).and_then(|(_, _, source)| source),
        None,
        "a scan adopts a book without reading it"
    );

    rename_on_shelf(&root, BOOK, "Elsewhere.epub");
    let (assigned, ids) = scan_minting(
        &root,
        &[(BookRoot::Library, "Elsewhere.epub", size)],
        &mut random,
    )
    .unwrap();
    assert_eq!(assigned.repaired, 0);
    assert_eq!(assigned.hashed, 0, "there was nothing to compare against");
    assert_eq!(assigned.minted, 1);
    assert_eq!(assigned.missing, 1);
    assert_ne!(ids[0], Some(id));
}

/// Record what the reader records when it opens a book: what the bytes it
/// read were, in the claim on the directory that book's place names.
fn note_open(root: &Dir<'_>, at: BookRoot, locator: &str, bytes: &[u8]) {
    if root.open_dir(CACHE_ROOT_DIR).is_err() {
        root.make_dir_in_dir(CACHE_ROOT_DIR).expect("make READER");
    }
    let cache_root = root.open_dir(CACHE_ROOT_DIR).expect("open READER");
    if cache_root.open_dir(proto::cache::CACHE_V2_DIR).is_err() {
        cache_root
            .make_dir_in_dir(proto::cache::CACHE_V2_DIR)
            .expect("make CACHE2");
    }
    let cache = cache_root
        .open_dir(proto::cache::CACHE_V2_DIR)
        .expect("open CACHE2");
    let key = cache_key_from(source_hash_at(at, locator, bytes.len() as u32));
    if cache.open_dir(key.as_str()).is_err() {
        cache
            .make_dir_in_dir(key.as_str())
            .expect("make book directory");
    }
    let book = cache.open_dir(key.as_str()).expect("open book directory");
    let evidence = proto::cache::CacheEvidence {
        cluster: None,
        digest: Some(CachedSourceDigest::new(digest_of(bytes))),
    };
    let mut encoded = [0u8; proto::cache::CACHE_CLAIM_MAX_BYTES];
    let len = proto::cache::encode_cache_claim(at, locator, false, &evidence, &mut encoded)
        .expect("the claim fits");
    let file = book
        .open_file_in_dir(
            proto::cache::CACHE_CLAIM_FILE,
            Mode::ReadWriteCreateOrTruncate,
        )
        .expect("claim file");
    file.write(&encoded[..len]).expect("write claim");
    file.close().expect("close claim");
}

/// A scan adopts a book without reading it, so most of a library has no
/// digest in the ledger. Opening one records what its bytes were beside the
/// place it was read from, and that is enough to find it again: the copy
/// that moved is the one the reader had been reading, which is the copy
/// whose place is worth keeping.
#[test]
fn a_sideloaded_copy_that_was_read_is_found_again_where_it_went() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    sideload(&books, BOOK, &bytes);
    let mut random = words();
    let (_, ids) = scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();
    let id = ids[0].unwrap();
    assert_eq!(
        record_for(&root, BOOK).and_then(|(_, _, source)| source),
        None,
        "the ledger was told nothing about the bytes"
    );

    // The reader opens it, which is what records them.
    note_open(&root, BookRoot::Library, BOOK, &bytes);
    let _ = found_again();

    let moved = "Herbert, Frank - Dune.epub";
    rename_on_shelf(&root, BOOK, moved);
    let (assigned, ids) =
        scan_minting(&root, &[(BookRoot::Library, moved, size)], &mut random).unwrap();
    assert_eq!(assigned.repaired, 1, "found again: {assigned:?}");
    assert_eq!(assigned.hashed, 1);
    assert_eq!(assigned.minted, 0);
    assert_eq!(ids[0], Some(id), "under the id it was adopted with");

    // And the move is reported with both places, so what is filed under the
    // old one can be carried to the new one.
    assert_eq!(
        found_again(),
        vec![(id, BOOK.to_owned(), moved.to_owned())],
        "the scan says which copy went where"
    );

    let live = ledger::open(&root).unwrap().unwrap();
    let copy = ledger::find_by_id(&root, &live, id).unwrap().unwrap();
    assert_eq!(copy.locator(), Some(moved));
    assert!(
        digest_agrees(copy.source, &bytes),
        "and the record now says what its bytes are, having read them"
    );
}

/// A directory another book claims says nothing about this one, however
/// well its digest fits: cache keys are 28 bits of a hash of the place, so
/// two books can land on one directory, and the claim is what tells them
/// apart.
#[test]
fn a_claim_naming_another_book_is_no_evidence_about_this_one() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    sideload(&books, BOOK, &bytes);
    let mut random = words();
    let (_, ids) = scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();
    let id = ids[0].unwrap();

    // The claim on this book's directory names somebody else, so what it
    // records of its bytes is somebody else's business.
    note_open(&root, BookRoot::Library, BOOK, &bytes);
    let key = cache_key_from(source_hash_at(BookRoot::Library, BOOK, size));
    let cache_root = root.open_dir(CACHE_ROOT_DIR).expect("open READER");
    let cache = cache_root
        .open_dir(proto::cache::CACHE_V2_DIR)
        .expect("open CACHE2");
    let book = cache.open_dir(key.as_str()).expect("open book directory");
    let evidence = proto::cache::CacheEvidence {
        cluster: None,
        digest: Some(CachedSourceDigest::new(digest_of(&bytes))),
    };
    let mut encoded = [0u8; proto::cache::CACHE_CLAIM_MAX_BYTES];
    let len = proto::cache::encode_cache_claim(
        BookRoot::Library,
        "Someone Else.epub",
        false,
        &evidence,
        &mut encoded,
    )
    .expect("the claim fits");
    let file = book
        .open_file_in_dir(
            proto::cache::CACHE_CLAIM_FILE,
            Mode::ReadWriteCreateOrTruncate,
        )
        .expect("claim file");
    file.write(&encoded[..len]).expect("write claim");
    file.close().expect("close claim");
    let _ = found_again();

    rename_on_shelf(&root, BOOK, "Elsewhere.epub");
    let (assigned, ids) = scan_minting(
        &root,
        &[(BookRoot::Library, "Elsewhere.epub", size)],
        &mut random,
    )
    .unwrap();
    assert_eq!(assigned.repaired, 0, "no evidence, no repair: {assigned:?}");
    assert_eq!(assigned.hashed, 0);
    assert_eq!(assigned.minted, 1);
    assert_ne!(ids[0], Some(id));
    assert!(found_again().is_empty());
}

/// One scan carries as many missing copies as its arena holds, and the
/// bound is on which copies it may repair rather than on what it knows: a
/// second copy of the same bytes past the end of the table still says the
/// two cannot be told apart.
#[test]
fn a_twin_past_the_end_of_the_table_still_refuses_the_repair() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let shared = body(1, 3_000);
    let size = shared.len() as u32;
    // Sixty-five missing copies, all of one length so all are candidates,
    // the first and the last holding the same bytes.
    let ids: Vec<BookId> = (0..65u32)
        .map(|index| {
            let mut bytes = [0u8; 16];
            bytes[..4].copy_from_slice(&(index + 1).to_le_bytes());
            BookId::from_bytes(bytes).unwrap()
        })
        .collect();
    ledger::write_generation(
        &root,
        None,
        &mut |_, record| Carry::Keep(Kept::of(record)),
        |writer| {
            for (index, id) in ids.iter().enumerate() {
                let mut locator = heapless::String::<32>::new();
                use core::fmt::Write as _;
                write!(locator, "Gone{index}.epub").unwrap();
                let bytes = if index == 0 || index == 64 {
                    shared.clone()
                } else {
                    body(index as u8 + 40, size as usize)
                };
                writer.append(&LedgerRecord {
                    id: *id,
                    root: BookRoot::Library,
                    locator: locator.as_str(),
                    byte_size: size,
                    misses: 1,
                    source: Some(CachedSourceDigest::new(digest_of(&bytes))),
                })?;
            }
            Ok(())
        },
    )
    .unwrap();

    // The bytes those two copies held turn up under a name nobody knows.
    sideload(&books, "Found.epub", &shared);
    let mut random = words();
    let (assigned, ids_seen) = scan_minting(
        &root,
        &[(BookRoot::Library, "Found.epub", size)],
        &mut random,
    )
    .unwrap();
    assert_eq!(
        assigned.repaired, 0,
        "either copy could be this file: {assigned:?}"
    );
    assert!(assigned.ambiguous >= 1, "and it is reported: {assigned:?}");
    assert_eq!(assigned.minted, 1, "the file is a copy in its own right");
    assert!(!ids_seen.contains(&Some(ids[0])));
    assert!(!ids_seen.contains(&Some(ids[64])));
}

/// A copy is found again however many files of its length the card holds:
/// the search reads every one of them, so one match means one match. There
/// is no reading budget to run out of, and so nothing to carry to another
/// scan.
#[test]
fn a_copy_is_found_whatever_else_shares_its_length() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    let mut random = words();
    upload_minting(&root, &books, BOOK, &bytes, &mut random)
        .unwrap()
        .expect("lands");
    let (id, _, _) = record_for(&root, BOOK).expect("adopted");
    scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();

    // Twenty strangers of the copy's length arrive ahead of it, which under
    // a sixteen-file budget would have hidden it.
    let mut names = Vec::new();
    for stranger in 0..20u8 {
        let name = std::format!("Stranger{stranger:02}.epub");
        sideload(&books, &name, &body(stranger + 40, size as usize));
        names.push(name);
    }
    rename_on_shelf(&root, BOOK, "Moved.epub");
    names.push(std::string::String::from("Moved.epub"));
    let rows: Vec<Row<'_>> = names
        .iter()
        .map(|name| (BookRoot::Library, name.as_str(), size))
        .collect();

    let (assigned, ids) = scan_minting(&root, &rows, &mut random).unwrap();
    assert_eq!(assigned.repaired, 1, "found in one scan: {assigned:?}");
    assert_eq!(assigned.hashed, 21, "having read every file of its length");
    assert_eq!(assigned.minted, 20, "the strangers are their own copies");
    assert_eq!(ids[20], Some(id));
    assert_eq!(
        found_again(),
        vec![(id, BOOK.to_owned(), "Moved.epub".to_owned())],
    );
}

/// Two files holding one copy's bytes are two files no copy can be told
/// apart by, whatever else the card holds. Both are adopted in their own
/// right, the copy stays missing, and nothing is reported: a reading place
/// moved onto either would be moved onto a guess.
#[test]
fn two_files_holding_a_copys_bytes_are_both_adopted() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    let mut random = words();
    upload_minting(&root, &books, BOOK, &bytes, &mut random)
        .unwrap()
        .expect("lands");
    let (id, _, _) = record_for(&root, BOOK).expect("adopted");
    scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();

    rename_on_shelf(&root, BOOK, "One.epub");
    sideload(&books, "Two.epub", &bytes);
    let rows = [
        (BookRoot::Library, "One.epub", size),
        (BookRoot::Library, "Two.epub", size),
    ];
    let (assigned, ids) = scan_minting(&root, &rows, &mut random).unwrap();
    assert_eq!(assigned.repaired, 0, "either could be it: {assigned:?}");
    assert!(assigned.ambiguous >= 1);
    assert_eq!(assigned.minted, 2);
    assert!(!ids.contains(&Some(id)));
    assert!(found_again().is_empty(), "and nothing moved on a guess");
}

/// A file the card would not give up could hold any copy's bytes, so the
/// length it claims stops being decidable: every copy that size is left
/// alone this scan, and the files are adopted in their own right. A card
/// that refuses a read costs a copy its continuity, as a card that refuses
/// a read costs anything else that depended on it.
#[test]
fn a_file_the_card_would_not_read_leaves_its_length_alone() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    let mut random = words();
    upload_minting(&root, &books, BOOK, &bytes, &mut random)
        .unwrap()
        .expect("lands");
    let (id, _, _) = record_for(&root, BOOK).expect("adopted");
    scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();

    // The listing names a file the card cannot produce, of the copy's
    // length, beside the file the copy actually moved to.
    rename_on_shelf(&root, BOOK, "Moved.epub");
    let rows = [
        (BookRoot::Library, "Unread.epub", size),
        (BookRoot::Library, "Moved.epub", size),
    ];
    let (assigned, ids) = scan_minting(&root, &rows, &mut random).unwrap();
    assert_eq!(
        assigned.repaired, 0,
        "the length is undecidable: {assigned:?}"
    );
    assert_eq!(assigned.unreadable, 1, "and the reason is said plainly");
    assert!(!ids.contains(&Some(id)));
    assert!(found_again().is_empty());
    let live = ledger::open(&root).unwrap().unwrap();
    assert_eq!(
        ledger::find_by_id(&root, &live, id)
            .unwrap()
            .unwrap()
            .misses,
        1,
        "the copy is missing, as it is"
    );
}

/// What a copy's bytes were is learned from the claim beside its reading
/// place, and the sweep that tidies a departed book's directory takes that
/// claim away. So the ledger takes it first, on the scan that misses the
/// copy, whether or not anything turned up to compare it with. Otherwise
/// the ordinary two-stage card edit, take the book away now and put the
/// replacement in later, would arrive with nothing to look with.
#[test]
fn a_copy_that_goes_missing_keeps_what_its_claim_said() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let bytes = body(1, 3_000);
    let size = bytes.len() as u32;
    sideload(&books, BOOK, &bytes);
    let mut random = words();
    let (_, ids) = scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();
    let id = ids[0].unwrap();
    note_open(&root, BookRoot::Library, BOOK, &bytes);
    assert_eq!(
        record_for(&root, BOOK).and_then(|(_, _, source)| source),
        None,
        "the ledger itself was told nothing"
    );

    // The book goes away, and the scan that notices has nothing to compare
    // it with: no file arrived in its place.
    remove_from_shelf(&root, BOOK);
    let (assigned, _) = scan_minting(&root, &[], &mut random).unwrap();
    assert_eq!(assigned.missing, 1);
    assert_eq!(assigned.hashed, 0, "and nothing was read to learn it");
    let live = ledger::open(&root).unwrap().unwrap();
    let copy = ledger::find_by_id(&root, &live, id).unwrap().unwrap();
    assert!(
        digest_agrees(copy.source, &bytes),
        "the record says what its bytes were"
    );

    // So the sweep may take the claim, and the replacement arriving later
    // is still found.
    sweep_claim(&root, BookRoot::Library, BOOK, size);
    sideload(&books, "Elsewhere.epub", &bytes);
    let (assigned, ids) = scan_minting(
        &root,
        &[(BookRoot::Library, "Elsewhere.epub", size)],
        &mut random,
    )
    .unwrap();
    assert_eq!(assigned.repaired, 1, "found again: {assigned:?}");
    assert_eq!(ids[0], Some(id));
}

/// The same, for two copies of one book where only one keeps a reading
/// place. The sweep takes the other's claim, and if the ledger had not
/// taken what it said first, the copy that kept its claim would look like
/// the only one those bytes could belong to. It is not, and the file that
/// comes back is adopted in its own right.
#[test]
fn a_twin_whose_cache_was_swept_still_refuses_the_repair() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let shared = body(1, 3_000);
    let size = shared.len() as u32;
    let twin = "Dune (2).epub";
    sideload(&books, BOOK, &shared);
    sideload(&books, twin, &shared);
    let mut random = words();
    let (_, ids) = scan_minting(
        &root,
        &[
            (BookRoot::Library, BOOK, size),
            (BookRoot::Library, twin, size),
        ],
        &mut random,
    )
    .unwrap();
    let (first, second) = (ids[0].unwrap(), ids[1].unwrap());
    note_open(&root, BookRoot::Library, BOOK, &shared);
    note_open(&root, BookRoot::Library, twin, &shared);

    // Both go away, and the scan that notices takes what their claims said.
    remove_from_shelf(&root, BOOK);
    remove_from_shelf(&root, twin);
    scan_minting(&root, &[], &mut random).unwrap();
    let live = ledger::open(&root).unwrap().unwrap();
    for id in [first, second] {
        let copy = ledger::find_by_id(&root, &live, id).unwrap().unwrap();
        assert!(digest_agrees(copy.source, &shared), "both records say");
    }

    // The sweep takes both claims, and one of the two files comes back.
    sweep_claim(&root, BookRoot::Library, BOOK, size);
    sweep_claim(&root, BookRoot::Library, twin, size);
    sideload(&books, "Returned.epub", &shared);
    let (assigned, ids) = scan_minting(
        &root,
        &[(BookRoot::Library, "Returned.epub", size)],
        &mut random,
    )
    .unwrap();
    assert_eq!(
        assigned.repaired, 0,
        "either copy could be this file: {assigned:?}"
    );
    assert!(assigned.ambiguous >= 1);
    assert_eq!(assigned.minted, 1);
    assert!(!ids.contains(&Some(first)));
    assert!(!ids.contains(&Some(second)));
    assert!(found_again().is_empty());
}

/// Take a claim away, as the cache sweep does once a departed book's
/// directory has nothing left to keep.
fn sweep_claim(root: &Dir<'_>, at: BookRoot, locator: &str, byte_size: u32) {
    let key = cache_key_from(source_hash_at(at, locator, byte_size));
    let cache_root = root.open_dir(CACHE_ROOT_DIR).expect("open READER");
    let cache = cache_root
        .open_dir(proto::cache::CACHE_V2_DIR)
        .expect("open CACHE2");
    let book = cache.open_dir(key.as_str()).expect("open book directory");
    book.delete_entry_in_dir(proto::cache::CACHE_CLAIM_FILE)
        .expect("sweep the claim away");
}

/// What a copy is, for a book the library adopted without reading it, is
/// the bytes seen at its own place while that place looked unchanged.
///
/// A computer can put a different book of exactly the same length at that
/// name, which the scan's cheap filter cannot see and no later reading can
/// undo: nothing on the card ever said what the first book's bytes were.
/// So the copy takes the bytes that were read there, and a move carries its
/// id and its reading place to wherever those bytes go. The alternative is
/// reading every book as the scan adopts it, which is hours on a full card
/// for a move that may never happen.
///
/// What this costs is bounded by what the cache already does: a same-sized
/// replacement at a stable name reopens the old book's cache and resumes
/// its place today, before any of this. The rule makes that durable across
/// a later rename rather than inventing it.
#[test]
fn a_copy_nobody_read_takes_the_bytes_seen_at_its_own_place() {
    let disk = new_card();
    let mgr = open_mgr(&disk);
    let (root, books) = open_dirs(&mgr);
    let first = body(1, 3_000);
    let size = first.len() as u32;
    sideload(&books, BOOK, &first);
    let mut random = words();
    let (_, ids) = scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).unwrap();
    let id = ids[0].unwrap();
    assert_eq!(
        record_for(&root, BOOK).and_then(|(_, _, source)| source),
        None,
        "adopted without reading it"
    );

    // A computer puts another book of the same length at that name. The
    // scan matches by root, locator and size, so the copy keeps its id.
    let second = body(2, 3_000);
    assert_eq!(second.len() as u32, size);
    overwrite_shelf(&root, BOOK, &second);
    let (assigned, ids) =
        scan_minting(&root, &[(BookRoot::Library, BOOK, size)], &mut random).expect("scan");
    assert_eq!(assigned.matched, 1, "the cheap filter cannot see the swap");
    assert_eq!(ids[0], Some(id));

    // The reader opens what is there, so what is there becomes what this
    // copy is.
    note_open(&root, BookRoot::Library, BOOK, &second);

    // And a rename carries the copy, its id and its place, to the bytes it
    // is now known by.
    rename_on_shelf(&root, BOOK, "Elsewhere.epub");
    let (assigned, ids) = scan_minting(
        &root,
        &[(BookRoot::Library, "Elsewhere.epub", size)],
        &mut random,
    )
    .unwrap();
    assert_eq!(assigned.repaired, 1, "found again: {assigned:?}");
    assert_eq!(ids[0], Some(id));
    let live = ledger::open(&root).unwrap().unwrap();
    let copy = ledger::find_by_id(&root, &live, id).unwrap().unwrap();
    assert_eq!(copy.locator(), Some("Elsewhere.epub"));
    assert!(
        digest_agrees(copy.source, &second),
        "under the bytes it was read as"
    );
    assert_eq!(
        found_again(),
        vec![(id, BOOK.to_owned(), "Elsewhere.epub".to_owned())],
    );
}
