//! The install sequence against a real FAT filesystem.
//!
//! The planner is tested on its own in `install_plan.rs`; what is checked
//! here is that each step does to the card what the planner believes it
//! does, and that stopping anywhere in the sequence — as a power cut would —
//! still leaves exactly one complete book on the shelf under the right name.

use core::ops::ControlFlow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use embedded_sdmmc::{
    Block, BlockCount, BlockDevice, BlockIdx, Directory, LfnBuffer, Mode, TimeSource, Timestamp,
    VolumeIdx, VolumeManager,
};
use heapless::String;
use proto::library_path::BookRoot;
use proto::source::FileKey;
use upload_store::install::{
    self, recover_installs, InstallIntent, Landed, Located, ShortName, Step, ROLLBACK_DIR,
    UPLOAD_DIR,
};
use upload_store::reclaim;

const BLOCK_BYTES: usize = 512;
const DISK_BLOCKS: u32 = 32 * 1024;
const PART_START_BLOCK: u32 = 64;

struct RamDisk {
    data: RefCell<Vec<u8>>,
    /// Writes from this number onward do nothing and report failure.
    ///
    /// The power cut: everything before it reached the card and everything
    /// after it did not, which is the one thing a reset guarantees and the
    /// only thing a test may assume.
    fail_writes_from: RefCell<Option<u32>>,
    writes_seen: RefCell<u32>,
}

#[derive(Debug)]
struct DiskError;

impl core::fmt::Display for DiskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "injected disk error")
    }
}

impl std::error::Error for DiskError {}

/// Shared so a test can look at the image after handing the card to a volume
/// manager, and put back what a half-finished move took away.
#[derive(Clone)]
struct SharedDisk(Rc<RamDisk>);

impl SharedDisk {
    fn image(&self) -> Vec<u8> {
        self.0.data.borrow().clone()
    }

    /// Cut the power before the `n`th write from now on.
    fn cut_writes_from(&self, n: Option<u32>) {
        *self.0.writes_seen.borrow_mut() = 0;
        *self.0.fail_writes_from.borrow_mut() = n;
    }

    fn writes_seen(&self) -> u32 {
        *self.0.writes_seen.borrow()
    }

    /// The clusters among `wanted` whose FAT16 entry is not free.
    ///
    /// Per cluster rather than by counting free space: the reclaim journal
    /// allocates while it works, so an aggregate can stay level while a
    /// batch leaks.
    fn still_allocated(&self, wanted: &[u32]) -> Vec<u32> {
        let image = self.image();
        let boot = PART_START_BLOCK as usize * BLOCK_BYTES;
        let reserved = u16::from_le_bytes([image[boot + 14], image[boot + 15]]) as usize;
        let fat_start = (PART_START_BLOCK as usize + reserved) * BLOCK_BYTES;
        wanted
            .iter()
            .copied()
            .filter(|cluster| {
                let at = fat_start + *cluster as usize * 2;
                // Otherwise a cluster number from outside the volume is an
                // index panic several frames from anything that names it.
                assert!(
                    at + 2 <= image.len(),
                    "cluster {cluster} puts its FAT entry at byte {at}, past the \
                     end of a {} byte image",
                    image.len(),
                );
                u16::from_le_bytes([image[at], image[at + 1]]) != 0
            })
            .collect()
    }

    /// Put back the directory entries a move marked deleted, leaving the
    /// state a power cut between its two writes would: the destination
    /// written, the source not yet taken away, one chain under both names.
    ///
    /// Only entries whose sole difference is the deletion mark are restored,
    /// so this undoes an unlink and nothing else.
    fn undo_unlinks(&self, before: &[u8]) {
        let mut data = self.0.data.borrow_mut();
        let mut restored = 0;
        for at in (0..data.len().min(before.len())).step_by(32) {
            let live_before = before[at] != 0xE5 && before[at] != 0x00;
            if data[at] == 0xE5 && live_before && data[at + 1..at + 32] == before[at + 1..at + 32] {
                data[at] = before[at];
                restored += 1;
            }
        }
        assert!(
            restored > 0,
            "no unlink to undo; the fixture proves nothing"
        );
    }
}

impl BlockDevice for SharedDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        self.0.read(blocks, start)
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        self.0.write(blocks, start)
    }

    fn num_blocks(&self) -> Result<BlockCount, DiskError> {
        self.0.num_blocks()
    }
}

impl BlockDevice for RamDisk {
    type Error = DiskError;

    fn read(&self, blocks: &mut [Block], start: BlockIdx) -> Result<(), DiskError> {
        let data = self.data.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let at = (start.0 as usize + i) * BLOCK_BYTES;
            block.copy_from_slice(&data[at..at + BLOCK_BYTES]);
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start: BlockIdx) -> Result<(), DiskError> {
        {
            let mut seen = self.writes_seen.borrow_mut();
            *seen += 1;
            if let Some(from) = *self.fail_writes_from.borrow() {
                if *seen >= from {
                    return Err(DiskError);
                }
            }
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
    }))
}

fn open_mgr(disk: SharedDisk) -> Mgr {
    VolumeManager::new_with_limits(disk, StaticTime, 5000)
}

fn open_dirs(mgr: &Mgr) -> (Dir<'_>, Dir<'_>) {
    let volume = mgr.open_volume(VolumeIdx(0)).expect("volume");
    let raw_volume = volume.to_raw_volume();
    let raw_root = mgr.open_root_dir(raw_volume).expect("root");
    let root = Directory::new(raw_root, mgr);
    if root.open_dir("BOOKS").is_err() {
        root.make_dir_in_dir("BOOKS").expect("make BOOKS");
    }
    let raw_root = mgr.open_root_dir(raw_volume).expect("reopen root");
    let raw_books = mgr.open_dir(raw_root, "BOOKS").expect("open BOOKS");
    mgr.close_dir(raw_root).expect("close spare root");
    (root, Directory::new(raw_books, mgr))
}

fn short(text: &str) -> String<12> {
    let mut name = String::new();
    name.push_str(text).expect("fits");
    name
}

fn long(text: &str) -> String<64> {
    let mut name = String::new();
    name.push_str(text).expect("fits");
    name
}

const BOOK_NAME: &str = "Middlemarch.epub";

/// Big enough to span several clusters. A single-cluster book hides the
/// mistake this suite exists to catch: freeing its chain still leaves the
/// first cluster readable, so a reclaim where an unlink was needed looks
/// harmless until the day the book is longer than one cluster.
const BODY_BYTES: usize = 40 * 1024;

fn old_body() -> Vec<u8> {
    (0..BODY_BYTES).map(|i| (i % 251) as u8).collect()
}

fn new_body() -> Vec<u8> {
    (0..BODY_BYTES + 512).map(|i| (i % 241) as u8).collect()
}

fn intent(replacing: bool) -> InstallIntent {
    InstallIntent {
        // Chains are filled in by `prepare` once the files exist: they belong
        // to the card, so a fixture cannot name them in advance.
        stage: Located {
            alias: short("TXN00001.TMP"),
            chain: 0,
        },
        long_name: long(BOOK_NAME),
        old: replacing.then(|| Located {
            alias: short(""),
            chain: 0,
        }),
        rollback: short("TXN00001.OLD"),
    }
}

/// The 8.3 alias and chain of the entry holding `long_name` — what a record
/// names its predecessor by.
fn holder_of(books: &Dir<'_>, long_name: &str) -> Option<Located> {
    let mut storage = [0u8; 256];
    let mut lfn = LfnBuffer::new(&mut storage);
    let mut found = None;
    books
        .iterate_dir_lfn(&mut lfn, |entry, name| {
            if name == Some(long_name) {
                found = Some(Located {
                    alias: short(&entry.name.to_string()),
                    chain: entry.cluster.value(),
                });
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })
        .expect("iterate");
    found
}

/// The 8.3 alias the driver gave the entry holding `long_name`.
fn alias_of(books: &Dir<'_>, long_name: &str) -> Option<String<12>> {
    holder_of(books, long_name).map(|held| held.alias)
}

/// The book on the shelf under `long_name`.
///
/// Resolved to the alias first and opened by that, which is how the reader
/// opens a book: the catalog stores the 8.3 name, and nothing outside these
/// tests opens by long name. It also avoids `open_long_name_file_in_dir`,
/// which the driver can fail to resolve for an entry its own iterator lists —
/// see the note in `a_stranger_on_the_recycled_cluster_does_not_get_the_predecessor_freed`.
fn body_named(books: &Dir<'_>, long_name: &str) -> Vec<u8> {
    let alias = alias_of(books, long_name).expect("a book under that long name");
    body(books, alias.as_str())
}

/// Lay down the starting state: a finished upload waiting in the scratch
/// directory, and optionally the book it replaces already on the shelf.
fn prepare(root: &Dir<'_>, books: &Dir<'_>, intent: &mut InstallIntent) {
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .unwrap_or_else(|_| {
            root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("make cache root");
            root.open_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("open cache root")
        });
    for name in [UPLOAD_DIR, ROLLBACK_DIR] {
        if cache_root.open_dir(name).is_err() {
            cache_root.make_dir_in_dir(name).expect("make subdir");
        }
    }
    let upload = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");

    let staged = upload
        .open_file_in_dir(intent.stage.alias.as_str(), Mode::ReadWriteCreate)
        .expect("create scratch file");
    staged.write(&new_body()).expect("stream");
    staged.close().expect("close");
    // After the close: an open file's entry still describes what it was.
    intent.stage.chain = upload
        .find_directory_entry(intent.stage.alias.as_str())
        .expect("the scratch file")
        .cluster
        .value();

    if intent.old.is_some() {
        let existing = books
            .create_file_in_dir_lfn(BOOK_NAME)
            .expect("existing book");
        existing.write(&old_body()).expect("write");
        existing.close().expect("close");
        intent.old = Some(holder_of(books, BOOK_NAME).expect("predecessor alias and chain"));
    }

    install::write_intent(root, intent).expect("journal the intent");
}

fn body(books: &Dir<'_>, name: &str) -> Vec<u8> {
    let file = books
        .open_file_in_dir(name, Mode::ReadOnly)
        .expect("open book");
    let mut out = vec![0u8; file.length() as usize];
    file.read(&mut out).expect("read");
    out
}

/// Every long name in `/BOOKS`, so a duplicate cannot hide behind an alias.
fn shelf_long_names(books: &Dir<'_>) -> Vec<std::string::String> {
    let mut storage = [0u8; 256];
    let mut lfn = LfnBuffer::new(&mut storage);
    let mut names = Vec::new();
    books
        .iterate_dir_lfn(&mut lfn, |entry, long| {
            if entry.attributes.is_directory() || entry.attributes.is_volume() {
                return ControlFlow::Continue(());
            }
            names.push(match long {
                Some(text) if !text.is_empty() => text.to_string(),
                _ => entry.name.to_string(),
            });
            ControlFlow::Continue(())
        })
        .expect("iterate");
    names
}

/// Walk the install one step at a time, stopping after `stop_after` of them,
/// then hand what is left to recovery. Whatever the cut point, the shelf must
/// end up holding exactly one `Middlemarch.epub`, and it must be complete.
fn install_interrupted_after(steps_taken: usize, replacing: bool) {
    let disk = new_card();
    let mut intent = intent(replacing);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    let mut applied = 0;
    while applied < steps_taken {
        let presence = install::observe(&root, &books, &intent).expect("observe");
        let step = install::plan(&intent, presence);
        if step == Step::Done {
            break;
        }
        install::apply_step(&root, &books, &intent, step).expect("step");
        applied += 1;
    }

    let outcome = recover_installs(&root, &books);
    assert!(
        outcome.complete,
        "recovery after {steps_taken} step(s) did not finish"
    );

    let names = shelf_long_names(&books);
    let holders = names.iter().filter(|n| n.as_str() == BOOK_NAME).count();
    assert_eq!(
        holders, 1,
        "after stopping at step {steps_taken}, the shelf held {holders} copies of {BOOK_NAME} ({names:?})"
    );

    // The surviving copy must be the complete new edition. Recovery drives
    // every cut point forward to the installed book; the case that ends in a
    // restored predecessor is a lost upload, tested separately.
    let installed = body_named(&books, BOOK_NAME);
    assert_eq!(
        installed,
        new_body(),
        "the surviving book was truncated or is the wrong edition"
    );

    // And nothing is left behind to confuse the next mount.
    assert!(
        install::read_intent(&root).expect("read journal") == install::IntentState::Absent,
        "the journal must be clear once recovery completes"
    );
}

#[test]
fn a_first_install_survives_a_cut_at_every_step() {
    for stop_after in 0..=4 {
        install_interrupted_after(stop_after, false);
    }
}

#[test]
fn a_replacement_survives_a_cut_at_every_step() {
    for stop_after in 0..=6 {
        install_interrupted_after(stop_after, true);
    }
}

/// Retiring the predecessor must park it whole. The move is the driver's now
/// — it writes the new name and takes the old one away, unlinking the new one
/// again if it cannot — so what is checked here is that the book arrives in
/// rollback intact and that recovery carries on from there. The window where
/// one chain briefly has two names is only reachable through a power cut
/// inside that call, and the rule for it (unlink, never reclaim) is pinned by
/// the planner tests.
#[test]
fn a_retired_predecessor_is_parked_whole() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");

    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let rollback = cache_root.open_dir(ROLLBACK_DIR).expect("rollback dir");
    // Opened by the name it was parked under: the alias is the driver's.
    let parked = rollback
        .open_long_name_file_in_dir(intent.rollback.as_str(), Mode::ReadOnly)
        .expect("parked copy");
    assert_eq!(
        parked.length() as usize,
        BODY_BYTES,
        "the parked copy must describe the same data, not an empty file"
    );
    parked.close().expect("close");
    assert!(
        alias_of(&books, BOOK_NAME).is_none(),
        "the shelf gave the name up when the predecessor was parked"
    );

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete);
    assert!(outcome.touched_shelf, "the shelf did change");
    assert_eq!(body_named(&books, BOOK_NAME), new_body());
}

/// An upload with no bytes has no cluster, and a record naming it would name
/// every other empty file on the card equally well — starting with the book it
/// is replacing, which recovery would then read as this upload already
/// installed and unlink the scratch file over. Refused before anything is
/// written down.
#[test]
fn an_upload_with_no_bytes_is_refused_before_a_record_exists() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    // The dangerous shape: an empty upload over an empty book of the same
    // name, where neither has a chain to tell them apart.
    let empty = books
        .create_file_in_dir_lfn(BOOK_NAME)
        .expect("an empty book");
    empty.close().expect("close");

    for name in [BOOK_NAME, "Nothing At All.epub"] {
        let staged = StagedUpload::begin(&root, &books, name, None).expect("stage");
        assert!(
            matches!(
                staged.install(&root, &books, &mut words()),
                Err(install::InstallError::Empty)
            ),
            "an upload of {name} with no body must be refused, not journalled"
        );
        assert_eq!(
            install::read_intent(&root).expect("journal"),
            install::IntentState::Absent,
            "a record here would refuse every later upload"
        );
        assert!(
            scratch_files(&root).is_empty(),
            "and the scratch file goes with it"
        );
    }

    assert_eq!(
        shelf_long_names(&books),
        vec![BOOK_NAME.to_string()],
        "the shelf is exactly as it was: one empty book, nothing added"
    );
    assert_eq!(body_named(&books, BOOK_NAME).len(), 0);

    // The card is not wedged by the refusal.
    upload(&root, &books, "Something Real.epub", &new_body()).expect("a real upload still works");
    assert_eq!(shelf_long_names(&books).len(), 2);
}

/// A card managed from a computer can hold a zero-byte EPUB. Replacing one is
/// refused rather than guessed at: it starts at cluster 0, which every other
/// empty file also starts at, so a record naming it could not recognise it
/// again — and both steps that act on the predecessor take a book off the
/// shelf. Deleting it and retrying is the remedy, and one the device can do.
#[test]
fn a_replacement_for_an_empty_book_is_refused_rather_than_guessed() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let empty = books
        .create_file_in_dir_lfn(BOOK_NAME)
        .expect("an empty book");
    empty.close().expect("close");
    let alias = holder_of(&books, BOOK_NAME).expect("on the shelf");
    assert_eq!(alias.chain, 0, "the fixture has to actually be empty");

    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, None).expect("stage");
    staged.write(&new_body()).expect("stream");
    assert!(
        matches!(
            staged.install(&root, &books, &mut words()),
            Err(install::InstallError::Empty)
        ),
        "a predecessor with no chain must not be journalled"
    );
    assert_eq!(
        install::read_intent(&root).expect("journal"),
        install::IntentState::Absent,
        "and no record is left to refuse later uploads"
    );
    assert!(scratch_files(&root).is_empty());
    assert_eq!(body_named(&books, BOOK_NAME).len(), 0, "shelf untouched");

    // The remedy, which the device can carry out itself: delete the empty
    // book, then upload.
    assert_eq!(
        upload_store::remove_file_reclaiming_clusters(&books, alias.alias.as_str()),
        upload_store::RemoveStatus::Removed
    );
    upload(&root, &books, BOOK_NAME, &new_body()).expect("now it lands");
    assert_eq!(body_named(&books, BOOK_NAME), new_body());
    assert_eq!(shelf_long_names(&books), vec![BOOK_NAME.to_string()]);
}

/// The move taking the scratch name away is load bearing. While that name
/// stands the upload has not been installed; once it is gone it has. So a
/// person deleting the newly installed book from a computer — ordinary, and
/// the reason this branch gives books real filenames — reads as a lost
/// install, and their old book goes back.
///
/// Publishing by *link* would keep the name and lose that signal: the same
/// deletion would read as "not installed yet", the dangling entry would be
/// linked in again, and the predecessor freed as obsolete. Both copies gone,
/// from one ordinary act.
#[test]
fn a_book_deleted_from_a_computer_mid_transaction_gets_the_old_one_back() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let mut intent = intent(true);
    prepare(&root, &books, &mut intent);
    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");
    install::apply_step(&root, &books, &intent, Step::InstallStage).expect("install");

    // The card comes out and somebody deletes the book they can see, the
    // ordinary way, which frees its chain.
    let alias = alias_of(&books, BOOK_NAME).expect("the installed book");
    assert_eq!(
        upload_store::remove_file_reclaiming_clusters(&books, alias.as_str()),
        upload_store::RemoveStatus::Removed
    );

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete, "a lost install is a settled transaction");
    assert_eq!(
        body_named(&books, BOOK_NAME),
        old_body(),
        "the book the user had is the book the user gets back"
    );
    assert_eq!(shelf_long_names(&books), vec![BOOK_NAME.to_string()]);
    assert!(scratch_files(&root).is_empty());
}

/// The same shape with the chain left allocated: the scratch entry is merely
/// unlinked, so the stranger lands somewhere else and is foreign rather than
/// unprovable. Either way the parked predecessor survives.
#[test]
fn a_lost_stage_does_not_make_a_stranger_look_like_the_installed_book() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let mut intent = intent(true);
    prepare(&root, &books, &mut intent);
    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");

    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let upload_dir = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");
    upload_dir
        .delete_entry_in_dir(intent.stage.alias.as_str())
        .expect("lose the upload");

    let intruder = books
        .create_file_in_dir_lfn(BOOK_NAME)
        .expect("somebody else's book");
    intruder.write(b"not either of ours").expect("write");
    intruder.close().expect("close");

    let outcome = recover_installs(&root, &books);
    assert!(
        !outcome.complete,
        "the predecessor cannot go back under a name somebody else holds"
    );
    assert_eq!(body_named(&books, BOOK_NAME), b"not either of ours");
    let rollback = cache_root.open_dir(ROLLBACK_DIR).expect("rollback dir");
    let parked = rollback
        .open_long_name_file_in_dir(intent.rollback.as_str(), Mode::ReadOnly)
        .expect("the predecessor must still be parked, not reclaimed");
    assert_eq!(parked.length() as usize, BODY_BYTES);
    parked.close().expect("close");
}

/// The alias can be gone before recovery ever runs a step: the card comes out,
/// the book being replaced is deleted from a computer, and a different one
/// lands on the alias it gave up. Only the recorded chain can tell — the record
/// names the predecessor by both, and here the name still resolves while the
/// chain does not.
#[test]
fn a_book_that_took_the_alias_before_the_retire_is_not_retired_in_its_place() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);
    let alias = intent.old.clone().expect("a predecessor").alias;

    // The predecessor goes, and somebody else's book takes the name it left.
    // Nothing of this transaction's has run yet.
    assert_eq!(
        upload_store::remove_file_reclaiming_clusters(&books, alias.as_str()),
        upload_store::RemoveStatus::Removed,
        "the predecessor has to really be gone for the alias to be free"
    );
    let squatter = books
        .open_file_in_dir(alias.as_str(), Mode::ReadWriteCreate)
        .expect("a different book at the same alias");
    squatter.write(b"a different book entirely").expect("write");
    squatter.close().expect("close");

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete);
    assert_eq!(
        body(&books, alias.as_str()),
        b"a different book entirely",
        "the book answering to the recorded alias was retired as though it were the predecessor"
    );
    assert_eq!(
        body_named(&books, BOOK_NAME),
        new_body(),
        "and the upload lands, since nothing holds its long name now"
    );
    assert!(scratch_files(&root).is_empty(), "with nothing left over");
}

/// Retiring the predecessor frees its alias, and the driver hands a free alias
/// to the next file that needs one. So the recorded alias stops meaning "the
/// predecessor" the moment the retire completes, and recovery must not treat
/// whatever answers to it as a book of this transaction's to unlink.
#[test]
fn a_book_that_took_the_freed_alias_is_not_mistaken_for_the_predecessor() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    // Park the predecessor, which gives its alias back to the directory.
    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");
    let freed = intent.old.clone().expect("a predecessor").alias;
    assert!(
        !shelf_long_names(&books).iter().any(|n| n == BOOK_NAME),
        "the retire has to have completed, or the alias was never freed"
    );

    // Somebody else's book lands on it. Created under the bare alias so the
    // collision is certain rather than a guess about what the driver derives.
    let squatter = books
        .open_file_in_dir(freed.as_str(), Mode::ReadWriteCreate)
        .expect("a different book at the freed alias");
    squatter.write(b"a different book entirely").expect("write");
    squatter.close().expect("close");

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete, "the install still has somewhere to go");
    assert_eq!(
        body(&books, freed.as_str()),
        b"a different book entirely",
        "the file at the freed alias was unlinked as though it were the predecessor"
    );
    assert_eq!(
        body_named(&books, BOOK_NAME),
        new_body(),
        "and the upload still installed under its own name"
    );
}

/// The other half of the foreign-holder rule. With the predecessor parked, an
/// install whose name has been taken has no exit: the upload cannot be
/// installed, and the predecessor cannot go back under a name somebody else
/// holds. Recovery has to keep saying so, because the record is the only thing
/// keeping the sweep off the parked book.
#[test]
fn an_install_whose_name_was_taken_holds_on_to_the_parked_predecessor() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");

    // The name the upload was going to take is taken by somebody else.
    let intruder = books
        .create_file_in_dir_lfn(BOOK_NAME)
        .expect("somebody else's book");
    intruder.write(b"not either of ours").expect("write");
    intruder.close().expect("close");

    for pass in 0..2 {
        let outcome = recover_installs(&root, &books);
        assert!(
            !outcome.complete,
            "pass {pass}: there is no way to finish, and saying otherwise clears the record"
        );
        assert!(outcome.had_intent);
    }

    assert_eq!(
        body_named(&books, BOOK_NAME),
        b"not either of ours",
        "the file holding the name must be left alone"
    );
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let rollback = cache_root.open_dir(ROLLBACK_DIR).expect("rollback dir");
    let parked = rollback
        .open_long_name_file_in_dir(intent.rollback.as_str(), Mode::ReadOnly)
        .expect("the predecessor must still be parked, not swept");
    assert_eq!(
        parked.length() as usize,
        BODY_BYTES,
        "and parked whole, so resolving the collision can still put it back"
    );
    parked.close().expect("close");
}

/// [`InstallError::Malformed`] says retrying cannot help, so recovery must not
/// keep asking: the same error on every mount, waited out, refuses every
/// upload and delete for the life of the card.
///
/// Reaching it takes a card nothing here builds. A record with no predecessor
/// still plans a predecessor step if `observe` reports one, and only a shelf
/// entry sharing the parked copy's chain can — so the fixture links one, which
/// is the shape a half-finished retire leaves.
#[test]
fn a_step_the_record_cannot_describe_settles_instead_of_wedging() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    // A first upload: nothing to replace, so the record names no predecessor.
    let mut intent = intent(false);
    prepare(&root, &books, &mut intent);

    // One chain under two names, one of them parked under this record's
    // rollback name and one holding the book's long name on the shelf.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let rollback = cache_root.open_dir(ROLLBACK_DIR).expect("rollback dir");
    let stray = rollback
        .create_file_in_dir_lfn(intent.rollback.as_str())
        .expect("a parked copy this record never made");
    stray.write(&old_body()).expect("write");
    stray.close().expect("close");
    let parked_alias = alias_of(&rollback, intent.rollback.as_str()).expect("parked alias");
    rollback
        .link_file_in_dir_lfn(parked_alias.as_str(), &books, BOOK_NAME)
        .expect("give that chain a name on the shelf too");

    // Which is enough for the planner to ask for a step the record cannot do.
    let presence = install::observe(&root, &books, &intent).expect("observe");
    assert!(
        presence.old,
        "the fixture has to look like a standing old book"
    );
    assert!(matches!(
        install::apply_step(&root, &books, &intent, install::plan(&intent, presence)),
        Err(install::InstallError::Malformed)
    ));

    let outcome = recover_installs(&root, &books);
    assert!(
        outcome.complete,
        "an error that cannot resolve must not be retried forever"
    );
    assert!(outcome.had_intent, "and the catalog cannot be trusted");
    assert_eq!(
        install::read_intent(&root).expect("journal"),
        install::IntentState::Absent,
        "the record is let go, or the card refuses uploads for good"
    );

    // The book on the shelf keeps its data: the parked name shared its chain,
    // so the sweep unlinks that name rather than freeing anything.
    assert_eq!(body_named(&books, BOOK_NAME), old_body());

    // And the card takes work again.
    upload(&root, &books, "Something Else.epub", &new_body()).expect("a later upload");
}

/// A record *shorter* than one is never replayed. A record is written whole
/// before the first mutation and truncated only after the last, so a short one
/// belongs to a transaction that either had not started or had already
/// finished. Recovery must move nothing. What it cannot do is call the card
/// untouched: one of those two possibilities changed the shelf.
///
/// A whole record this build cannot read is the opposite case — it was
/// written, so its transaction had started — and is kept rather than
/// reclaimed. That is `a_record_from_another_build_is_kept_and_blocks_the_card`.
#[test]
fn a_torn_journal_changes_nothing() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    // Corrupt the record in place.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let file = cache_root
        .open_file_in_dir(install::JOURNAL_FILE, Mode::ReadWriteTruncate)
        .expect("open journal");
    file.write(&[0x5A; 40]).expect("scribble");
    file.close().expect("close");

    let outcome = recover_installs(&root, &books);
    assert!(
        outcome.complete,
        "there is nothing to replay, so recovery is settled"
    );
    assert!(
        outcome.had_intent,
        "a record was there, and it may have been the one a finished install \
         was interrupted while clearing"
    );
    assert!(
        !outcome.touched_shelf,
        "nothing may be moved on the strength of a record that cannot be read"
    );
    assert_eq!(
        install::read_intent(&root).expect("journal"),
        install::IntentState::Absent,
        "and it is reclaimed, or it would retire the catalog on every mount"
    );
    assert_eq!(
        body_named(&books, BOOK_NAME),
        old_body(),
        "no install may have happened"
    );
}

/// Losing the upload after the predecessor has been unlinked is the state
/// that makes the unlink-versus-reclaim rule bite. The predecessor's only
/// remaining name is the parked one, so if unlinking had freed the chain the
/// restored book would read back as rubble.
#[test]
fn a_predecessor_restored_after_a_lost_upload_is_still_whole() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    // Retiring is one move: the predecessor is parked and off the shelf.
    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");

    // The scratch file goes missing before it was ever installed.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let upload = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");
    upload
        .delete_entry_in_dir(intent.stage.alias.as_str())
        .expect("lose the upload");

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete, "recovery must resolve a lost upload");

    assert_eq!(
        body_named(&books, BOOK_NAME),
        old_body(),
        "the book that was already on the shelf must come back intact"
    );
    let names = shelf_long_names(&books);
    assert_eq!(
        names.iter().filter(|n| n.as_str() == BOOK_NAME).count(),
        1,
        "exactly one copy, under its own name ({names:?})"
    );
}

// ---------------------------------------------------------------------------
// The caller's path: stage, then install
// ---------------------------------------------------------------------------

use upload_store::install::StagedUpload;

/// Whatever is sitting in the scratch directory.
fn scratch_files(root: &Dir<'_>) -> Vec<std::string::String> {
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let upload_dir = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");
    let mut left = Vec::new();
    upload_dir
        .iterate_dir(|entry| {
            let name = entry.name.to_string();
            if !entry.attributes.is_directory() && name != "." && name != ".." {
                left.push(name);
            }
            ControlFlow::Continue(())
        })
        .expect("iterate");
    left
}

fn upload(root: &Dir<'_>, books: &Dir<'_>, name: &str, body: &[u8]) -> Option<String<12>> {
    landing(root, books, name, body).map(|landed| landed.alias)
}

/// The whole landing, for tests that care what the bytes turned out to be.
fn landing(root: &Dir<'_>, books: &Dir<'_>, name: &str, body: &[u8]) -> Option<Landed> {
    let mut staged = StagedUpload::begin(root, books, name, None).expect("stage");
    staged.write(body).expect("stream");
    staged.install(root, books, &mut words()).expect("install")
}

#[test]
fn an_upload_lands_under_its_long_name() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let alias = upload(&root, &books, BOOK_NAME, &new_body()).expect("install");

    assert_eq!(body(&books, alias.as_str()), new_body());
    assert_eq!(
        shelf_long_names(&books),
        vec![BOOK_NAME.to_string()],
        "the shelf shows the book under the name the user gave it"
    );
    assert!(
        install::read_intent(&root).expect("journal") == install::IntentState::Absent,
        "a finished install leaves no journal"
    );
}

#[test]
fn uploading_the_same_name_again_replaces_the_book() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    upload(&root, &books, BOOK_NAME, &old_body()).expect("first");
    let alias = upload(&root, &books, BOOK_NAME, &new_body()).expect("second");

    assert_eq!(
        body(&books, alias.as_str()),
        new_body(),
        "the second upload is what the shelf serves"
    );
    assert_eq!(
        shelf_long_names(&books)
            .iter()
            .filter(|n| n.as_str() == BOOK_NAME)
            .count(),
        1,
        "a folder must not end up with two files of one name"
    );
}

/// Case is not a new book. FAT compares long names case-insensitively, and a
/// computer would refuse the second file, so the upload has to replace rather
/// than add.
#[test]
fn a_case_variant_replaces_rather_than_duplicates() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    upload(&root, &books, "Middlemarch.epub", &old_body()).expect("first");
    let alias = upload(&root, &books, "MIDDLEMARCH.EPUB", &new_body()).expect("second");

    let names = shelf_long_names(&books);
    assert_eq!(names.len(), 1, "one book, not two ({names:?})");
    assert_eq!(
        body(&books, alias.as_str()),
        new_body(),
        "the case variant must replace the book, not be discarded by it"
    );
}

#[test]
fn an_abandoned_upload_leaves_the_shelf_exactly_as_it_was() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);
    let alias = upload(&root, &books, BOOK_NAME, &old_body()).expect("first");

    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, None).expect("stage");
    staged.write(b"half a book").expect("stream");
    staged.abandon(&root);

    assert_eq!(
        body(&books, alias.as_str()),
        old_body(),
        "the book already on the shelf is untouched"
    );
    assert_eq!(shelf_long_names(&books), vec![BOOK_NAME.to_string()]);

    // And the scratch space is clear, so the next upload starts empty.
    assert!(
        scratch_files(&root).is_empty(),
        "scratch left behind: {:?}",
        scratch_files(&root)
    );
}

/// A book that never finished streaming is never published, so a mount that
/// happens before the install has nothing to clean out of the library.
#[test]
fn an_upload_interrupted_mid_stream_never_reaches_the_shelf() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, None).expect("stage");
    staged.write(b"the first few chapters").expect("stream");
    drop(staged);
    assert!(
        !scratch_files(&root).is_empty(),
        "the half-written file should still be in scratch before recovery"
    );

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete);
    assert!(!outcome.touched_shelf);
    assert!(
        shelf_long_names(&books).is_empty(),
        "an upload that was never installed must leave the shelf empty"
    );
    // A book nobody uploads again would otherwise keep its scratch forever.
    assert!(
        scratch_files(&root).is_empty(),
        "recovery must reclaim scratch no transaction is using: {:?}",
        scratch_files(&root)
    );
}

/// Two books whose names collide on the 8.3 alias are still two books.
#[test]
fn books_that_share_an_alias_both_survive() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let first = upload(&root, &books, "A Very Long Title Indeed.epub", &old_body()).expect("first");
    let second = upload(
        &root,
        &books,
        "A Very Long Title Indeed II.epub",
        &new_body(),
    )
    .expect("second");

    assert_ne!(first, second, "each book needs its own alias");
    assert_eq!(body(&books, first.as_str()), old_body());
    assert_eq!(body(&books, second.as_str()), new_body());
    assert_eq!(shelf_long_names(&books).len(), 2);
}

// ---------------------------------------------------------------------------
// One journal, one owner
// ---------------------------------------------------------------------------

fn journal_bytes(root: &Dir<'_>) -> Vec<u8> {
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let file = cache_root
        .open_file_in_dir(install::JOURNAL_FILE, Mode::ReadOnly)
        .expect("journal present");
    let mut out = vec![0u8; file.length() as usize];
    file.read(&mut out).expect("read journal");
    out
}

/// A record in flight owns the names it describes. A second transaction
/// starting alongside it would take the scratch and alias names it is
/// counting on, and writing a second record would destroy the only
/// description of where the first one's files went — leaving a predecessor
/// parked in rollback under a name nothing is looking for.
#[test]
fn a_second_upload_cannot_displace_an_unfinished_one() {
    // Stop transaction A at each point where its record is still owed work.
    for stop_after in 1..=3 {
        let disk = new_card();
        let mut intent_a = intent(true);
        let mgr = open_mgr(disk);
        let (root, books) = open_dirs(&mgr);
        prepare(&root, &books, &mut intent_a);

        for _ in 0..stop_after {
            let presence = install::observe(&root, &books, &intent_a).expect("observe");
            let step = install::plan(&intent_a, presence);
            if step == Step::Done {
                break;
            }
            install::apply_step(&root, &books, &intent_a, step).expect("step");
        }
        let before = journal_bytes(&root);

        // B tries to start while A's record still stands.
        let refused = StagedUpload::begin(&root, &books, "Another Book.epub", None);
        assert!(
            matches!(refused, Err(install::InstallError::Busy)),
            "a second upload must be refused while a record is in flight (stopped after {stop_after})"
        );

        // And writing B's record directly must not overwrite A's.
        let intent_b = InstallIntent {
            stage: Located {
                alias: short("TXN00002.TMP"),
                chain: 77,
            },
            long_name: long("Another Book.epub"),
            old: None,
            rollback: short("TXN00002.OLD"),
        };
        assert!(
            matches!(
                install::write_intent(&root, &intent_b),
                Err(install::InstallError::Busy)
            ),
            "a record may not be replaced while it is owed work"
        );
        assert_eq!(
            journal_bytes(&root),
            before,
            "A's record must survive byte for byte (stopped after {stop_after})"
        );

        // A still finishes, which is the point of protecting its record.
        let outcome = recover_installs(&root, &books);
        assert!(outcome.complete, "A must still be finishable");
        assert_eq!(body_named(&books, BOOK_NAME), new_body());
    }
}

/// Once a record is cleared the card is free again.
#[test]
fn a_finished_transaction_releases_the_card() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    upload(&root, &books, BOOK_NAME, &old_body()).expect("first");
    upload(&root, &books, "Another Book.epub", &new_body()).expect("second");

    assert_eq!(shelf_long_names(&books).len(), 2);
}

/// The shelf can stop matching the catalog *before* a reset, leaving recovery
/// with nothing to change in `/BOOKS` and a cached catalog that describes an
/// alias no longer on the card. The record's existence is what says the
/// catalog is stale — not what this pass had left to do.
#[test]
fn a_record_left_by_an_installed_book_still_invalidates_the_catalog() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    // Walk right up to the last step: the book is installed under its new
    // alias, the predecessor is gone from /BOOKS, and only the rollback copy
    // is left to reclaim.
    for step in [Step::RetireOldHolder, Step::InstallStage] {
        install::apply_step(&root, &books, &intent, step).expect("step");
    }
    let presence = install::observe(&root, &books, &intent).expect("observe");
    assert_eq!(
        install::plan(&intent, presence),
        Step::ReclaimRollback,
        "only the rollback copy should be left"
    );

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete);
    assert!(
        !outcome.touched_shelf,
        "this pass really does leave /BOOKS alone -- which is the trap"
    );
    assert!(
        outcome.had_intent,
        "a catalog read before this recovery cannot be trusted: /BOOKS moved \
         from the old alias to the new one before the reset"
    );
}

/// A book uploaded before long names existed is an 8.3 alias with a `.TXT`
/// label and a `.ID` identity sidecar, and no long name at all. Re-uploading
/// it must replace it: matching on the long name alone finds nothing, and the
/// user ends up with the same book twice.
#[test]
fn re_uploading_a_book_from_before_long_names_replaces_it() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    // Lay down the old shape by hand: alias, label, identity, no LFN.
    let client_name = b"Middlemarch.epub";
    let legacy_alias = proto::upload::sanitized_name(client_name);
    let identity = proto::upload::hash_identity(client_name);
    let legacy = books
        .open_file_in_dir(legacy_alias.as_str(), Mode::ReadWriteCreate)
        .expect("legacy book");
    legacy.write(&old_body()).expect("write");
    legacy.close().expect("close");
    write_legacy_sidecars(&root, legacy_alias.as_str(), identity, BOOK_NAME);

    let key = upload_store::install::LegacyKey {
        alias: short(legacy_alias.as_str()),
        identity,
    };
    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, Some(key)).expect("stage");
    staged.write(&new_body()).expect("stream");
    let alias = staged
        .install(&root, &books, &mut words())
        .expect("install")
        .expect("the book landed")
        .alias;

    assert_ne!(
        alias.as_str(),
        legacy_alias.as_str(),
        "the new copy gets its own alias"
    );
    assert_eq!(body(&books, alias.as_str()), new_body());
    assert_eq!(
        shelf_long_names(&books),
        vec![BOOK_NAME.to_string()],
        "one book on the shelf, under its long name"
    );
    assert!(
        books.find_directory_entry(legacy_alias.as_str()).is_err(),
        "the book it replaced must be gone, not left beside it"
    );
    assert_eq!(
        upload_store::read_upload_identity(&root, legacy_alias.as_str()),
        Ok(None),
        "the retired book's identity sidecar describes nothing now"
    );
}

/// A different book that happens to sit in the same probe window is not the
/// book being replaced — which is what the identity sidecar is for, since two
/// books can share a truncated label.
#[test]
fn a_legacy_book_with_another_identity_is_left_alone() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let client_name = b"Middlemarch.epub";
    let legacy_alias = proto::upload::sanitized_name(client_name);
    let stranger = books
        .open_file_in_dir(legacy_alias.as_str(), Mode::ReadWriteCreate)
        .expect("stranger");
    stranger.write(&old_body()).expect("write");
    stranger.close().expect("close");
    write_legacy_sidecars(
        &root,
        legacy_alias.as_str(),
        proto::upload::hash_identity(b"A Different Book.epub"),
        "A Different Book.epub",
    );

    let key = upload_store::install::LegacyKey {
        alias: short(legacy_alias.as_str()),
        identity: proto::upload::hash_identity(client_name),
    };
    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, Some(key)).expect("stage");
    staged.write(&new_body()).expect("stream");
    let alias = staged
        .install(&root, &books, &mut words())
        .expect("install")
        .expect("the book landed")
        .alias;

    assert_eq!(body(&books, alias.as_str()), new_body());
    assert_eq!(
        body(&books, legacy_alias.as_str()),
        old_body(),
        "someone else's book must survive an upload that is not theirs"
    );
}

fn write_legacy_sidecars(root: &Dir<'_>, alias: &str, identity: u64, label: &str) {
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .unwrap_or_else(|_| {
            root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("make cache root");
            root.open_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("open cache root")
        });
    if cache_root.open_dir("LABELS").is_err() {
        cache_root.make_dir_in_dir("LABELS").expect("make LABELS");
    }
    let labels = cache_root.open_dir("LABELS").expect("labels");
    let stem = alias.split('.').next().expect("stem");

    let mut id_name = std::string::String::from(stem);
    id_name.push_str(".ID");
    let file = labels
        .open_file_in_dir(id_name.as_str(), Mode::ReadWriteCreate)
        .expect("identity sidecar");
    file.write(&identity.to_le_bytes()).expect("write identity");
    file.close().expect("close");

    let mut txt_name = std::string::String::from(stem);
    txt_name.push_str(".TXT");
    let file = labels
        .open_file_in_dir(txt_name.as_str(), Mode::ReadWriteCreate)
        .expect("label sidecar");
    file.write(label.as_bytes()).expect("write label");
    file.close().expect("close");
}

/// Clearing leftovers is housekeeping, not part of the transaction. Those
/// files sit outside `/BOOKS` and no catalog or reader can see them, so a
/// failure to delete one must not turn a committed upload into a failed one
/// — nor, through `complete`, stop the library being scanned.
#[test]
fn a_leftover_that_will_not_delete_does_not_fail_the_install() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    // A stray scratch file from some earlier upload, held open so the sweep
    // cannot reclaim it.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .unwrap_or_else(|_| {
            root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("make cache root");
            root.open_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("open cache root")
        });
    if cache_root.open_dir(UPLOAD_DIR).is_err() {
        cache_root.make_dir_in_dir(UPLOAD_DIR).expect("make UPLOAD");
    }
    let upload_dir = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");
    let stray = upload_dir
        .open_file_in_dir("STRAY001.TMP", Mode::ReadWriteCreate)
        .expect("stray");
    stray.write(b"orphaned").expect("write");

    let alias = upload(&root, &books, BOOK_NAME, &new_body())
        .expect("a book must still install with a leftover in the way");
    assert_eq!(body(&books, alias.as_str()), new_body());

    let outcome = recover_installs(&root, &books);
    assert!(
        outcome.complete,
        "no journal is outstanding, so the transaction state is settled"
    );
    assert!(
        !outcome.swept,
        "the stray really is undeletable here, or this proves nothing"
    );

    // Once it is closed, the next pass tidies it away.
    stray.close().expect("close");
    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete && outcome.swept);
    assert!(scratch_files(&root).is_empty());
}

/// The sweep takes a bounded number of files per pass, and `swept` is the only
/// signal that any are left. Stopping at the quota has to read the same as
/// failing to delete one, or the "could not clear every leftover" line goes
/// quiet on the card with the most to clear.
#[test]
fn a_sweep_that_runs_out_of_quota_says_leftovers_remain() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    // More than one pass takes. If the quota is ever raised past this, the
    // first assertion below is what says so.
    const LEFTOVERS: usize = 9;
    root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("make cache root");
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    cache_root.make_dir_in_dir(UPLOAD_DIR).expect("make UPLOAD");
    let upload_dir = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");
    for index in 0..LEFTOVERS {
        let mut name = std::string::String::from("LEFT000");
        name.push(char::from_digit(index as u32, 10).expect("digit"));
        name.push_str(".TMP");
        let stray = upload_dir
            .open_file_in_dir(name.as_str(), Mode::ReadWriteCreate)
            .expect("stray");
        stray.write(b"an upload nobody finished").expect("write");
        stray.close().expect("close");
    }

    let first = recover_installs(&root, &books);
    assert!(
        first.complete,
        "leftovers are housekeeping; they cannot fail the transaction"
    );
    assert!(
        !first.swept,
        "the pass stopped at its quota with files still there, and said it was done"
    );
    let left = scratch_files(&root).len();
    assert!(
        left > 0 && left < LEFTOVERS,
        "the pass should have taken some but not all, and left {left}"
    );

    // The rest go on the next mount, and only then is the sweep finished.
    let second = recover_installs(&root, &books);
    assert!(
        second.swept,
        "nothing is left, so nothing is left to report"
    );
    assert!(scratch_files(&root).is_empty());
}

/// A restore cut between its two writes leaves the predecessor back on the
/// shelf while the parked copy still stands — one chain under both names.
/// Recovery has to see that shelf entry *as* the predecessor, and the alias
/// cannot tell it: the driver derived a fresh one for the long name, and a
/// book stored before long names had one of the old uploader's, which nothing
/// hands back. Reading it as "no predecessor" sends recovery round to restore
/// again, which fails on a long name already taken, forever.
#[test]
fn a_restore_cut_half_way_through_is_finished_not_repeated() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    // A book from before long names: 8.3 alias, sidecars, no LFN at all.
    let client_name = b"Middlemarch.epub";
    let legacy_alias = proto::upload::sanitized_name(client_name);
    let identity = proto::upload::hash_identity(client_name);
    let legacy = books
        .open_file_in_dir(legacy_alias.as_str(), Mode::ReadWriteCreate)
        .expect("legacy book");
    legacy.write(&old_body()).expect("write");
    legacy.close().expect("close");
    write_legacy_sidecars(&root, legacy_alias.as_str(), identity, BOOK_NAME);

    let mut intent = InstallIntent {
        stage: Located {
            alias: short("TXN00001.TMP"),
            chain: 0,
        },
        long_name: long(BOOK_NAME),
        old: Some(Located {
            alias: short(legacy_alias.as_str()),
            chain: books
                .find_directory_entry(legacy_alias.as_str())
                .expect("the legacy book")
                .cluster
                .value(),
        }),
        rollback: short("TXN00001.OLD"),
    };
    prepare_scratch(&root, &mut intent);
    install::write_intent(&root, &intent).expect("journal");

    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");

    // The upload is lost, so the transaction has to walk backwards.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let upload_dir = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");
    upload_dir
        .delete_entry_in_dir(intent.stage.alias.as_str())
        .expect("lose the upload");

    let presence = install::observe(&root, &books, &intent).expect("observe");
    assert_eq!(install::plan(&intent, presence), Step::RestoreOldHolder);

    // Cut power inside the restore: the shelf entry is written, the parked
    // one is not yet taken away.
    let before = disk.image();
    install::apply_step(&root, &books, &intent, Step::RestoreOldHolder).expect("restore");
    disk.undo_unlinks(&before);

    let presence = install::observe(&root, &books, &intent).expect("observe");
    assert!(
        presence.old,
        "the shelf entry carries the parked copy's chain, so it is the \
         predecessor whatever it is called: {presence:?}"
    );
    assert!(presence.rollback && !presence.stage && !presence.dest);
    assert_eq!(
        install::plan(&intent, presence),
        Step::UnlinkRollbackCopy,
        "the parked name is the one to drop, and only the name"
    );

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete, "recovery must finish, not go round again");
    assert!(
        install::read_intent(&root).expect("journal") == install::IntentState::Absent,
        "the record must be cleared"
    );
    assert_eq!(
        body_named(&books, BOOK_NAME),
        old_body(),
        "the predecessor must be back, whole"
    );
    assert_eq!(shelf_long_names(&books).len(), 1, "exactly one book");
}

/// Just the scratch file, for fixtures that build their own intent.
fn prepare_scratch(root: &Dir<'_>, intent: &mut InstallIntent) {
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .unwrap_or_else(|_| {
            root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("make cache root");
            root.open_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("open cache root")
        });
    for name in [UPLOAD_DIR, ROLLBACK_DIR] {
        if cache_root.open_dir(name).is_err() {
            cache_root.make_dir_in_dir(name).expect("make subdir");
        }
    }
    let upload = cache_root.open_dir(UPLOAD_DIR).expect("upload dir");
    let staged = upload
        .open_file_in_dir(intent.stage.alias.as_str(), Mode::ReadWriteCreate)
        .expect("scratch file");
    staged.write(&new_body()).expect("stream");
    staged.close().expect("close");
    intent.stage.chain = upload
        .find_directory_entry(intent.stage.alias.as_str())
        .expect("the scratch file")
        .cluster
        .value();
}

/// Clearing the journal truncates it before unlinking it, so a cut in between
/// leaves a zero-length record behind — after the shelf has already changed.
/// Reading that as "no record" is how a catalog naming the old alias survives
/// a replacement.
#[test]
fn a_journal_cut_while_being_cleared_still_retires_the_catalog() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    upload(&root, &books, BOOK_NAME, &old_body()).expect("first");
    upload(&root, &books, BOOK_NAME, &new_body()).expect("replacement");

    // Reproduce the first half of clearing: the record is truncated, its
    // directory entry is not yet gone.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    cache_root
        .open_file_in_dir(install::JOURNAL_FILE, Mode::ReadWriteCreate)
        .expect("recreate journal")
        .close()
        .expect("close");

    let outcome = recover_installs(&root, &books);
    assert!(
        outcome.complete,
        "an install that reached Done has nothing left to replay"
    );
    assert!(
        outcome.had_intent,
        "a record was on the card, and the shelf moved before it was cleared"
    );
    assert_eq!(
        install::read_intent(&root).expect("journal"),
        install::IntentState::Absent,
        "the leftover record is reclaimed, not carried forever"
    );
    assert_eq!(body_named(&books, BOOK_NAME), new_body());
}

/// A parked copy can still share its chain with a book on the shelf — what a
/// move cut half way through leaves. If its record was lost too, the sweep is
/// what finds it, and reclaiming there would free the clusters the shelf copy
/// reads from.
#[test]
fn the_sweep_never_frees_a_chain_the_shelf_is_reading() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    let mut intent = intent(true);
    prepare(&root, &books, &mut intent);

    // Park the predecessor, then undo the unlink half: the shelf entry and
    // the parked copy now name one chain.
    let before = disk.image();
    install::apply_step(&root, &books, &intent, Step::RetireOldHolder).expect("retire");
    disk.undo_unlinks(&before);

    // And the record goes missing, so nothing above the sweep will tidy it.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    cache_root
        .delete_entry_in_dir(install::JOURNAL_FILE)
        .expect("lose the record");

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete, "no record, nothing to replay");
    assert_eq!(
        body_named(&books, BOOK_NAME),
        old_body(),
        "the book the shelf is holding must survive the sweep intact"
    );

    // The parked name is gone either way; only the chain was spared.
    let rollback = cache_root.open_dir(ROLLBACK_DIR).expect("rollback dir");
    let mut parked = Vec::new();
    rollback
        .iterate_dir(|entry| {
            let name = entry.name.to_string();
            if !entry.attributes.is_directory() && name != "." && name != ".." {
                parked.push(name);
            }
            ControlFlow::Continue(())
        })
        .expect("iterate");
    assert!(
        parked.is_empty(),
        "the parked name was not cleared: {parked:?}"
    );

    // Once nothing on the shelf shares it, an ordinary leftover is reclaimed.
    assert!(
        outcome.swept,
        "the sweep reported a failure it did not have"
    );
}

/// The mirror of the rollback case: an install move cut half way leaves the
/// scratch file and the shelf entry naming one chain. If the record is lost
/// too, the sweep is what finds the scratch file — and reclaiming it would
/// free the clusters the book on the shelf is reading from.
#[test]
fn the_sweep_never_frees_a_chain_a_just_installed_book_is_reading() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    let mut intent = intent(false);
    prepare(&root, &books, &mut intent);

    // Publish the book, then undo the unlink half of the move.
    let before = disk.image();
    install::apply_step(&root, &books, &intent, Step::InstallStage).expect("install");
    disk.undo_unlinks(&before);
    assert!(
        !scratch_files(&root).is_empty(),
        "the scratch name must be back for this to test anything"
    );

    // And the record goes missing, so only the sweep is left.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    cache_root
        .delete_entry_in_dir(install::JOURNAL_FILE)
        .expect("lose the record");

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete, "no record, nothing to replay");
    assert_eq!(
        body_named(&books, BOOK_NAME),
        new_body(),
        "the installed book must survive the sweep intact"
    );
    assert!(
        scratch_files(&root).is_empty(),
        "the scratch name is still cleared; only the chain is spared"
    );
}

/// The card is browsable from a computer now, so the name an upload is about
/// to take can be taken by somebody else while it is in flight. That file is
/// not this transaction's to move aside, and the install can never happen —
/// but refusing forever would leave the card taking no uploads and no deletes
/// for as long as the file stayed there.
#[test]
fn an_upload_gives_up_when_something_else_took_its_name() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    // An upload finished and journalled, with no book to replace.
    let mut intent = intent(false);
    prepare(&root, &books, &mut intent);

    // Then the name is taken, by a file on neither of this transaction's
    // chains.
    let intruder = books
        .create_file_in_dir_lfn(BOOK_NAME)
        .expect("somebody else's book");
    intruder.write(&old_body()).expect("write");
    intruder.close().expect("close");

    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete, "the card must settle, not refuse forever");
    assert!(
        outcome.had_intent,
        "a record was in flight, so a cached catalog cannot be trusted"
    );
    assert!(
        !outcome.touched_shelf,
        "giving up must not have moved anything in /BOOKS"
    );
    assert_eq!(
        body_named(&books, BOOK_NAME),
        old_body(),
        "the file that holds the name must be left exactly as it was"
    );
    assert_eq!(
        shelf_long_names(&books),
        vec![BOOK_NAME.to_string()],
        "and it must be the only thing on the shelf"
    );
    assert!(
        scratch_files(&root).is_empty(),
        "the abandoned upload is reclaimed by the sweep, not leaked"
    );

    // A second pass has nothing left to say, so the catalog it rebuilt stands.
    let again = recover_installs(&root, &books);
    assert!(again.complete);
    assert!(
        !again.had_intent,
        "the record is gone, so this must stop retiring the catalog"
    );

    // And the card takes uploads again: this time the file holding the name is
    // seen up front and replaced properly.
    upload(&root, &books, BOOK_NAME, &new_body()).expect("upload again");
    assert_eq!(shelf_long_names(&books), vec![BOOK_NAME.to_string()]);
    assert_eq!(body_named(&books, BOOK_NAME), new_body());
}

/// A whole record this build cannot read is not a torn one. It was written
/// before the transaction touched anything, so its transaction had started —
/// and if it came from a build with a newer format, the work it describes is
/// in a shape this one cannot see. Erasing it would destroy the only account
/// of where a book went.
#[test]
fn a_record_from_another_build_is_kept_and_blocks_the_card() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);
    upload(&root, &books, BOOK_NAME, &old_body()).expect("a book to protect");

    // A full-size record with a version this build does not know.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let file = cache_root
        .open_file_in_dir(install::JOURNAL_FILE, Mode::ReadWriteCreate)
        .expect("journal");
    // Built by the encoder, so the header is whatever this build writes, and
    // then aged: only the version is changed. A magic spelled out here would
    // stop matching if the header ever changed, and the record would be
    // rejected for that instead -- still Unrecognized, so the test would keep
    // passing while no longer reaching the version check at all.
    let mut record = InstallIntent {
        stage: Located {
            alias: short("TXN00009.TMP"),
            chain: 33,
        },
        long_name: long("From A Later Build.epub"),
        old: None,
        rollback: short("TXN00009.OLD"),
    }
    .encode();
    let unknown_version = 0xFFFFu16;
    record[4..6].copy_from_slice(&unknown_version.to_le_bytes());
    file.write(&record).expect("write");
    file.close().expect("close");

    assert_eq!(
        install::read_intent(&root).expect("journal"),
        install::IntentState::Unrecognized
    );

    let outcome = recover_installs(&root, &books);
    assert!(
        !outcome.complete,
        "a record this build cannot read is not a settled card"
    );
    assert!(outcome.had_intent, "and the catalog cannot be trusted");
    assert_eq!(
        install::read_intent(&root).expect("journal"),
        install::IntentState::Unrecognized,
        "the record must be preserved, not reclaimed"
    );

    // Nothing may write while it stands.
    assert!(matches!(
        StagedUpload::begin(&root, &books, "Another Book.epub", None),
        Err(install::InstallError::Busy)
    ));
    assert_eq!(body_named(&books, BOOK_NAME), old_body());
}

/// The sweep is best-effort by design, so it cannot be the only thing
/// standing between a half-finished install and the next upload. If it could
/// not clear the scratch name — a transient card error is enough — the next
/// upload of a book deriving that same name finds it still there, and
/// reclaiming it would free the clusters of the book already on the shelf.
#[test]
fn staging_over_a_leftover_never_frees_a_chain_the_shelf_is_reading() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    // The scratch name has to be the one a later upload of this book will
    // derive, or nothing would collide and this would prove nothing.
    let derived = proto::upload::upload_short_alias(BOOK_NAME, 0);
    let stem = derived.as_str().split('.').next().expect("stem");
    let mut intent = intent(false);
    intent.stage.alias = short(&format!("{stem}.TMP"));
    prepare(&root, &books, &mut intent);

    // An install cut between its two writes: the book is published and the
    // scratch name still points at the same chain.
    let before = disk.image();
    install::apply_step(&root, &books, &intent, Step::InstallStage).expect("install");
    disk.undo_unlinks(&before);

    // The record is gone, and the sweep never got to this file.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    cache_root
        .delete_entry_in_dir(install::JOURNAL_FILE)
        .expect("lose the record");
    assert!(
        !scratch_files(&root).is_empty(),
        "the leftover must still be there for this to test anything"
    );

    // The next upload of the same book derives the same scratch name.
    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, None).expect("stage");
    assert_eq!(
        body_named(&books, BOOK_NAME),
        new_body(),
        "the book on the shelf must survive having its scratch name reused"
    );

    // And the upload that follows still works, replacing it properly.
    staged.write(&old_body()).expect("stream");
    staged
        .install(&root, &books, &mut words())
        .expect("install");
    assert_eq!(body_named(&books, BOOK_NAME), old_body());
    assert_eq!(shelf_long_names(&books).len(), 1);
}

/// Recovery changes `/BOOKS` like anything else, and owes the same proof.
///
/// It ends by clearing the record, and that record is the only thing that
/// would tell the next mount a surviving snapshot is stale. So replaying while
/// a snapshot that would not delete was standing would move the books and then
/// destroy the evidence: the mount after finds a snapshot of the old shelf, no
/// record, and nothing to say they disagree.
#[test]
fn recovery_leaves_the_shelf_alone_while_a_snapshot_it_cannot_clear_stands() {
    let disk = new_card();
    let mut intent = intent(true);
    let mgr = open_mgr(disk);
    let (root, books) = open_dirs(&mgr);
    prepare(&root, &books, &mut intent);

    // A snapshot of the shelf as it stands, held open so it cannot be removed
    // — the transient failure a wireless session hits on the way in.
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    let snapshot = cache_root
        .open_file_in_dir(proto::cache::CATALOG_FILE, Mode::ReadWriteCreate)
        .expect("snapshot");
    snapshot
        .write(b"a catalog of the shelf as it stands")
        .expect("write");

    let outcome = recover_installs(&root, &books);
    assert!(
        !outcome.complete,
        "a snapshot that will not go leaves the whole transaction for the next mount"
    );
    assert!(outcome.had_intent, "and the catalog cannot be trusted");
    assert!(!outcome.touched_shelf);
    assert_eq!(
        body_named(&books, BOOK_NAME),
        old_body(),
        "the shelf must not have advanced past what the snapshot describes"
    );
    assert!(
        matches!(
            install::read_intent(&root),
            Ok(install::IntentState::Valid(_))
        ),
        "and the record has to still be there to catch it next time"
    );

    // Once the snapshot can go, it goes first and the transaction finishes.
    snapshot.close().expect("close");
    let outcome = recover_installs(&root, &books);
    assert!(outcome.complete);
    assert_eq!(body_named(&books, BOOK_NAME), new_body());
    assert!(
        matches!(
            cache_root.find_directory_entry(proto::cache::CATALOG_FILE),
            Err(embedded_sdmmc::Error::NotFound)
        ),
        "the snapshot went before the shelf moved, not after"
    );
}

/// Invalidating the catalog snapshot is a precondition for changing `/BOOKS`,
/// not a courtesy: a clean install clears its journal and a delete never
/// writes one, so once the shelf has moved nothing on the card would tell the
/// next mount that the snapshot is describing the old one. The caller may
/// only proceed when the card says the snapshot is gone.
#[test]
fn a_snapshot_that_will_not_go_is_not_reported_gone() {
    let mgr = open_mgr(new_card());
    let (root, _books) = open_dirs(&mgr);

    // No cache root at all: nothing can be holding a snapshot.
    assert!(
        upload_store::clear_cache_file(&root, proto::cache::CATALOG_FILE),
        "a card with no cache root has no snapshot to invalidate"
    );

    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .unwrap_or_else(|_| {
            root.make_dir_in_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("make cache root");
            root.open_dir(proto::cache::CACHE_ROOT_DIR)
                .expect("open cache root")
        });

    // Present but not there: still nothing to invalidate.
    assert!(upload_store::clear_cache_file(
        &root,
        proto::cache::CATALOG_FILE
    ));

    // A snapshot that cannot be removed must not read as removed.
    let snapshot = cache_root
        .open_file_in_dir(proto::cache::CATALOG_FILE, Mode::ReadWriteCreate)
        .expect("snapshot");
    snapshot
        .write(b"a catalog of the old shelf")
        .expect("write");
    assert!(
        !upload_store::clear_cache_file(&root, proto::cache::CATALOG_FILE),
        "a snapshot still held open is still on the card"
    );

    // Once it can be removed, it is, and the card says so.
    snapshot.close().expect("close");
    assert!(upload_store::clear_cache_file(
        &root,
        proto::cache::CATALOG_FILE
    ));
    assert!(
        matches!(
            cache_root.find_directory_entry(proto::cache::CATALOG_FILE),
            Err(embedded_sdmmc::Error::NotFound)
        ),
        "and it really is gone"
    );
}

/// The sidecar readers had no direct test at all, which is how a change to the
/// cache root could have moved them silently.
#[test]
fn labels_under_the_current_cache_root_are_read() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let alias = proto::upload::sanitized_name(b"Middlemarch.epub");
    let existing = books
        .open_file_in_dir(alias.as_str(), Mode::ReadWriteCreate)
        .expect("a book from before long names");
    existing.write(&old_body()).expect("write");
    existing.close().expect("close");
    let identity = proto::upload::hash_identity(b"Middlemarch.epub");
    write_legacy_sidecars(&root, alias.as_str(), identity, "Middlemarch");

    let mut label = String::<64>::new();
    assert!(
        upload_store::read_upload_label(
            &root,
            BookRoot::Library,
            alias.as_str(),
            alias.as_str(),
            &mut label
        ),
        "a label under {} must be found",
        proto::cache::CACHE_ROOT_DIR
    );
    assert_eq!(label.as_str(), "Middlemarch");
    assert_eq!(
        upload_store::read_upload_identity(&root, alias.as_str()),
        Ok(Some(identity))
    );
}

/// A label names an alias, and an alias only means anything inside one
/// directory. The scheme that wrote these put every book straight into
/// `/BOOKS`, so a nested book wearing the same alias is a different file, and
/// a computer that deleted or moved the labelled one cannot clear what it left
/// behind. Reading the label for it would put another book's name in the list.
#[test]
fn a_nested_book_does_not_inherit_a_flat_alias_label() {
    let mgr = open_mgr(new_card());
    let (root, books) = open_dirs(&mgr);

    let alias = proto::upload::sanitized_name(b"Middlemarch.epub");
    let existing = books
        .open_file_in_dir(alias.as_str(), Mode::ReadWriteCreate)
        .expect("a book from before long names");
    existing.write(&old_body()).expect("write");
    existing.close().expect("close");
    write_legacy_sidecars(
        &root,
        alias.as_str(),
        proto::upload::hash_identity(b"Middlemarch.epub"),
        "Middlemarch",
    );

    // The same alias, legally, one directory down: FAT hands out 8.3 names
    // per directory, not per card.
    let mut label = String::<64>::new();
    let mut nested = std::string::String::from("Fiction/");
    nested.push_str(alias.as_str());
    assert!(
        !upload_store::read_upload_label(
            &root,
            BookRoot::Library,
            nested.as_str(),
            alias.as_str(),
            &mut label
        ),
        "a nested book has no label in a flat alias namespace"
    );
    assert!(label.is_empty(), "and nothing is written into the label");

    // The card root is the other place the old scheme could name, so it still
    // asks; only depth is out of reach.
    assert!(upload_store::read_upload_label(
        &root,
        BookRoot::CardRoot,
        alias.as_str(),
        alias.as_str(),
        &mut label
    ));
    assert_eq!(label.as_str(), "Middlemarch");
}

/// Every cluster of a file, walked through the driver.
fn chain_of(dir: &Dir<'_>, name: &str) -> Vec<u32> {
    let first = dir
        .find_directory_entry(name)
        .unwrap_or_else(|e| panic!("no entry {name:?}: {e:?}"))
        .cluster;
    let mut chain = vec![first.value()];
    let mut at = first;
    while let Some(next) = dir.next_cluster_in_chain(at).expect("walk") {
        chain.push(next.value());
        at = next;
    }
    chain
}

/// The rollback directory, opened from the root.
fn rollback_dir<'a>(root: &Dir<'a>) -> Dir<'a> {
    let cache_root = root
        .open_dir(proto::cache::CACHE_ROOT_DIR)
        .expect("cache root");
    cache_root.open_dir(ROLLBACK_DIR).expect("rollback dir")
}

/// A card walked to the point where only the predecessor's space is left to
/// reclaim, with the install record still standing.
///
/// Reached by applying the real steps rather than by writing a record that
/// looks like it: a synthetic one lets the planner take some other path, and
/// then the test proves nothing about the step it is named for.
fn card_awaiting_rollback_reclaim(disk: &SharedDisk) -> (Vec<u32>, Vec<u32>, ShortName) {
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let mut intent = intent(true);
    prepare(&root, &books, &mut intent);
    for step in [Step::RetireOldHolder, Step::InstallStage] {
        install::apply_step(&root, &books, &intent, step).expect("step");
    }
    let presence = install::observe(&root, &books, &intent).expect("observe");
    assert_eq!(
        install::plan(&intent, presence),
        Step::ReclaimRollback,
        "the fixture must actually reach the step it is named for",
    );
    // By long name: the 8.3 alias a move lands under is the driver's to
    // derive, which is the whole reason the install record names chains.
    let rollback = rollback_dir(&root);
    let parked_alias = holder_of(&rollback, intent.rollback.as_str())
        .expect("the predecessor is parked")
        .alias;
    let parked = chain_of(&rollback, parked_alias.as_str());
    let shelf = chain_of(
        &books,
        holder_of(&books, BOOK_NAME)
            .expect("installed")
            .alias
            .as_str(),
    );
    (parked, shelf, intent.rollback)
}

/// The digest the upload accumulated describes the file the shelf ended up
/// holding. Checked against an independent hash of the committed bytes.
#[test]
fn a_landing_carries_the_identity_of_the_bytes_that_landed() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    let landed = landing(&root, &books, BOOK_NAME, &old_body()).expect("the book landed");
    let on_card = body(&books, landed.alias.as_str());

    assert_eq!(landed.source, proto::source::digest_of(&on_card));
    assert_eq!(landed.source.byte_len(), on_card.len() as u64);
}

/// Chunking is the caller's business, and a socket cuts where it likes.
#[test]
fn the_identity_does_not_depend_on_how_the_body_was_chunked() {
    let whole = {
        let disk = new_card();
        let mgr = open_mgr(disk.clone());
        let (root, books) = open_dirs(&mgr);
        landing(&root, &books, BOOK_NAME, &old_body())
            .expect("landed")
            .source
    };

    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, None).expect("stage");
    for chunk in old_body().chunks(7) {
        staged.write(chunk).expect("stream");
    }
    let landed = staged
        .install(&root, &books, &mut words())
        .expect("install")
        .expect("landed");

    assert_eq!(landed.source, whole);
}

/// Asking whether two books share their bytes writes nothing.
///
/// The shelf stays readable while a reclaim is unsettled, which is when a
/// reader might ask, and an allocation then can be handed a cluster the
/// journal already recorded.
#[test]
fn an_equivalence_question_writes_nothing() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let one = landing(&root, &books, "One.epub", &old_body()).expect("one");
    let copy = landing(&root, &books, "Copy.epub", &old_body()).expect("copy");

    // Nothing has recorded a digest for these, so the tempting moment to
    // write one is exactly now.
    assert!(upload_store::cached_source_identity(&root, one.alias.as_str()).is_none());

    disk.cut_writes_from(None);
    let shared = upload_store::may_share_derived_state(
        &root,
        &FileKey::new(true, one.alias.as_str()).expect("alias fits"),
        &FileKey::new(true, copy.alias.as_str()).expect("alias fits"),
    )
    .expect("read");

    assert_eq!(shared, Some(true));
    assert_eq!(disk.writes_seen(), 0, "the question only read");
    assert!(
        upload_store::cached_source_identity(&root, one.alias.as_str()).is_none(),
        "and left no record behind",
    );
}

/// A delete between two questions is seen by the second.
///
/// Why nothing is remembered between calls: `remove_file_reclaiming_clusters`
/// mutates through whatever directory it is given, so a remembered digest
/// could outlive the file it described with nothing in the way.
#[test]
fn a_sibling_handle_cannot_leave_a_stale_trusted_entry() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let one = landing(&root, &books, "One.epub", &old_body()).expect("one");
    let copy = landing(&root, &books, "Copy.epub", &old_body()).expect("copy");

    assert_eq!(
        upload_store::source_identity(
            &root,
            &FileKey::new(true, one.alias.as_str()).expect("alias fits")
        )
        .expect("read"),
        Some(one.source),
    );

    // A delete that reaches the card through /BOOKS, needing no root.
    upload_store::remove_file_reclaiming_clusters(&books, one.alias.as_str());

    assert_eq!(
        upload_store::may_share_derived_state(
            &root,
            &FileKey::new(true, one.alias.as_str()).expect("alias fits"),
            &FileKey::new(true, copy.alias.as_str()).expect("alias fits"),
        )
        .expect("read"),
        None,
        "the first book is gone, so there is no equivalence to report",
    );
}

/// A file that is not there matches nothing, itself included: the question is
/// about bytes, and there are none.
#[test]
fn a_missing_book_matches_nothing_including_itself() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, _books) = open_dirs(&mgr);
    let missing = FileKey::new(true, "NOSUCH.EPU").expect("alias fits");

    assert_eq!(
        upload_store::may_share_derived_state(&root, &missing, &missing).expect("read"),
        None,
    );
}

/// The key opens the file, so a digest answers for the book the key names.
/// Asking about one book's key returns that book's bytes with another book
/// sitting right beside it.
#[test]
fn a_digest_is_filed_against_the_file_it_came_from() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let one = landing(&root, &books, "One.epub", &old_body()).expect("one");
    let other = landing(&root, &books, "Other.epub", &new_body()).expect("other");

    let key = FileKey::new(true, other.alias.as_str()).expect("alias fits");
    let digest = upload_store::source_identity(&root, &key)
        .expect("read")
        .expect("present");

    assert_eq!(digest, other.source);
    assert_ne!(
        digest, one.source,
        "asking about one book's key reads that book, with another sitting \
         beside it holding different bytes",
    );
}

/// Two copies of the same book may share what was derived from their
/// contents. Two different books may not.
#[test]
fn identical_copies_may_share_and_different_books_may_not() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let one = landing(&root, &books, "One.epub", &old_body()).expect("one");
    let copy = landing(&root, &books, "Copy.epub", &old_body()).expect("copy");
    let other = landing(&root, &books, "Other.epub", &new_body()).expect("other");

    let key = |alias: &str| FileKey::new(true, alias).expect("alias fits");
    assert_eq!(
        upload_store::may_share_derived_state(
            &root,
            &key(one.alias.as_str()),
            &key(copy.alias.as_str()),
        )
        .expect("read"),
        Some(true),
    );
    assert_eq!(
        upload_store::may_share_derived_state(
            &root,
            &key(one.alias.as_str()),
            &key(other.alias.as_str()),
        )
        .expect("read"),
        Some(false),
    );
}

/// Agreeing records are not evidence that two files hold the same bytes. The
/// fresh read in each question settles it.
#[test]
fn matching_records_do_not_authorize_sharing() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let one = landing(&root, &books, "One.epub", &old_body()).expect("one");
    let other = landing(&root, &books, "Other.epub", &new_body()).expect("other");

    // Somebody wrote a record for the second book claiming the first's bytes,
    // which is what a computer's delete plus an alias reuse leaves behind.
    upload_store::record_source_identity(&root, other.alias.as_str(), &one.source);
    let cached = upload_store::cached_source_identity(&root, other.alias.as_str())
        .expect("the misleading record is there");
    assert!(
        cached.agrees_with(&one.source),
        "the records agree, which is exactly the trap",
    );

    assert_eq!(
        upload_store::may_share_derived_state(
            &root,
            &FileKey::new(true, one.alias.as_str()).expect("alias fits"),
            &FileKey::new(true, other.alias.as_str()).expect("alias fits"),
        )
        .expect("read"),
        Some(false),
        "the bytes decide, not the records",
    );

    // The misleading record is still there, and still misleading. Repairing
    // it would mean writing, and this question is answerable while the card
    // is readable but must not be mutated.
    let untouched =
        upload_store::cached_source_identity(&root, other.alias.as_str()).expect("record");
    assert!(untouched.agrees_with(&one.source));

    // A caller that knows mutation is safe can repair it.
    upload_store::record_source_identity(&root, other.alias.as_str(), &other.source);
    let repaired =
        upload_store::cached_source_identity(&root, other.alias.as_str()).expect("record");
    assert!(repaired.agrees_with(&other.source));
}

/// A book that is not there is an absence, not an answer about equivalence.
#[test]
fn sharing_against_a_missing_book_has_no_answer() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let one = landing(&root, &books, BOOK_NAME, &old_body()).expect("one");

    assert_eq!(
        upload_store::may_share_derived_state(
            &root,
            &FileKey::new(true, one.alias.as_str()).expect("alias fits"),
            &FileKey::new(true, "NOSUCH.EPU").expect("alias fits"),
        )
        .expect("read"),
        None,
    );
}

/// A computed digest can be remembered, so the next question about the book
/// avoids another full read.
#[test]
fn a_recorded_identity_reads_back() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let landed = landing(&root, &books, BOOK_NAME, &old_body()).expect("landed");

    upload_store::record_source_identity(&root, landed.alias.as_str(), &landed.source);

    let cached = upload_store::cached_source_identity(&root, landed.alias.as_str())
        .expect("a record was written");
    assert!(
        cached.agrees_with(&landed.source),
        "the record describes the bytes that landed",
    );
}

/// Nothing recorded and a record this build cannot trust both mean the same
/// thing to a caller: read the file.
#[test]
fn an_unreadable_record_is_an_absence() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let landed = landing(&root, &books, BOOK_NAME, &old_body()).expect("landed");

    assert_eq!(
        upload_store::cached_source_identity(&root, landed.alias.as_str()),
        None,
        "nothing has been recorded yet, and no directory for one either",
    );

    // Record one so the directories exist, then put something else entirely
    // under the name a record would use.
    upload_store::record_source_identity(&root, landed.alias.as_str(), &landed.source);
    let cache_root = root.open_dir("READER").expect("cache root");
    let labels = cache_root.open_dir("LABELS").expect("labels");
    let stem = landed.alias.as_str().split('.').next().expect("stem");
    let mut name = String::<12>::new();
    name.push_str(stem).expect("stem");
    name.push_str(".SRC").expect("ext");
    let file = labels
        .open_file_in_dir(name.as_str(), Mode::ReadWriteCreateOrTruncate)
        .expect("open over the record just written");
    file.write(b"not a record at all").expect("write");
    file.close().expect("close");

    assert_eq!(
        upload_store::cached_source_identity(&root, landed.alias.as_str()),
        None,
    );
}

/// The record goes when the book does, since a freed alias goes to whatever
/// is written next.
#[test]
fn deleting_a_book_takes_its_recorded_identity_with_it() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let landed = landing(&root, &books, BOOK_NAME, &old_body()).expect("landed");
    upload_store::record_source_identity(&root, landed.alias.as_str(), &landed.source);

    upload_store::delete_upload_sidecars(&root, landed.alias.as_str());

    assert_eq!(
        upload_store::cached_source_identity(&root, landed.alias.as_str()),
        None,
    );
}

/// A replacement retires the predecessor's sidecars along with it.
#[test]
fn replacing_a_book_does_not_leave_the_old_record_behind() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let first = landing(&root, &books, BOOK_NAME, &old_body()).expect("first");
    upload_store::record_source_identity(&root, first.alias.as_str(), &first.source);

    let second = landing(&root, &books, BOOK_NAME, &new_body()).expect("replacement");
    upload_store::record_source_identity(&root, second.alias.as_str(), &second.source);

    let cached = upload_store::cached_source_identity(&root, second.alias.as_str())
        .expect("a record was written");
    assert!(cached.agrees_with(&second.source));
    assert_eq!(
        second.alias, first.alias,
        "the predecessor is retired, so its alias is derived again for the \
         replacement",
    );
}

/// A sideloaded book has an identity too, read out of it rather than
/// accumulated while it was written.
#[test]
fn a_sideloaded_book_can_be_hashed_on_demand() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (_root, books) = open_dirs(&mgr);

    // Written directly, the way a computer would leave it.
    let file = books
        .create_file_in_dir_lfn(BOOK_NAME)
        .expect("sideloaded book");
    file.write(&old_body()).expect("write");
    file.close().expect("close");

    let alias = holder_of(&books, BOOK_NAME).expect("shelved").alias;
    let digest = upload_store::digest_of_file(&books, alias.as_str())
        .expect("read")
        .expect("the book is there");

    assert_eq!(digest, proto::source::digest_of(&old_body()));
    assert_eq!(digest.byte_len(), old_body().len() as u64);
}

/// The same file hashes the same whether its bytes arrived through the
/// upload path or were read back off the card afterwards.
#[test]
fn reading_a_book_back_agrees_with_the_upload_that_wrote_it() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    let landed = landing(&root, &books, BOOK_NAME, &old_body()).expect("landed");
    let reread = upload_store::digest_of_file(&books, landed.alias.as_str())
        .expect("read")
        .expect("the book is there");

    assert_eq!(reread, landed.source);
}

/// A name that is not there is an absence, not a failure. Callers ask about
/// books the catalog listed, and a book can go between the listing and the
/// question.
#[test]
fn hashing_a_missing_book_is_an_absence() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (_root, books) = open_dirs(&mgr);

    assert!(upload_store::digest_of_file(&books, "NOSUCH.EPU")
        .expect("read")
        .is_none());
}

/// An empty file is a legitimate zero-length identity rather than an error,
/// so the caller decides what an empty book means.
#[test]
fn an_empty_file_hashes_to_the_empty_identity() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (_root, books) = open_dirs(&mgr);

    let file = books.create_file_in_dir_lfn("Empty.epub").expect("create");
    file.close().expect("close");
    let alias = holder_of(&books, "Empty.epub").expect("shelved").alias;

    let digest = upload_store::digest_of_file(&books, alias.as_str())
        .expect("read")
        .expect("present");
    assert_eq!(digest, proto::source::digest_of(&[]));
    assert_eq!(digest.byte_len(), 0);
}

/// A replacement carries the identity of what replaced, not of what was
/// there. The first landing's digest describes bytes no reader can reach any
/// more.
#[test]
fn a_replacement_carries_the_identity_of_the_new_bytes() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    let first = landing(&root, &books, BOOK_NAME, &old_body()).expect("first landed");
    let second = landing(&root, &books, BOOK_NAME, &new_body()).expect("replacement landed");

    assert_eq!(second.source, proto::source::digest_of(&new_body()));
    assert_ne!(second.source, first.source);
    assert_eq!(body(&books, second.alias.as_str()), new_body());
}

/// The check that decides whether an identity is published: a staged upload,
/// a standing intent, and a destination on none of this transaction's chains.
///
/// Reached through `observe` rather than `install`, because `install` records
/// whichever file holds the name at that moment as the predecessor, so a
/// stranger arriving mid-upload is replaced rather than refused.
#[test]
fn a_stranger_holding_the_destination_is_not_a_landing() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let mut intent = intent(false);
    prepare(&root, &books, &mut intent);

    let intruder = books
        .create_file_in_dir_lfn(BOOK_NAME)
        .expect("somebody else's book");
    intruder.write(b"not ours").expect("write");
    intruder.close().expect("close");

    let presence = install::observe(&root, &books, &intent).expect("observe");
    assert!(
        !presence.dest,
        "the destination is not on the upload's chain"
    );
    assert!(presence.foreign, "and it belongs to somebody else");
    assert_eq!(
        install::plan(&intent, presence),
        Step::Done,
        "nothing of this transaction is off the shelf, so it gives up",
    );
}

/// The alias handed back is the one on the chain the install verified, so the
/// digest describes the file that was checked rather than whatever answers to
/// the name afterwards.
#[test]
fn a_landing_names_the_file_on_the_verified_chain() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);

    let landed = landing(&root, &books, BOOK_NAME, &old_body()).expect("landed");
    let holder = holder_of(&books, BOOK_NAME).expect("shelved");

    assert_eq!(landed.alias, holder.alias);
    assert_eq!(
        chain_of(&books, holder.alias.as_str()).first().copied(),
        Some(holder.chain),
        "and that alias is the chain the shelf entry points at",
    );
    assert_eq!(landed.source, proto::source::digest_of(&old_body()));
}

/// A rollback has no landing, so nothing is published and the shelf still
/// holds the predecessor's bytes.
#[test]
fn a_rollback_publishes_no_identity_and_leaves_the_old_bytes() {
    let disk = new_card();
    let old_identity = {
        let mgr = open_mgr(disk.clone());
        let (root, books) = open_dirs(&mgr);
        let landed = landing(&root, &books, BOOK_NAME, &old_body()).expect("first landed");
        assert_eq!(landed.source, proto::source::digest_of(&old_body()));
        landed.source
    };

    // Cut the replacement before its intent is durable: the transaction
    // finishes, installs nothing, and reports no landing.
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, None).expect("stage");
    staged.write(&new_body()).expect("stream");
    staged.abandon(&root);

    let alias = holder_of(&books, BOOK_NAME).expect("still shelved").alias;
    let on_card = body(&books, alias.as_str());
    assert_eq!(
        proto::source::digest_of(&on_card),
        old_identity,
        "the predecessor's identity still describes what the shelf holds",
    );
    assert_ne!(on_card, new_body(), "the replacement did not land");
}

/// The digest lives on the staged upload and dies with it, so an interrupted
/// upload leaves nothing describing the book that is still there.
#[test]
fn an_interrupted_upload_attaches_no_identity_to_the_old_landing() {
    let disk = new_card();
    let mgr = open_mgr(disk.clone());
    let (root, books) = open_dirs(&mgr);
    let first = landing(&root, &books, BOOK_NAME, &old_body()).expect("first landed");

    // Stream the replacement, then lose the session before install runs.
    let mut staged = StagedUpload::begin(&root, &books, BOOK_NAME, None).expect("stage");
    staged.write(&new_body()).expect("stream");
    drop(staged);

    // Recovery has no record to replay, and the shelf is untouched.
    assert!(recover_installs(&root, &books).complete);
    let alias = holder_of(&books, BOOK_NAME).expect("still shelved").alias;
    assert_eq!(alias, first.alias);
    assert_eq!(
        proto::source::digest_of(&body(&books, alias.as_str())),
        first.source,
    );
}

#[test]
fn the_two_journals_hand_off_across_a_cut_at_any_write() {
    // The install journal has to stay sufficient while the reclaim journal
    // temporarily owns the cleanup. Neither is updated in the same breath as
    // the other, and the claim is that it does not need to be: whatever the
    // cut interrupts, replaying reclaim before installs converges.
    //
    // Cut before every write the handoff makes, reboot, run the pair in the
    // order every firmware entry point uses, and require that the reclaim
    // really ran at least once across the sweep -- the parked copy going
    // away is also reachable by the leftover sweep, which unlinks without
    // freeing when the shelf shares the chain, so absence alone proves
    // nothing about which path did it.
    let writes = {
        let probe = new_card();
        card_awaiting_rollback_reclaim(&probe);
        let mgr = open_mgr(probe.clone());
        let (root, books) = open_dirs(&mgr);
        // Clearing the fault also zeroes the counter, so what follows is
        // counted from nothing.
        probe.cut_writes_from(None);
        assert!(recover_installs(&root, &books).complete);
        probe.writes_seen()
    };
    assert!(
        writes > 2,
        "the handoff should take several writes, took {writes}"
    );

    let mut journals_overlapped = 0;
    for cut in 1..=writes + 1 {
        let disk = new_card();
        let (parked, shelf, rollback_name) = card_awaiting_rollback_reclaim(&disk);

        // The cut, inside install recovery's handoff.
        {
            let mgr = open_mgr(disk.clone());
            let (root, books) = open_dirs(&mgr);
            disk.cut_writes_from(Some(cut));
            let _ = recover_installs(&root, &books);
        }
        disk.cut_writes_from(None);

        // Did the two journals actually overlap? This is the evidence that
        // the handoff happened, as opposed to the predecessor having gone by
        // some other route.
        //
        // Both halves are needed. A live reclaim record alone would still be
        // satisfied by a version that cleared the install record first and
        // then reclaimed -- the parked copy would vanish, the clusters would
        // come back, and nothing here would notice that the install journal
        // stopped covering the work before the reclaim journal started. The
        // claim this test exists for is that INSTALL.JNL stays sufficient
        // while RECLAIM.JNL temporarily owns the cleanup, so the install
        // record must still be standing at the moment the reclaim one is.
        {
            let mgr = open_mgr(disk.clone());
            let (root, _books) = open_dirs(&mgr);
            if let Ok(reclaim::Journal::Found(live)) = reclaim::read_journal(&root) {
                if matches!(live.slot, reclaim::Slot::Work(_)) {
                    assert!(
                        matches!(
                            install::read_intent(&root),
                            Ok(install::IntentState::Valid(_))
                        ),
                        "cut at {cut}: a reclaim was live with no install record behind \
                         it -- the handoff stopped overlapping",
                    );
                    journals_overlapped += 1;
                }
            }
        }

        // The reboot: both journals, in the order every entry point uses.
        let mgr = open_mgr(disk.clone());
        let (root, books) = open_dirs(&mgr);
        reclaim::recover(&root, Some(&books))
            .unwrap_or_else(|e| panic!("cut at {cut}: reclaim would not settle: {e:?}"));
        let outcome = recover_installs(&root, &books);

        // The reader's book is untouched throughout.
        let installed = holder_of(&books, BOOK_NAME).expect("the book is still on the shelf");
        assert_eq!(
            chain_of(&books, installed.alias.as_str()),
            shelf,
            "cut at {cut}: the installed book changed under the handoff",
        );

        // The predecessor is gone, and its space came back.
        let parked_gone = holder_of(&rollback_dir(&root), rollback_name.as_str()).is_none();
        assert!(parked_gone, "cut at {cut}: the parked copy is still there");
        assert!(outcome.complete, "cut at {cut}: the install did not finish");
        drop(books);
        drop(root);
        drop(mgr);
        assert!(
            disk.still_allocated(&parked).is_empty(),
            "cut at {cut}: the parked copy's clusters were never reclaimed",
        );
    }

    assert!(
        journals_overlapped > 0,
        "no cut caught both journals live at once, so the handoff itself was never \
         exercised -- only its end state",
    );
}

/// A word source for minting ids in these tests: distinct, and not random,
/// since nothing here asserts on the ids themselves.
///
/// Distinct across call sites, which is the part that took a fix. Every
/// generator started from one seed, so two uploads in a test minted the same
/// id and the ledger held a duplicate no production path can produce.
fn words() -> impl FnMut() -> u32 {
    static SEED: AtomicU32 = AtomicU32::new(0x2545_F491);
    let mut state = SEED.fetch_add(0x9E37_79B9, Ordering::Relaxed);
    move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state
    }
}
