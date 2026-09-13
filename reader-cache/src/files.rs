use crate::layout;
use crate::store::{
    ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS, MAX_SD_TOC_ITEMS,
    MAX_SD_TOC_TEXT_BYTES,
};
use core::ops::ControlFlow;
use display::font::FontStyle;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use heapless::String;
use proto::cache::{
    decode_block, decode_book_v2_header, decode_book_v2_section, decode_cover_header, decode_page,
    decode_section_v2_header, decode_toc, decode_toc_chapter, decode_toc_file_header, encode_block,
    encode_book_v2_header, encode_book_v2_section, encode_content_header,
    encode_content_record_header, encode_page, encode_section_v2_header, encode_toc,
    encode_toc_file_header, section_file_name, BookV2Header, BookV2SectionRecord, ContentHeader,
    ContentRecordHeader, SectionV2Header, TocFileHeader, BLOCK_RECORD_BYTES, BOOK_V2_HEADER_BYTES,
    BOOK_V2_SECTION_RECORD_BYTES, CACHE_BOOK_FILE, CACHE_CONTENT_FILE, CACHE_COVER_FILE,
    CACHE_ROOT_DIR, CACHE_SECTIONS_DIR, CACHE_SECTION_FILE_BYTES, CACHE_STATE_FILE, CACHE_TOC_FILE,
    CACHE_V2_DIR, CONTENT_HEADER_BYTES, CONTENT_RECORD_HEADER_BYTES, COVER_HEADER_BYTES,
    PAGE_RECORD_BYTES, SECTION_V2_HEADER_BYTES, TOC_CHAPTER_RECORD_BYTES, TOC_FILE_HEADER_BYTES,
    TOC_RECORD_BYTES,
};
use proto::font_pack::{
    decode_font_pack_name, FontPackFaceRecord, FontPackHeader, FONT_PACK_DIR,
    FONT_PACK_FACE_RECORD_BYTES, FONT_PACK_FILE, FONT_PACK_HEADER_BYTES,
};

use proto::durable::{
    decode_durable_record, encode_durable_record, generation_is_newer, DURABLE_MAX_BYTES,
    DURABLE_OVERHEAD,
};

/// Read and validate one durable generation file; `payload` receives the
/// record body and the valid record's generation is returned. Any missing,
/// short, oversized, or corrupt file reads as `None`.
fn read_generation_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
    magic: [u8; 4],
    payload: &mut [u8],
) -> Option<u32>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let total = payload.len().checked_add(DURABLE_OVERHEAD)?;
    if total > DURABLE_MAX_BYTES {
        return None;
    }
    let file = directory.open_file_in_dir(name, Mode::ReadOnly).ok()?;
    if file.length() as usize != total {
        return None;
    }
    let mut bytes = [0u8; DURABLE_MAX_BYTES];
    read_exact_file(&file, &mut bytes[..total]).ok()?;
    decode_durable_record(magic, &bytes[..total], payload)
}

/// Read the newest valid generation out of an A/B file pair into `payload`.
/// False means neither side holds a valid record.
fn read_two_generation<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    names: [&str; 2],
    magic: [u8; 4],
    payload: &mut [u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut other = [0u8; DURABLE_MAX_BYTES];
    let a = read_generation_file(directory, names[0], magic, payload);
    let b = read_generation_file(directory, names[1], magic, &mut other[..payload.len()]);
    match (a, b) {
        (None, None) => false,
        (Some(_), None) => true,
        (None, Some(_)) => {
            payload.copy_from_slice(&other[..payload.len()]);
            true
        }
        (Some(a), Some(b)) if generation_is_newer(b, a) => {
            payload.copy_from_slice(&other[..payload.len()]);
            true
        }
        (Some(_), Some(_)) => true,
    }
}

/// Write `payload` as the next generation of an A/B file pair, overwriting
/// the *older* side so the newest survivor is never the one mid-write, then
/// prove the write by re-reading it through the validating read path.
fn write_two_generation<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    names: [&str; 2],
    magic: [u8; 4],
    payload: &[u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut scratch = [0u8; DURABLE_MAX_BYTES];
    let a = read_generation_file(directory, names[0], magic, &mut scratch[..payload.len()]);
    let b = read_generation_file(directory, names[1], magic, &mut scratch[..payload.len()]);
    let (target, generation) = match (a, b) {
        (Some(a), Some(b)) if generation_is_newer(b, a) => (0, b.wrapping_add(1)),
        (Some(a), Some(_)) => (1, a.wrapping_add(1)),
        (Some(a), None) => (1, a.wrapping_add(1)),
        (None, Some(b)) => (0, b.wrapping_add(1)),
        (None, None) => (0, 1),
    };
    let mut record = [0u8; DURABLE_MAX_BYTES];
    let total = encode_durable_record(magic, generation, payload, &mut record)?;
    {
        let file = directory
            .open_file_in_dir(names[target], Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| ())?;
        file.write(&record[..total]).map_err(|_| ())?;
    }
    let mut verify = [0u8; DURABLE_MAX_BYTES];
    let verified = read_generation_file(
        directory,
        names[target],
        magic,
        &mut verify[..payload.len()],
    );
    if verified == Some(generation) && &verify[..payload.len()] == payload {
        Ok(())
    } else {
        Err(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomFontManifest {
    pub name: heapless::String<{ crate::store::MAX_CUSTOM_FONT_NAME }>,
    pub identity: u64,
    pub faces: [FontPackFaceRecord; crate::store::MAX_CUSTOM_FONT_FACES],
    pub face_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheLoadResult {
    Hit { pages: usize, repaginated: bool },
    Miss,
    Invalid,
    TooShort { pages: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookIndexLoadResult {
    Hit {
        /// The index was published mid-build and a walk meant to come back for
        /// the rest. Whether that walk still exists is the caller's to know;
        /// see `app_core::storage_loop::partial_index_is_usable`.
        unfinished: bool,
    },
    Miss,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoverLoadResult {
    Hit,
    Miss,
    Invalid,
}

#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn ensure_v2_cache_dirs<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let book = claim_v2_book_dir(root, owner).map_err(|_| ())?;
    let _ = open_or_make_dir(&book, CACHE_SECTIONS_DIR)?;
    Ok(())
}

const POSITION_FILE: &str = "POS.BIN";
const POSITION_GENERATIONS: [&str; 2] = ["POSA.BIN", "POSB.BIN"];
/// MarigoldOS v0.4.x durable-position magic; keep byte-identical so cards
/// carry reading positions between the two firmwares.
const POSITION_DURABLE_MAGIC: [u8; 4] = *b"MGPS";
const POSITION_BYTES: usize = proto::nvm::PositionRecord::ENCODED_LEN;

/// Panel-geometry salt mixed into the position checksum. The stored screen
/// is a page-within-chapter index under this panel's pagination, so a
/// position written on a differently sized panel (an SD card moved between
/// an X4 and an X3) is meaningless. Zero on the X4 keeps every existing
/// POS.BIN validating byte-for-byte; the X3's non-zero salt makes an
/// X4-written record fail the checksum, so the reader resumes at book start
/// rather than a stale page. The chapter would survive, but there is no
/// separate progress source to reconcile it against, so full reset is the
/// honest fallback.
const POSITION_GEOMETRY_SALT: u32 = (display::WIDTH as u32 ^ display::HEIGHT as u32) ^ (800 ^ 480);

// The salt must vanish on the X4 or an upgrade would reject every existing
// POS.BIN; guard the backward-compat guarantee at compile time.
#[cfg(not(feature = "device-x3"))]
const _: () = assert!(POSITION_GEOMETRY_SALT == 0);

fn encode_position(chapter: u16, screen: u32) -> [u8; POSITION_BYTES] {
    proto::nvm::PositionRecord { chapter, screen }.encode(POSITION_GEOMETRY_SALT)
}

fn decode_position(bytes: &[u8]) -> Option<(u16, u32)> {
    proto::nvm::PositionRecord::decode(bytes, POSITION_GEOMETRY_SALT)
        .map(|record| (record.chapter, record.screen))
}

/// Per-book reading position beside the book's cache records, so
/// switching books does not abandon the previous one's place.
///
/// The error distinguishes a card fault, which is retryable and worth
/// failing a transaction over, from a foreign active claim, which is
/// deliberate and durable: while a full-hash twin holds the key this book's
/// position is unpersistable by design, and blocking every book switch on
/// that would trade a lost place for a stuck reader.
pub fn write_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    chapter: u16,
    screen: u32,
) -> Result<(), ClaimDenied>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Claims: a position write may be the first thing to create the
    // directory, and a full-hash twin's directory must not take another
    // book's place in it.
    let book = claim_v2_book_dir(root, owner)?;
    write_two_generation(
        &book,
        POSITION_GENERATIONS,
        POSITION_DURABLE_MAGIC,
        &encode_position(chapter, screen),
    )
    .map_err(|_| ClaimDenied::Fault)
}

/// Record what a copy is, beside the claim that says whose directory it is.
///
/// Takes a freshly hashed digest for the reason [`carry_position`] does: a
/// writer that accepted the cached form could persist a claim about bytes
/// nobody read.
///
/// Merged rather than replaced, so an absence cannot erase a fact: a witness
/// that read the bytes but not the directory entry has a digest and no
/// chain, and an older claim has neither and may still learn one.
///
/// Called once per copy by the background reading of a book's bytes, which
/// is where a book nobody uploaded gets a digest at all.
pub fn record_cache_evidence<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    cluster: Option<u32>,
    digest: Option<proto::source::SourceDigest>,
) -> Result<(), ClaimDenied>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let book = claim_v2_book_dir(root, owner)?;
    // A failed read is refused rather than merged over: defaulting it to
    // empty would let one transient fault erase the half of the evidence
    // this call is not the one supplying.
    let held = match read_stored_claim(&book) {
        StoredClaim::Present { evidence, .. } => evidence,
        StoredClaim::Absent => proto::cache::CacheEvidence::default(),
        StoredClaim::Fault => return Err(ClaimDenied::Fault),
    };
    let merged = proto::cache::CacheEvidence {
        cluster: cluster.or(held.cluster),
        // Downgraded on the way onto the card, which is the only direction
        // the fence allows: what comes back off a card is evidence, and what
        // goes onto one has to have been read from the bytes.
        digest: digest
            .map(proto::source::CachedSourceDigest::new)
            .or(held.digest),
    };
    if merged == held {
        return Ok(());
    }
    write_book_dir_claim(&book, owner, false, Some(&merged)).map_err(|_| ClaimDenied::Fault)
}

/// Carry a reading position from the key a book used to have to the key it
/// has now.
///
/// Takes a [`proto::source::SourceDigest`] rather than the cached form so
/// the caller must have read the destination's bytes. The cached type comes
/// off a claim and describes a file that may since have changed, so
/// accepting it would let a caller pass the departed book's own stored
/// digest and carry a place onto a candidate nobody hashed.
///
/// Position only. Pagination is keyed on the locator too, so a move
/// invalidates it whatever happens here, and rebuilding is cheaper than
/// shuttling section files during a scan the reader is waiting on.
///
/// The claim lands before the position, because a position in a directory
/// whose claim did not land cannot be read back. Nothing here deletes the
/// old copy, so an interruption leaves the place where it was.
///
/// `Ok(false)` is a departed directory with no position to carry.
///
/// Reached from the firmware through [`carry_position_for_move`], on a move
/// the scan has proved. Nothing else may conclude that a book moved.
pub fn carry_position<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    from: &proto::cache::CacheOwner<'_>,
    to: &proto::cache::CacheOwner<'_>,
    confirmed: proto::source::SourceDigest,
    cluster: Option<u32>,
) -> Result<bool, ClaimDenied>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let evidence = &proto::cache::CacheEvidence {
        cluster,
        digest: Some(proto::source::CachedSourceDigest::new(confirmed)),
    };
    let Some((chapter, screen)) = read_position_file(root, from) else {
        return Ok(false);
    };
    if from.key == to.key {
        // The book moved and its key did not follow. Keys are 28 bits of a
        // hash of the place, so two locators can share one, and then the
        // place the reader left is already in the directory the moved book
        // will use. Nothing is carried. What has to change is whose
        // directory it is, because the claim still names a locator that is
        // gone, and the ordinary adoption path reads a claim naming another
        // locator as another book's and clears the positions under it.
        //
        // Re-attributing without going through the claim gate is the whole
        // point: the gate would refuse, since by its reading this is a
        // stranger. The digest is what says otherwise, and having one means
        // somebody read these bytes and found the departed owner's witness
        // in them.
        //
        // It rewrites the one thing a departed copy's evidence lives in, so
        // a caller whose durable record of the move has not landed yet must
        // not reach here: what it would take away is what the retry reads.
        // See [`carry_position_for_move`], which declines this case for
        // exactly that reason.
        let mut book = root
            .open_dir(CACHE_ROOT_DIR)
            .map_err(|_| ClaimDenied::Fault)?;
        book.change_dir(CACHE_V2_DIR)
            .map_err(|_| ClaimDenied::Fault)?;
        book.change_dir(to.key).map_err(|_| ClaimDenied::Fault)?;
        write_book_dir_claim(&book, to, false, Some(evidence)).map_err(|_| ClaimDenied::Fault)?;
        return Ok(true);
    }
    let book = claim_v2_book_dir(root, to)?;
    write_book_dir_claim(&book, to, false, Some(evidence)).map_err(|_| ClaimDenied::Fault)?;
    write_two_generation(
        &book,
        POSITION_GENERATIONS,
        POSITION_DURABLE_MAGIC,
        &encode_position(chapter, screen),
    )
    .map_err(|_| ClaimDenied::Fault)?;
    Ok(true)
}

/// What a book's own claim records of its bytes, when it records anything.
///
/// For a caller deciding whether reading the book again would tell the card
/// something it does not already know. `None` covers every reason not to
/// bother distinguishing: no directory, no claim, another book's claim, or
/// one written before claims carried evidence.
pub fn recorded_evidence<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) -> Option<proto::cache::CacheEvidence>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = cache_root.open_dir(CACHE_V2_DIR).ok()?;
    let book = cache.open_dir(owner.key).ok()?;
    match book_dir_claim(&book, owner) {
        ClaimState::MineActive | ClaimState::MineReleased => match read_stored_claim(&book) {
            StoredClaim::Present { evidence, .. } => Some(evidence),
            _ => None,
        },
        _ => None,
    }
}

/// Carry a reading position to where a copy has been found again.
///
/// The place a reader left is filed under where the copy used to be, so a
/// repaired locator on its own would leave it behind. The scan proves the
/// move by the bytes; this reads the destination again rather than taking
/// that word for it, because what it writes into the claim is a statement
/// about the bytes at the new place and the only way to make one is to read
/// them.
///
/// `Ok(false)` is a copy that had no place to carry, which is most of a
/// library, or one whose two places share a cache directory. That last is
/// declined rather than carried: with one directory between them the carry
/// would have to re-attribute it, which rewrites the claim the copy's
/// bytes are recorded in, and this runs before the ledger has written the
/// move down. A reset in that window would leave a ledger naming the old
/// place and a claim naming the new one, so the retry the ordering exists
/// for would find no evidence and mint the copy afresh. The reader's place
/// is lost in that case, which the bridge already allows for, and the
/// copy's identity is not, which it does not. Two locators share a key
/// once in a few hundred million.
pub fn carry_position_for_move<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    was: &proto::cache::CacheOwner<'_>,
    now: &proto::cache::CacheOwner<'_>,
) -> Result<bool, ClaimDenied>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if was.key == now.key {
        return Ok(false);
    }
    let path =
        proto::library_path::LibraryPath::parse(now.locator).map_err(|_| ClaimDenied::Fault)?;
    let read = upload_store::library::with_book_at(root, now.root, &path, |dir, alias| {
        let mut name = heapless::String::<12>::new();
        use core::fmt::Write as _;
        if write!(name, "{}", alias).is_err() {
            return None;
        }
        upload_store::digest_of_file(dir, name.as_str())
            .ok()
            .flatten()
    })
    .map_err(|_| ClaimDenied::Fault)?
    .flatten();
    let Some(digest) = read else {
        return Err(ClaimDenied::Fault);
    };
    carry_position(root, was, now, digest, None)
}

/// The position files inside an open book directory.
fn read_position_in<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<(u16, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut bytes = [0u8; POSITION_BYTES];
    if read_two_generation(
        book,
        POSITION_GENERATIONS,
        POSITION_DURABLE_MAGIC,
        &mut bytes,
    ) {
        return decode_position(&bytes);
    }
    // Legacy single-file fallback, kept readable so an upgrade resumes at
    // the pre-durable position; the next write lands on the A/B pair.
    let file = book.open_file_in_dir(POSITION_FILE, Mode::ReadOnly).ok()?;
    let len = file.read(&mut bytes).ok()?;
    decode_position(&bytes[..len])
}

pub fn read_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) -> Option<(u16, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = cache_root.open_dir(CACHE_V2_DIR).ok()?;
    let book = cache.open_dir(owner.key).ok()?;
    // A directory a twin holds is not this book's, whatever it contains. An
    // unclaimed one is readable: no other book has asserted over it, so the
    // read is as safe as it ever was, and the next write claims it.
    match book_dir_claim(&book, owner) {
        ClaimState::MineActive | ClaimState::MineReleased => read_position_in(&book),
        // Anything else is not provably this book's place. Pre-claim
        // positions have their own explicit path: the legacy-key fallback,
        // whose directories no claim-aware firmware ever wrote.
        _ => None,
    }
}

/// Cache directory names held in RAM at once. The whole listing does not
/// fit: one name per opened book, against a frame that has room for a few
/// dozen.
pub const CACHE_SWEEP_BATCH: usize = 48;

/// Hand a listing to `on_batch` a batch at a time, until it runs out.
///
/// The batching rule over a listing it cannot read, so it can be walked at
/// any size without a card. `collect` fills a batch from a cursor;
/// `survives` says whether a name handed over is still listed.
///
/// The cursor counts entries that survived, not entries handed over: a
/// caller that removes one shortens the listing behind it, and counting
/// what it asked about would step over whatever moved up.
///
/// No cap on batches. Each batch either moves the cursor or shortens the
/// listing, so it ends, given a caller whose additions are finite. A
/// listing that reports an entry gone while still listing it would hand the
/// same names over forever; a batch that neither moved the cursor nor
/// changed its first name is that, and stops.
pub fn walk_in_batches(
    batch: usize,
    mut collect: impl FnMut(usize, usize, &mut heapless::Vec<heapless::String<8>, CACHE_SWEEP_BATCH>),
    mut survives: impl FnMut(&str) -> bool,
    mut on_batch: impl FnMut(&[heapless::String<8>]),
) {
    let batch = batch.clamp(1, CACHE_SWEEP_BATCH);
    let mut handled = 0usize;
    let mut previous_first: Option<heapless::String<8>> = None;
    loop {
        let mut keys: heapless::Vec<heapless::String<8>, CACHE_SWEEP_BATCH> = heapless::Vec::new();
        collect(handled, batch, &mut keys);
        if keys.is_empty() {
            return;
        }
        let full = keys.len() == batch;
        let first = keys[0].clone();
        on_batch(&keys);
        let before = handled;
        handled += keys.iter().filter(|key| survives(key.as_str())).count();
        // Neither moved on, nor moved up.
        if handled == before && previous_first.as_ref() == Some(&first) {
            return;
        }
        previous_first = Some(first);
        if !full {
            return;
        }
    }
}

/// Hand every cache directory name to `on_batch`, a batch at a time.
///
/// The batching rule is [`walk_in_batches`]; this supplies the card.
/// Enumeration restarts from the top for each batch, because embedded-sdmmc
/// forbids opening files while a directory iteration holds the lock and the
/// caller's whole job is opening files.
///
/// `batch` is how many names to hold at once, capped at
/// [`CACHE_SWEEP_BATCH`]. It is a parameter so the rule can be walked at a
/// size that makes the number of batches worth counting.
pub fn for_each_cache_dir<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    batch: usize,
    on_batch: impl FnMut(&[heapless::String<8>]),
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    walk_in_batches(
        batch,
        |skip, want, keys| {
            let Ok(cache_root) = root.open_dir(CACHE_ROOT_DIR) else {
                return;
            };
            let Ok(cache) = cache_root.open_dir(CACHE_V2_DIR) else {
                return;
            };
            let mut seen = 0usize;
            let _ = cache.iterate_dir(|entry| {
                if !entry.attributes.is_directory() {
                    return core::ops::ControlFlow::Continue(());
                }
                let mut name = heapless::String::<8>::new();
                // A name that does not fit would be truncated into a
                // *different* key, and everything downstream would act on
                // whatever that names.
                if write!(name, "{}", entry.name).is_err() {
                    return core::ops::ControlFlow::Continue(());
                }
                if name.is_empty() || name.as_str() == "." || name.as_str() == ".." {
                    return core::ops::ControlFlow::Continue(());
                }
                seen += 1;
                if seen <= skip {
                    return core::ops::ControlFlow::Continue(());
                }
                if keys.push(name).is_err() || keys.len() == want {
                    return core::ops::ControlFlow::Break(());
                }
                core::ops::ControlFlow::Continue(())
            });
        },
        |key| book_dir_exists(root, key),
        on_batch,
    );
}

/// Whether a book's cache directory is still on the card.
///
/// For a caller walking `CACHE2` in batches: a pass that reclaims a
/// directory shortens the listing behind it, and a cursor counting entries
/// has to know by how much or it steps over the ones it has not reached.
pub fn book_dir_exists<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(cache_root) = root.open_dir(CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(cache) = cache_root.open_dir(CACHE_V2_DIR) else {
        return false;
    };
    cache.open_dir(key).is_ok()
}

/// What a cache directory has to say about holding a reading position.
///
/// Three-valued for the reason every other read here is: a carry that reads
/// "no position" acts by writing one, and the directory it writes into is
/// adopted, which deletes what was already there. Absence and a card that
/// stumbled have to be different answers, or one transient read turns a
/// guard against destroying a place into the thing that destroys it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionPresence {
    /// No position files. A carry may write here.
    Absent,
    /// Position files are there. Whether they decode is not the question:
    /// a carry overwrites the bytes either way, so their existence is what
    /// makes this somewhere not to write.
    Present,
    /// The card would not say. Not an answer, and not a licence to write.
    Unreadable,
}

/// Whether a claimed cache directory's owner still exists on the card and
/// still keys to this directory. The file itself is asked, not the catalog:
/// the catalog's 32-bit identities cannot tell a twin from the owner, and
/// the sweep exists to be exact where the hashes cannot be. A book whose
/// size changed keys elsewhere now, so its old directory reads as dead here
/// and retires like any other.
pub fn claimant_place<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    at: proto::library_path::BookRoot,
    locator: &str,
    key: &str,
) -> ClaimantPlace
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // A locator this build cannot parse is one it cannot ever resolve, so
    // waiting for a better answer would keep the directory forever.
    let Ok(path) = proto::library_path::LibraryPath::parse(locator) else {
        return ClaimantPlace::Gone;
    };
    let probed = upload_store::library::with_book_at(root, at, &path, |dir, alias| {
        match dir.open_file_in_dir(alias, Mode::ReadOnly) {
            Ok(file) => FileProbe::Length(file.length()),
            Err(embedded_sdmmc::Error::NotFound) => FileProbe::Absent,
            Err(_) => FileProbe::Unreadable,
        }
    });
    let size = match probed {
        // A component of the path is missing or is not the kind of thing the
        // locator says. The walk answered.
        Ok(None) => return ClaimantPlace::Gone,
        Ok(Some(FileProbe::Absent)) => return ClaimantPlace::Gone,
        Ok(Some(FileProbe::Unreadable)) | Err(_) => return ClaimantPlace::Unreadable,
        Ok(Some(FileProbe::Length(size))) => size,
    };
    let current = proto::cache::cache_key_from(proto::cache::source_hash_at(at, locator, size));
    if current.as_str() == key {
        ClaimantPlace::Live
    } else {
        // The file is there and keys elsewhere now, which is a change the
        // card stated plainly: its size is not what it was.
        ClaimantPlace::Gone
    }
}

/// What one probe of a book file found.
enum FileProbe {
    Length(u32),
    Absent,
    Unreadable,
}

/// Whether a claim's owner is still where the claim says, still with the
/// size that keys it here.
///
/// Three-valued for the reason the claim reads are, and this one is runtime
/// behaviour: the answer decides whether the sweep retires a claim, which
/// ends a book's cache and gives up the place it was keeping. A card that
/// stumbled while opening a book sitting right there would otherwise read as
/// a book that left, and the book would lose its cache to a bad block.
///
/// The distinction outlives the move reconciliation it was built beside. An
/// I/O failure is not evidence that a book departed, whatever the caller
/// goes on to do about a departure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimantPlace {
    /// There, and still keying to this directory.
    Live,
    /// The card answered, and the book is not there or is not this
    /// directory's any more.
    Gone,
    /// A read failed. Not evidence of anything.
    Unreadable,
}

/// Whether a cache directory already holds a reading position, asked of the
/// directory itself rather than of the claim over it.
///
/// The question a carry answers before it writes, since overwriting the
/// generations destroys whatever is in them. A claim says nothing about
/// this: a book is claimed when its cache is built, long before it has a
/// place to remember.
///
/// It reads the generations itself, because the ordinary position read
/// collapses a torn record and a card that stumbled into one absence. A
/// record that arrived whole and invalid counts as absent: nobody can return
/// to it, and treating it as occupied would strand a destination whose own
/// carry tore half way.
pub fn book_dir_position_presence<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> PositionPresence
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Absence is stated plainly at every level, and a card holding no cache
    // at all holds no position in it.
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return PositionPresence::Absent,
        Err(_) => return PositionPresence::Unreadable,
    };
    let cache = match cache_root.open_dir(CACHE_V2_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return PositionPresence::Absent,
        Err(_) => return PositionPresence::Unreadable,
    };
    let book = match cache.open_dir(key) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return PositionPresence::Absent,
        Err(_) => return PositionPresence::Unreadable,
    };
    // Any generation that holds a place makes this somewhere not to write.
    // Otherwise a card that stumbled outranks a clean absence, because the
    // one thing this must not do is report a place that is there as gone.
    let mut stumbled = false;
    for name in POSITION_GENERATIONS {
        match position_generation_presence(&book, name) {
            PositionPresence::Present => return PositionPresence::Present,
            PositionPresence::Unreadable => stumbled = true,
            PositionPresence::Absent => {}
        }
    }
    // The legacy single file counts: it is a place a reader left, and the
    // carry's adoption removes it with the rest.
    match legacy_position_presence(&book) {
        PositionPresence::Present => return PositionPresence::Present,
        PositionPresence::Unreadable => stumbled = true,
        PositionPresence::Absent => {}
    }
    if stumbled {
        PositionPresence::Unreadable
    } else {
        PositionPresence::Absent
    }
}

/// One durable generation, answered three ways.
///
/// Mirrors [`read_generation_file`] rather than calling it, because that
/// path deliberately reports only whether it came back with a record, and
/// the whole question here is which kind of nothing it came back with.
///
/// A file whose length is wrong or whose checksum fails reads as `Absent`.
/// It holds no place anybody can return to, and adoption would clear it the
/// same way; what has to be kept apart from absence is a card that would
/// not answer, not a record that answered and was not one.
fn position_generation_presence<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> PositionPresence
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let total = POSITION_BYTES + DURABLE_OVERHEAD;
    let file = match book.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return PositionPresence::Absent,
        Err(_) => return PositionPresence::Unreadable,
    };
    if file.length() as usize != total {
        return PositionPresence::Absent;
    }
    let mut bytes = [0u8; DURABLE_MAX_BYTES];
    if read_exact_file(&file, &mut bytes[..total]).is_err() {
        return PositionPresence::Unreadable;
    }
    let mut payload = [0u8; POSITION_BYTES];
    if decode_durable_record(POSITION_DURABLE_MAGIC, &bytes[..total], &mut payload).is_some() {
        PositionPresence::Present
    } else {
        PositionPresence::Absent
    }
}

/// The pre-durable single position file, answered the same three ways.
fn legacy_position_presence<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> PositionPresence
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match book.open_file_in_dir(POSITION_FILE, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return PositionPresence::Absent,
        Err(_) => return PositionPresence::Unreadable,
    };
    let mut bytes = [0u8; POSITION_BYTES];
    match file.read(&mut bytes) {
        Ok(len) if decode_position(&bytes[..len]).is_some() => PositionPresence::Present,
        Ok(_) => PositionPresence::Absent,
        Err(_) => PositionPresence::Unreadable,
    }
}

/// The position under a key alone, with no ownership check: only for keys
/// derived by firmware before claims existed, whose directories are read as
/// found. Current keys go through [`read_position_file`].
fn read_position_file_at_legacy_key<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Option<(u16, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = cache_root.open_dir(CACHE_V2_DIR).ok()?;
    let book = cache.open_dir(key).ok()?;
    read_position_in(&book)
}

/// [`read_position_file`], with a second layer of the same compatibility it
/// already applies inside one directory: when the book's current key holds
/// no position, look under the key firmware before catalog v8 derived for
/// it. Position is the one non-rebuildable thing under a key, and the v8
/// re-key would otherwise strand every inactive book's place in a directory
/// nothing asks for.
///
/// Read-only: the next ordinary position save lands under the current key,
/// and the old directory stays an orphan. `display_name` and `byte_size`
/// come from the book's catalog record; books with no legacy-representable
/// display shape skip the fallback (see
/// `proto::cache::legacy_position_cache_key`). The legacy directory is read
/// without an ownership check, since pre-claim firmware wrote nothing to
/// check.
pub fn read_position_file_or_legacy<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    display_name: &str,
    byte_size: u32,
) -> Option<(u16, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if let Some(found) = read_position_file(root, owner) {
        return Some(found);
    }
    let legacy = proto::cache::legacy_position_cache_key(display_name, byte_size)?;
    read_position_file_at_legacy_key(root, legacy.as_str())
}

#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn write_state_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: proto::nvm::AppStateRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &cache_root,
        STATE_GENERATIONS,
        STATE_DURABLE_MAGIC,
        &record.encode(),
    )
}

const STATE_GENERATIONS: [&str; 2] = ["STATEA.BIN", "STATEB.BIN"];
/// MarigoldOS v0.4.x durable-state magic; byte-identical for card interchange.
const STATE_DURABLE_MAGIC: [u8; 4] = *b"MGST";

/// Read the newest valid STATEA/STATEB generation, falling back to the
/// legacy `/READER/STATE.BIN`. Returns None when every copy is absent,
/// short, or fails its checksum.
pub fn read_state_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<proto::nvm::AppStateRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; proto::nvm::AppStateRecord::ENCODED_LEN];
    if read_two_generation(
        &cache_root,
        STATE_GENERATIONS,
        STATE_DURABLE_MAGIC,
        &mut bytes,
    ) {
        return proto::nvm::AppStateRecord::decode(&bytes);
    }
    let file = cache_root
        .open_file_in_dir(CACHE_STATE_FILE, Mode::ReadOnly)
        .ok()?;
    // One read suffices for a 32-byte record; shorter V1/V2 files decode
    // from their actual length.
    let len = file.read(&mut bytes).ok()?;
    proto::nvm::AppStateRecord::decode(&bytes[..len])
}

pub fn read_custom_font_manifest<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<CustomFontManifest>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let fonts = cache_root.open_dir(FONT_PACK_DIR).ok()?;
    let file = fonts
        .open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly)
        .ok()?;
    let mut header_bytes = [0u8; FONT_PACK_HEADER_BYTES];
    if file.read(&mut header_bytes).ok()? != FONT_PACK_HEADER_BYTES {
        return None;
    }
    let header = FontPackHeader::decode(&header_bytes).ok()?;
    if file.length() != header.total_len {
        return None;
    }
    let face_count = usize::from(header.face_count).min(crate::store::MAX_CUSTOM_FONT_FACES);
    let mut faces = [FontPackFaceRecord::EMPTY; crate::store::MAX_CUSTOM_FONT_FACES];
    file.seek_from_start(header.face_table_offset).ok()?;
    let mut face_bytes = [0u8; FONT_PACK_FACE_RECORD_BYTES];
    for face in faces.iter_mut().take(face_count) {
        if file.read(&mut face_bytes).ok()? != FONT_PACK_FACE_RECORD_BYTES {
            return None;
        }
        *face = FontPackFaceRecord::decode(&face_bytes).ok()?;
    }
    file.seek_from_start(header.name_offset).ok()?;
    let mut name_bytes = [0u8; proto::font_pack::FONT_PACK_MAX_NAME_BYTES];
    let name_len = header.name_len as usize;
    if file.read(&mut name_bytes[..name_len]).ok()? != name_len {
        return None;
    }
    let name = decode_font_pack_name(header, &name_bytes[..name_len]).ok()?;
    Some(CustomFontManifest {
        name,
        identity: header.identity,
        faces,
        face_count,
    })
}

const WIFI_FILE: &str = "WIFI.BIN";
const WIFI_GENERATIONS: [&str; 2] = ["WIFIA.BIN", "WIFIB.BIN"];
/// The AP hint's own generation pair, separate from the credentials' —
/// see [`proto::nvm::WifiApHintRecord`] for why the two are not one record.
const WIFI_HINT_GENERATIONS: [&str; 2] = ["WIFHA.BIN", "WIFHB.BIN"];
/// MarigoldOS v0.4.x durable-credentials magic; byte-identical for card
/// interchange.
const WIFI_DURABLE_MAGIC: [u8; 4] = *b"MGWF";
const WIFI_HINT_DURABLE_MAGIC: [u8; 4] = *b"CAWH";

/// Write the onboarding portal's credentials to alternating WIFIA/WIFIB.
#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn write_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: proto::nvm::WifiCredentialsRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &cache_root,
        WIFI_GENERATIONS,
        WIFI_DURABLE_MAGIC,
        &record.encode(),
    )
}

/// Delete every stored credential copy (legacy WIFI.BIN and both
/// generations); missing files count as success.
pub fn delete_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(cache_root) = root.open_dir(CACHE_ROOT_DIR) else {
        return true;
    };
    let mut ok = true;
    for name in [
        WIFI_FILE,
        WIFI_GENERATIONS[0],
        WIFI_GENERATIONS[1],
        // Forgetting a network forgets which AP served it. Leaving the hint
        // would steer the next join for a *different* network toward it —
        // the SSID hash refuses that, but a hint for a network the user
        // deliberately dropped has no reason to survive either.
        WIFI_HINT_GENERATIONS[0],
        WIFI_HINT_GENERATIONS[1],
    ] {
        ok &= upload_store::remove_file_reclaiming_clusters(&cache_root, name)
            != upload_store::RemoveStatus::Failed;
    }
    ok
}

/// Persist which AP the station last associated through, so the next session
/// can join it directly instead of sweeping every channel.
///
/// Failure is ignored by callers by design: the hint is an accelerator, and a
/// session that cannot write one simply scans next time.
#[allow(clippy::result_unit_err)] // Same as the credentials writer above: the card gives no distinguishable reason, and the only caller branches on success.
pub fn write_wifi_hint_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: proto::nvm::WifiApHintRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &cache_root,
        WIFI_HINT_GENERATIONS,
        WIFI_HINT_DURABLE_MAGIC,
        &record.encode(),
    )
}

/// Read the stored AP hint. `None` for missing, torn, or nonsensical —
/// every one of which just means the next join scans.
pub fn read_wifi_hint_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<proto::nvm::WifiApHintRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; proto::nvm::WifiApHintRecord::ENCODED_LEN];
    read_two_generation(
        &cache_root,
        WIFI_HINT_GENERATIONS,
        WIFI_HINT_DURABLE_MAGIC,
        &mut bytes,
    )
    .then(|| proto::nvm::WifiApHintRecord::decode(&bytes))
    .flatten()
}

/// Read the newest WIFIA/WIFIB generation, falling back to legacy WIFI.BIN;
/// None when every copy is missing, short, or corrupt.
pub fn read_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<proto::nvm::WifiCredentialsRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; proto::nvm::WifiCredentialsRecord::ENCODED_LEN];
    if read_two_generation(
        &cache_root,
        WIFI_GENERATIONS,
        WIFI_DURABLE_MAGIC,
        &mut bytes,
    ) {
        return proto::nvm::WifiCredentialsRecord::decode(&bytes);
    }
    let file = cache_root
        .open_file_in_dir(WIFI_FILE, Mode::ReadOnly)
        .ok()?;
    let len = file.read(&mut bytes).ok()?;
    proto::nvm::WifiCredentialsRecord::decode(&bytes[..len])
}

pub fn load_v2_cover_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    library: &mut ReaderStore,
) -> CoverLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_cover_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; COVER_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return CoverLoadResult::Invalid;
        }
        let Ok(header) = decode_cover_header(&header_bytes) else {
            return CoverLoadResult::Invalid;
        };
        // Read straight into the store's cover buffer: a stack copy here is
        // an ~8 KB frame on a path that already runs near the stack floor.
        if read_exact_file(file, library.cover_bits_mut()).is_err() {
            library.clear_cover();
            return CoverLoadResult::Invalid;
        }
        library.finish_cover_load(header.width, header.height);
        CoverLoadResult::Hit
    })
    .unwrap_or(CoverLoadResult::Miss)
}

/// Read just the book's total page count from the V2 index header,
/// without loading any section records. Used at boot restore so the Home
/// progress bar has a denominator before the book is opened. Returns 0 if the
/// index is missing, stale, or for another book.
pub fn read_v2_book_total_pages<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    library: &ReaderStore,
) -> u32
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return 0;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return 0;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.custom_font_identity != library.custom_font_identity()
        {
            return 0;
        }
        header.total_pages
    })
    .unwrap_or(0)
}

/// Bounds the TOC and label counts a BOOK.BIN header may claim before any
/// body loader trusts them.
fn v2_toc_label_bounds_ok(header: &BookV2Header) -> bool {
    header.toc_count as usize <= MAX_SD_TOC_ITEMS
        && header.toc_text_bytes as usize <= MAX_SD_TOC_TEXT_BYTES
        && header.title_text_bytes as usize <= 64
        && header.author_text_bytes as usize <= 64
}

/// Read the TOC records and text at the file's current position (just past
/// the section records) into the library. The one decoder of BOOK.BIN's
/// TOC body — `load_v2_book_index` and `load_v2_book_labels_and_toc` both
/// call it. On any failure the library's TOC is left cleared and false is
/// returned.
fn read_v2_toc_into_library<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: &BookV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    library.clear_toc();
    if !read_records_batched(
        file,
        TOC_RECORD_BYTES,
        header.toc_count as usize,
        |index, bytes| {
            let Ok(record) = decode_toc(bytes) else {
                return false;
            };
            if !toc_record_fits_text(record, header.toc_text_bytes) {
                return false;
            }
            library.toc[index] = record;
            library.toc_page[index] = 0;
            true
        },
    ) {
        library.clear_toc();
        return false;
    }
    if header.toc_text_bytes > 0 {
        let text_len = header.toc_text_bytes as usize;
        if read_exact_file(file, &mut library.toc_text[..text_len]).is_err() {
            library.clear_toc();
            return false;
        }
        library.toc_text_len = text_len;
        library.toc_count = header.toc_count as usize;
    }
    true
}

/// Read the title/author labels at the file's current position (just past
/// the TOC text) and publish them to the library — the shared tail of both
/// BOOK.BIN body loaders. A book with neither label leaves the store's
/// labels untouched.
fn read_v2_labels_into_library<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: &BookV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut title = [0u8; 64];
    let mut author = [0u8; 64];
    let mut title_str = "";
    let mut author_str = "";
    if header.title_text_bytes > 0 {
        let title_len = header.title_text_bytes as usize;
        if read_exact_file(file, &mut title[..title_len]).is_err() {
            return false;
        }
        let Ok(parsed_title) = core::str::from_utf8(&title[..title_len]) else {
            return false;
        };
        title_str = parsed_title;
    }
    if header.author_text_bytes > 0 {
        let author_len = header.author_text_bytes as usize;
        if read_exact_file(file, &mut author[..author_len]).is_err() {
            return false;
        }
        let Ok(parsed_author) = core::str::from_utf8(&author[..author_len]) else {
            return false;
        };
        author_str = parsed_author;
    }
    if header.title_text_bytes > 0 || header.author_text_bytes > 0 {
        library.set_book_labels(title_str, author_str);
    }
    true
}

pub fn load_v2_book_index<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> BookIndexLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return BookIndexLoadResult::Invalid;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return BookIndexLoadResult::Invalid;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.font_config
                != layout::reader_layout_config(library.type_settings(), library.portrait())
            || header.custom_font_identity != library.custom_font_identity()
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || !v2_toc_label_bounds_ok(&header)
            || header.total_pages == 0
        {
            return BookIndexLoadResult::Invalid;
        }
        let mut sections = [EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS];
        if !read_records_batched(
            file,
            BOOK_V2_SECTION_RECORD_BYTES,
            header.section_count as usize,
            |index, bytes| {
                let Ok(record) = decode_book_v2_section(bytes) else {
                    return false;
                };
                if record.page_count == 0 {
                    return false;
                }
                sections[index] = record;
                true
            },
        ) {
            return BookIndexLoadResult::Invalid;
        }
        if !read_v2_toc_into_library(file, &header, library) {
            return BookIndexLoadResult::Invalid;
        }
        if !read_v2_labels_into_library(file, &header, library) {
            return BookIndexLoadResult::Invalid;
        }
        library.set_book_index(
            header.total_pages,
            header.partial,
            &sections[..header.section_count as usize],
        );
        BookIndexLoadResult::Hit {
            unfinished: header.resume_spine != 0,
        }
    })
    .unwrap_or(BookIndexLoadResult::Miss)
}

/// Read just the stored EPUB title from a book's v2 cache index, skipping the
/// section records and the rest of the body. The Library list uses this to
/// label books whose on-disk name can't carry a real title (8.3 upload names)
/// with the title learned the last time the book was opened. Returns false
/// (leaving `out` untouched) when there is no cache for the book, the cached
/// identity doesn't match, or the cache holds no title.
pub fn read_cached_book_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.title_text_bytes == 0
            || header.title_text_bytes as usize > 64
        {
            return false;
        }
        // The title text sits after the header, the section records, and the
        // TOC block (records + text) -- the same body order write_v2_book_index
        // lays down and load_v2_book_index reads through.
        let title_offset = BOOK_V2_HEADER_BYTES as u32
            + header.section_count as u32 * BOOK_V2_SECTION_RECORD_BYTES as u32
            + header.toc_count as u32 * TOC_RECORD_BYTES as u32
            + header.toc_text_bytes;
        if file.seek_from_start(title_offset).is_err() {
            return false;
        }
        let title_len = header.title_text_bytes as usize;
        let mut title = [0u8; 64];
        if read_exact_file(file, &mut title[..title_len]).is_err() {
            return false;
        }
        let Ok(title_str) = core::str::from_utf8(&title[..title_len]) else {
            return false;
        };
        out.clear();
        let _ = out.push_str(title_str);
        true
    })
    .unwrap_or(false)
}

/// What a cache's BOOK.BIN had to say about itself.
///
/// The three cases are not interchangeable, because a cache key is only 28
/// bits of hash and collisions are an accepted possibility. `Absent` says the
/// cache has no index — nothing usable is there, whoever it belongs to.
/// `Unreadable` says there *is* an index and we could not tell whose it is,
/// which is exactly when a caller about to delete has to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheHeader {
    Present(BookV2Header),
    /// No BOOK.BIN at all.
    Absent,
    /// BOOK.BIN is there and says nothing usable: truncated, corrupt, or the
    /// read failed.
    Unreadable,
}

/// Read a book cache's v2 header, for its stored source identity and section
/// count. Used by the orphan sweep to decide whether a cache still belongs to
/// a book on the card, and by the clear to prove a key names the book it was
/// asked about.
pub fn read_cache_header<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> CacheHeader
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Opened step by step rather than through `with_v2_book_file`, which
    // folds every failure into one `None`. A directory that is not there and
    // a directory that would not open are the same value to it, and the
    // difference is the whole point here.
    macro_rules! open {
        ($opened:expr) => {
            match $opened {
                Ok(handle) => handle,
                Err(embedded_sdmmc::Error::NotFound) => return CacheHeader::Absent,
                Err(_) => return CacheHeader::Unreadable,
            }
        };
    }
    let cache_root = open!(root.open_dir(CACHE_ROOT_DIR));
    let cache = open!(cache_root.open_dir(CACHE_V2_DIR));
    let book_dir = open!(cache.open_dir(key));
    let file = open!(book_dir.open_file_in_dir(CACHE_BOOK_FILE, Mode::ReadOnly));
    let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
    if read_exact_file(&file, &mut header_bytes).is_err() {
        return CacheHeader::Unreadable;
    }
    match decode_book_v2_header(&header_bytes) {
        Ok(header) => CacheHeader::Present(header),
        Err(_) => CacheHeader::Unreadable,
    }
}

/// Section files deleted per directory-listing pass. embedded-sdmmc will not
/// let a file be opened while an iteration holds the volume lock, so names are
/// collected first and deleted once the walk has returned, and the loop
/// re-lists until the directory comes back empty.
///
/// The batch is what keeps that staging off the stack budget: 16 short names
/// is 392 B, against the 7,680 B a whole `MAX_BOOK_SECTIONS` book would need,
/// and it costs 20 extra listings only for a book at that cap — an ordinary
/// book empties in two or three passes. The deepest caller is the publish
/// failure path inside the EPUB-open chain, which is why this is sized rather
/// than left to the book.
const SECTION_SWEEP_BATCH: usize = 16;

/// A FAT short name at its widest: `NNNNNNNN.EXT`.
const SHORT_NAME_BYTES: usize = 12;

/// Delete one book cache completely: every section file, BOOK/TOC/COVER/CONT,
/// then the emptied `SECTIONS/` and `<key>/` directories themselves. Returns
/// whether the cache is actually gone.
///
/// Nothing here trusts the cache's own header for what to delete. It used to
/// remove sections by generated name over `0..section_count`, which left a
/// header that undercounted (a torn write, a truncated build) with orphan
/// section files that no later pass could name — the sweep reads
/// `section_count` from the same BOOK.BIN this deletes, so once the header was
/// gone the leftovers were unnameable and `SECTIONS/` stayed non-empty
/// forever. `SECTIONS/` is enumerated instead, so what is on the card decides
/// what gets deleted.
///
/// BOOK.BIN goes first, deliberately. It is the cache's liveness marker, so an
/// interrupted delete leaves a cache that reads as absent and gets rebuilt,
/// rather than a header advertising sections that are no longer there.
///
/// The global reading position in READER/STATE.BIN is never touched.
/// Delete every rebuildable artifact in an open book directory. Positions
/// are untouched, and so is the claim: ownership evidence must live exactly
/// as long as the positions it vouches for, so its lifecycle belongs to the
/// callers, not to this sweep of derivables. True when everything named
/// went.
fn empty_book_dir_artifacts<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut cleared = true;
    for name in [
        CACHE_BOOK_FILE,
        CACHE_TOC_FILE,
        CACHE_COVER_FILE,
        CACHE_CONTENT_FILE,
    ] {
        if upload_store::remove_file_reclaiming_clusters(book, name)
            == upload_store::RemoveStatus::Failed
        {
            cleared = false;
        }
    }
    match book.open_dir(CACHE_SECTIONS_DIR) {
        Ok(sections) => {
            if !empty_sections_dir(&sections) {
                cleared = false;
            }
        }
        Err(embedded_sdmmc::Error::NotFound) => {}
        Err(_) => cleared = false,
    }
    if cleared {
        // The SECTIONS handle has dropped; the empty directory can go now
        // (a directory entry has no chain to reclaim). A refusal here
        // leaves an empty directory, not cache data, so it does not make
        // the clear a failure.
        let _ = book.delete_entry_in_dir(CACHE_SECTIONS_DIR);
    }
    cleared
}

pub fn empty_cache_dir<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // A directory that isn't there holds no cache; anything else going wrong
    // on the way in means we cannot say the cache is gone.
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return true,
        Err(_) => return false,
    };
    let cache = match cache_root.open_dir(CACHE_V2_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return true,
        Err(_) => return false,
    };
    let position_kept;
    let mut cleared = true;
    {
        let book = match cache.open_dir(key) {
            Ok(dir) => dir,
            Err(embedded_sdmmc::Error::NotFound) => return true,
            Err(_) => return false,
        };
        if !empty_book_dir_artifacts(&book) {
            cleared = false;
        }
        // The claim outlives the clear exactly as long as the positions it
        // vouches for: with them gone, the evidence has nothing left to
        // prove and the directory can be reclaimed; with them kept, the
        // claim stays, or the surviving place would become adoptable by a
        // full-hash twin.
        if !has_position_file(&book)
            && upload_store::remove_file_reclaiming_clusters(&book, proto::cache::CACHE_CLAIM_FILE)
                == upload_store::RemoveStatus::Failed
        {
            cleared = false;
        }
        // Everything above worked from a list of names. This is the part that
        // does not: it asks the directory what is actually left, which is the
        // only way to catch what the names never covered — a file from an
        // older layout, a `SECTIONS/` that refused to go, anything a future
        // format adds without teaching this function about it.
        cleared = cleared && book_dir_is_reclaimed(&book);
        // Everything above is re-derivable from the EPUB; the position is not.
        // POS*.BIN is the authoritative record of where the reader is in this
        // book, so it is never swept, and the directory holding it has to stay
        // as well. A book moved off the card and back keys to the same name and
        // size, so its place is still waiting when it returns.
        position_kept = has_position_file(&book);
    }
    if cleared && !position_kept {
        // Likewise the book handle: closed by the scope above, deletable here
        // (a directory entry has no chain to reclaim).
        let _ = cache.delete_entry_in_dir(key);
    }
    cleared
}

/// The section ordinal a `S###.BIN` name encodes, or `None` for any other
/// name. Parsed rather than trusted: `SECTIONS/` is on removable media, so a
/// name that is not one of ours must be left alone, not miscounted into the
/// prune range.
fn section_ordinal_from_name(name: &str) -> Option<u16> {
    let digits = name
        .strip_prefix(['S', 's'])?
        .get(..3)
        .filter(|rest| rest.bytes().all(|byte| byte.is_ascii_digit()))?;
    let suffix = name.get(4..)?;
    if !suffix.eq_ignore_ascii_case(".BIN") {
        return None;
    }
    digits.parse::<u16>().ok()
}

/// Delete the section files a freshly published index no longer names.
///
/// Section files are keyed by section *ordinal* — a dense `0..count` counter
/// the walk assigns as it flushes, distinct from the spine index the record
/// also carries — so a rebuild producing fewer sections than the one before
/// it strands `S<count>..` on the card. Nothing references them:
/// `load_v2_section_by_global_page` indexes off BOOK.BIN. They are unreachable
/// but not free, and they survive until the whole cache directory is emptied.
///
/// No shipping setting reaches that state today. Sections split on content
/// volume — `flush_if_full` breaks on block and text capacity long before the
/// page-count bound — so re-flowing the same content produces the same count.
/// Measured on the X3 2026-08-04: one book at 1252, 763 and 708 pages across a
/// type-size change and an orientation flip held 79 sections throughout. What
/// *would* shrink the count is a change to the capacity constants themselves,
/// which re-derives fewer sections over the same content and strands the old
/// tail. This exists for that case; it is insurance, not a present-day leak.
///
/// **Only ever call this with a final section count.** A suspended walk is
/// coming back to write more sections, and pruning against its provisional
/// count would delete the ones it is about to need.
///
/// Best effort by design: orphans are dead weight, not corruption, so a
/// failure here must not turn a successful publish into a failed one. It is
/// safe to run only *after* the new index is on the card — everything it
/// deletes is already unreachable from the index a reader could be holding.
///
/// Returns how many files it removed.
fn prune_orphan_sections_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    keep_count: u16,
) -> usize
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    let mut removed = 0usize;
    // Same shape as `empty_sections_dir`: collect a bounded batch by listing,
    // delete it, and list again, because deleting while iterating is not
    // something the directory walk promises. The spare pass proves the tail
    // is gone rather than merely out of budget.
    let max_passes = MAX_BOOK_SECTIONS.div_ceil(SECTION_SWEEP_BATCH) + 1;
    for _ in 0..max_passes {
        let mut names: heapless::Vec<String<SHORT_NAME_BYTES>, SECTION_SWEEP_BATCH> =
            heapless::Vec::new();
        if sections
            .iterate_dir(|entry| {
                // A full batch is this pass's whole quota; the next pass
                // starts the scan again and picks up the rest.
                if names.is_full() {
                    return ControlFlow::Break(());
                }
                if entry.attributes.is_directory() {
                    return ControlFlow::Continue(());
                }
                let mut name = String::<SHORT_NAME_BYTES>::new();
                if write!(name, "{}", entry.name).is_err() {
                    return ControlFlow::Continue(());
                }
                match section_ordinal_from_name(name.as_str()) {
                    Some(ordinal) if ordinal >= keep_count => {
                        let _ = names.push(name);
                    }
                    _ => {}
                }
                ControlFlow::Continue(())
            })
            .is_err()
        {
            return removed;
        }
        if names.is_empty() {
            return removed;
        }
        // Attempt every name in the batch, including the ones after a failure.
        // `remove_file_reclaiming_clusters` opens, truncates, closes and then
        // deletes, and a fault in any of those fails that one file without
        // saying anything about the next — the card model these paths are
        // tested against injects exactly that, a single refused write followed
        // by writes that succeed. Abandoning the batch on the first failure
        // would leave the rest stranded until another completed rebuild, and
        // for the capacity-constant change this exists for there may never be
        // one.
        let mut progressed = false;
        for name in &names {
            if upload_store::remove_file_reclaiming_clusters(sections, name.as_str())
                != upload_store::RemoveStatus::Failed
            {
                removed += 1;
                progressed = true;
            }
        }
        if !progressed {
            // A pass that took nothing is where best-effort stops. The listing
            // is deterministic, so the next pass would collect the same names
            // and refuse them again; this is the bound on a card that really
            // has stopped accepting deletes, while a pass that took anything
            // still earns the failures beside it one more attempt.
            return removed;
        }
    }
    removed
}

/// [`prune_orphan_sections_in`], opening the book's `SECTIONS/` directory.
pub fn prune_orphan_sections<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    keep_count: u16,
) -> usize
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_sections_dir(root, owner, |sections| match sections {
        Some(sections) => prune_orphan_sections_in(sections, keep_count),
        None => 0,
    })
}

/// Delete every file in an opened `SECTIONS/` directory, by listing rather
/// than by generated name. Returns false if any delete failed or the directory
/// still had entries after the pass budget — either way the caller must not
/// report the cache as cleared.
fn empty_sections_dir<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    // A full book is MAX_BOOK_SECTIONS files; the spare pass is what proves
    // the directory came back empty rather than merely running out of budget.
    let max_passes = MAX_BOOK_SECTIONS.div_ceil(SECTION_SWEEP_BATCH) + 1;
    for _ in 0..max_passes {
        let mut names: heapless::Vec<String<SHORT_NAME_BYTES>, SECTION_SWEEP_BATCH> =
            heapless::Vec::new();
        // Something this pass could not take. It keeps the walk going (there
        // may still be files worth deleting) but a pass that ends with the
        // directory non-empty must not read as emptied.
        let mut blocked = false;
        if sections
            .iterate_dir(|entry| {
                let mut name = String::<SHORT_NAME_BYTES>::new();
                if write!(name, "{}", entry.name).is_err() {
                    // A name this pass cannot reproduce is a name it cannot
                    // delete; say so rather than silently leaving it.
                    blocked = true;
                    return ControlFlow::Continue(());
                }
                if name.as_str() == "." || name.as_str() == ".." {
                    return ControlFlow::Continue(());
                }
                if entry.attributes.is_directory() {
                    // Nothing here writes directories. Whatever it is, this
                    // pass cannot delete it, and skipping it silently was how
                    // a `SECTIONS/` holding only a subdirectory reported
                    // itself emptied.
                    blocked = true;
                    return ControlFlow::Continue(());
                }
                if names.push(name).is_err() {
                    blocked = true;
                }
                ControlFlow::Continue(())
            })
            .is_err()
        {
            return false;
        }
        if names.is_empty() {
            return !blocked;
        }
        for name in &names {
            if upload_store::remove_file_reclaiming_clusters(sections, name.as_str())
                == upload_store::RemoveStatus::Failed
            {
                return false;
            }
        }
    }
    false
}

/// The files a cleared cache is allowed to keep: the reading position, in
/// either the durable A/B pair or the legacy single file.
fn is_kept_after_clear(name: &str) -> bool {
    POSITION_GENERATIONS
        .iter()
        .chain(core::iter::once(&POSITION_FILE))
        .chain(core::iter::once(&proto::cache::CACHE_CLAIM_FILE))
        .any(|kept| name.eq_ignore_ascii_case(kept))
}

/// Whether the book's cache directory holds nothing but its reading position.
///
/// This is what success is reported on. The deletes above name what they
/// expect to find; this asks what is really there, so a clear cannot claim to
/// have reclaimed a directory it left occupied.
fn book_dir_is_reclaimed<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    let mut reclaimed = true;
    let walked = book.iterate_dir(|entry| {
        let mut name = String::<SHORT_NAME_BYTES>::new();
        if write!(name, "{}", entry.name).is_err() {
            reclaimed = false;
            return ControlFlow::Break(());
        }
        if name.as_str() == "." || name.as_str() == ".." {
            return ControlFlow::Continue(());
        }
        // A surviving `SECTIONS/` lands here too: it is not a position file,
        // so it fails the check like any other leftover.
        if !is_kept_after_clear(name.as_str()) {
            reclaimed = false;
            // One leftover settles the question.
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    walked.is_ok() && reclaimed
}

/// Whether a book's cache directory still holds a reading position, in either
/// the durable A/B pair or the legacy single file.
fn has_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    POSITION_GENERATIONS
        .iter()
        .chain(core::iter::once(&POSITION_FILE))
        .any(|name| book.open_file_in_dir(*name, Mode::ReadOnly).is_ok())
}

#[expect(clippy::too_many_arguments)] // The index's own field set: identity, shape, and the resume cursor, all caller-owned
pub fn write_v2_book_index<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    total_pages: u32,
    sections: &[BookV2SectionRecord],
    library: &ReaderStore,
    partial: bool,
    // The spine item a suspended progressive build will walk next, or `0`
    // when nothing is still building this index. See
    // [`proto::cache::BookV2Header::resume_spine`] — it is what stops an
    // abandoned build's index from capping the book forever.
    resume_spine: u16,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if total_pages == 0 || sections.is_empty() || sections.len() > MAX_BOOK_SECTIONS {
        return false;
    }
    if ensure_v2_cache_dirs(root, owner).is_err() {
        return false;
    }
    with_v2_book_file(root, owner, Mode::ReadWriteCreateOrTruncate, |file| {
        let toc_count = library
            .toc_count
            .min(MAX_SD_TOC_ITEMS)
            .min(u16::MAX as usize);
        let title_text_bytes = library.title.len().min(64) as u32;
        let author_text_bytes = library.author.len().min(64) as u32;
        let header = BookV2Header {
            source_hash: source_identity.0,
            source_size: source_identity.1,
            total_pages,
            section_count: sections.len().min(u16::MAX as usize) as u16,
            spine_count: sections
                .iter()
                .map(|section| section.spine as usize + 1)
                .max()
                .unwrap_or(0)
                .min(u16::MAX as usize) as u16,
            toc_count: toc_count as u16,
            toc_text_bytes: library
                .toc_text_len
                .min(MAX_SD_TOC_TEXT_BYTES)
                .min(u32::MAX as usize) as u32,
            title_text_bytes,
            author_text_bytes,
            viewport_width: 800,
            viewport_height: 480,
            font_config: layout::reader_layout_config(library.type_settings(), library.portrait()),
            custom_font_identity: library.custom_font_identity(),
            partial,
            resume_spine,
        };
        let mut bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if encode_book_v2_header(header, &mut bytes).is_err() {
            return false;
        }
        let mut stage = WriteStage::new(file);
        if stage.push(&bytes).is_err() {
            return false;
        }
        let mut record_bytes = [0u8; BOOK_V2_SECTION_RECORD_BYTES];
        for section in sections {
            if encode_book_v2_section(*section, &mut record_bytes).is_err()
                || stage.push(&record_bytes).is_err()
            {
                return false;
            }
        }
        let mut toc_bytes = [0u8; TOC_RECORD_BYTES];
        for record in library.toc.iter().take(toc_count).copied() {
            if encode_toc(record, &mut toc_bytes).is_err() || stage.push(&toc_bytes).is_err() {
                return false;
            }
        }
        if stage.flush().is_err() {
            return false;
        }
        if header.toc_text_bytes > 0
            && file
                .write(&library.toc_text[..header.toc_text_bytes as usize])
                .is_err()
        {
            return false;
        }
        if header.title_text_bytes > 0
            && file
                .write(&library.title.as_bytes()[..header.title_text_bytes as usize])
                .is_err()
        {
            return false;
        }
        if header.author_text_bytes > 0
            && file
                .write(&library.author.as_bytes()[..header.author_text_bytes as usize])
                .is_err()
        {
            return false;
        }
        true
    })
    .unwrap_or(false)
}

fn toc_record_fits_text(record: proto::cache::TocRecord, text_bytes: u32) -> bool {
    range_fits(record.title_offset, record.title_len, text_bytes)
        && range_fits(record.href_offset, record.href_len, text_bytes)
        && range_fits(record.anchor_offset, record.anchor_len, text_bytes)
}

fn range_fits(offset: u32, len: u16, text_bytes: u32) -> bool {
    offset
        .checked_add(len as u32)
        .map(|end| end <= text_bytes)
        .unwrap_or(false)
}

pub fn load_v2_section_by_global_page<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    global_page: u32,
    library: &mut ReaderStore,
) -> CacheLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(section) = library.section_for_global_page(global_page) else {
        return CacheLoadResult::Miss;
    };
    let result = load_v2_section_cache(
        root,
        owner,
        source_identity,
        section.section,
        section.spine,
        section.page_count as usize,
        library,
    );
    if let CacheLoadResult::Hit { pages, repaginated } = result {
        library.set_current_section_range(section.start_page, pages);
        if repaginated {
            let _ = write_v2_section_cache(root, owner, source_identity, section.section, library);
        }
    }
    result
}

fn open_or_make_dir<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &'a Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> Result<Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match parent.open_dir(name) {
        Ok(dir) => Ok(dir),
        Err(_) => {
            let _ = parent.make_dir_in_dir(name);
            parent.open_dir(name).map_err(|_| ())
        }
    }
}

pub(crate) fn load_v2_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    section: u16,
    expected_spine: u16,
    target_pages: usize,
    library: &mut ReaderStore,
) -> CacheLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_section_file(root, owner, section, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; SECTION_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return CacheLoadResult::Invalid;
        }
        let Ok(header) = decode_section_v2_header(&header_bytes) else {
            return CacheLoadResult::Invalid;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.spine != expected_spine
        {
            return CacheLoadResult::Invalid;
        }
        let expected_config =
            layout::reader_layout_config(library.type_settings(), library.portrait());
        if header.custom_font_identity != library.custom_font_identity() {
            return CacheLoadResult::Invalid;
        }
        // Cached blocks are pre-wrapped lines: they survive a spacing
        // change (heights re-walk below) but not a size change, which
        // alters every wrap point and needs the full EPUB rebuild.
        if header.font_config & !0b11 != expected_config & !0b11 {
            return CacheLoadResult::Invalid;
        }
        let layout_matches = header.font_config == expected_config;
        if !load_v2_section_body(file, header, library) {
            return CacheLoadResult::Invalid;
        }
        if !layout_matches {
            layout::rebuild_page_index(library);
        }
        let pages = library.page_count;
        if pages < target_pages {
            CacheLoadResult::TooShort { pages }
        } else {
            CacheLoadResult::Hit {
                pages,
                repaginated: !layout_matches,
            }
        }
    })
    .unwrap_or(CacheLoadResult::Miss)
}

pub(crate) fn write_v2_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    section: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if ensure_v2_cache_dirs(root, owner).is_err() {
        cache_log!("cache: v2 ensure dirs failed key={}", owner.key);
        return false;
    }
    with_v2_section_file(
        root,
        owner,
        section,
        Mode::ReadWriteCreateOrTruncate,
        |file| write_v2_section_body(file, source_identity, library.cached_spine, library),
    )
    .unwrap_or_else(|| {
        cache_log!(
            "cache: v2 open section failed key={} section={}",
            owner.key,
            section
        );
        false
    })
}

/// Open the book's SECTIONS directory once and run `f` with it, so a whole
/// build writes tens of section files without re-walking the four-level
/// cache chain per section. Directory creation failure passes `None`: the
/// build still runs, every section write reports failure, and the book is
/// marked partial — the same degraded path as before.
pub fn with_v2_sections_dir<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    f: impl for<'a> FnOnce(Option<&Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>) -> R,
) -> R
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // One handle walks the chain via change_dir, so the whole build holds a
    // single directory slot instead of the four-level ladder. The caller is
    // responsible for `ensure_v2_cache_dirs` when the tree might not exist
    // yet (the full build runs it once up front); a missing tree lands in
    // the `f(None)` fallback like any other open failure.
    let Some(mut dir) = open_v2_book_dir(root, owner) else {
        return f(None);
    };
    if dir.change_dir(CACHE_SECTIONS_DIR).is_err() {
        return f(None);
    }
    f(Some(&dir))
}

/// Write one section file into an already-open SECTIONS directory — the
/// per-section body of `write_v2_section_cache` without the per-call
/// directory walk.
pub fn write_v2_section_cache_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
    section: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(section, &mut name);
    match sections.open_file_in_dir(name.as_str(), Mode::ReadWriteCreateOrTruncate) {
        Ok(file) => write_v2_section_body(&file, source_identity, library.cached_spine, library),
        Err(_) => {
            cache_log!("cache: v2 open section failed section={}", section);
            false
        }
    }
}

/// What a book directory's claim says about one owner.
pub enum ClaimState {
    /// An active claim names this owner.
    MineActive,
    /// A claim naming this owner that a sweep released after the book left
    /// the card. The returning owner resumes the directory and the
    /// positions the evidence proves are its own.
    MineReleased,
    /// A well-formed active claim names another book: a full-hash twin
    /// holds the directory, and nothing here may read or write it.
    OtherActive,
    /// Another book's claim, released after its owner left the card.
    /// Adoptable, but the surviving positions are provably not the
    /// adopter's and go with the adoption.
    OtherReleased,
    /// No claim, or bytes that are not a claim at all. A writer repairs
    /// this by emptying the directory, positions included since nothing
    /// vouches for them, and claiming it; a reader treats it as a miss.
    Unclaimed,
    /// The card would not answer an open or a read. Failure to read the
    /// claim is not evidence that there is no owner, so a writer refuses
    /// rather than adopts, and a reader misses.
    Fault,
}

/// Why a writer was refused the directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimDenied {
    /// Another book's active claim holds the key. Deliberate and durable:
    /// retrying without the card changing cannot help, and callers must not
    /// treat it like a card fault.
    Foreign,
    /// The card would not answer, or a write failed. Retryable.
    Fault,
}

/// Delete the position files in an open book directory: the A/B pair and
/// the legacy single file. Only claim adoption does this, and only because
/// the surviving place provably or unprovably belongs to somebody else.
fn remove_position_files<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut cleared = true;
    for name in POSITION_GENERATIONS
        .iter()
        .chain(core::iter::once(&POSITION_FILE))
    {
        if upload_store::remove_file_reclaiming_clusters(book, name)
            == upload_store::RemoveStatus::Failed
        {
            cleared = false;
        }
    }
    cleared
}

/// Read the claim of an already-open book directory against one owner.
fn book_dir_claim<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) -> ClaimState
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match book.open_file_in_dir(proto::cache::CACHE_CLAIM_FILE, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return ClaimState::Unclaimed,
        Err(_) => return ClaimState::Fault,
    };
    let stored_len = file.length() as usize;
    if stored_len > proto::cache::CACHE_CLAIM_MAX_BYTES {
        // Readable and absurd: not a claim, like any other alien bytes.
        return ClaimState::Unclaimed;
    }
    let mut bytes = [0u8; proto::cache::CACHE_CLAIM_MAX_BYTES];
    if read_exact_file(&file, &mut bytes[..stored_len]).is_err() {
        return ClaimState::Fault;
    }
    match proto::cache::read_cache_claim(&bytes[..stored_len], owner.root, owner.locator) {
        proto::cache::CacheClaimReading::MineActive => ClaimState::MineActive,
        proto::cache::CacheClaimReading::MineReleased => ClaimState::MineReleased,
        proto::cache::CacheClaimReading::OtherActive => ClaimState::OtherActive,
        proto::cache::CacheClaimReading::OtherReleased => ClaimState::OtherReleased,
        proto::cache::CacheClaimReading::Invalid => ClaimState::Unclaimed,
    }
}

/// What is stored in an open directory's claim file.
///
/// Three-valued on purpose. A card that would not answer is not the same
/// thing as a directory with no claim, and collapsing them into one absence
/// is how a rewrite comes to erase the evidence it was meant to preserve: a
/// single failed read would look exactly like a claim that had none.
// One decoded claim beside two empty variants, so the enum is the size of a
// locator. The same trade `DirClaimant` makes and for the same reason: one
// instance lives briefly on a shallow frame, not in a collection.
#[allow(clippy::large_enum_variant)]
enum StoredClaim {
    /// A well-formed claim, decoded.
    Present {
        root: proto::library_path::BookRoot,
        locator: String<{ proto::library_path::MAX_PATH_BYTES }>,
        released: bool,
        evidence: proto::cache::CacheEvidence,
    },
    /// No claim file, or bytes that are not a claim.
    Absent,
    /// The card would not answer. Evidence of nothing.
    Fault,
}

/// Read an open directory's claim file.
fn read_stored_claim<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> StoredClaim
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match book.open_file_in_dir(proto::cache::CACHE_CLAIM_FILE, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return StoredClaim::Absent,
        Err(_) => return StoredClaim::Fault,
    };
    let stored_len = file.length() as usize;
    if stored_len > proto::cache::CACHE_CLAIM_MAX_BYTES {
        return StoredClaim::Absent;
    }
    let mut bytes = [0u8; proto::cache::CACHE_CLAIM_MAX_BYTES];
    if read_exact_file(&file, &mut bytes[..stored_len]).is_err() {
        return StoredClaim::Fault;
    }
    match proto::cache::decode_cache_claimant(&bytes[..stored_len]) {
        Some(claim) => {
            let mut locator = String::new();
            if locator.push_str(claim.locator).is_err() {
                return StoredClaim::Absent;
            }
            StoredClaim::Present {
                root: claim.root,
                locator,
                released: claim.released,
                evidence: claim.evidence,
            }
        }
        None => StoredClaim::Absent,
    }
}

/// Claim an open book directory for this owner: write the claim file so
/// every later access can prove whose directory it is.
fn write_book_dir_claim<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    book: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    released: bool,
    evidence: Option<&proto::cache::CacheEvidence>,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Every other claim write is about ownership, not about what the file is,
    // and must leave the evidence where it found it. A position write that
    // quietly erased the chain would take a later move's only witness with it.
    //
    // A card that would not answer is refused rather than read as a claim
    // with no evidence: that reading would let one transient read failure
    // destroy the witness during the very rewrite meant to preserve it.
    let carried;
    let evidence = match evidence {
        Some(evidence) => evidence,
        None => {
            carried = match read_stored_claim(book) {
                StoredClaim::Present { evidence, .. } => evidence,
                StoredClaim::Absent => proto::cache::CacheEvidence::default(),
                StoredClaim::Fault => return Err(()),
            };
            &carried
        }
    };
    let mut bytes = [0u8; proto::cache::CACHE_CLAIM_MAX_BYTES];
    let len =
        proto::cache::encode_cache_claim(owner.root, owner.locator, released, evidence, &mut bytes)
            .ok_or(())?;
    {
        let file = book
            .open_file_in_dir(
                proto::cache::CACHE_CLAIM_FILE,
                Mode::ReadWriteCreateOrTruncate,
            )
            .map_err(|_| ())?;
        file.write(&bytes[..len]).map_err(|_| ())?;
    }
    // Read it back, the way every other durable write here does, and compare
    // the whole claim rather than the evidence alone. A torn claim leaves the
    // position it gates on the card and unreadable, and evidence alone cannot
    // see that: a release keeps its evidence identical by design, so an
    // evidence-only check passes whether or not the released bit landed.
    //
    // Comparing the owner and the state is defensive rather than pinned. No
    // fault this layer models leaves the previous claim standing intact after
    // a write reports success, because the open truncates before the write,
    // so the reachable failures are a torn file or an outright error. The
    // check costs one comparison and does not depend on that staying true.
    match read_stored_claim(book) {
        StoredClaim::Present {
            root,
            locator,
            released: stored_released,
            evidence: stored_evidence,
        } => (root == owner.root
            && locator.as_str() == owner.locator
            && stored_released == released
            && stored_evidence == *evidence)
            .then_some(())
            .ok_or(()),
        StoredClaim::Absent | StoredClaim::Fault => Err(()),
    }
}

/// Open the book's cache directory (`READER/CACHE2/<key>`) with one handle
/// walked via `change_dir` — the single owner of that path walk. Opening a
/// directory another walk also passes through is fine: this embedded-sdmmc
/// rev allows duplicate directory opens (directories hold no cached
/// state); only deleting an open directory errors.
///
/// The directory is handed back only when its claim names this owner: the
/// key and every artifact identity are 32-bit hashes two legal books can
/// share, and the claim is what keeps a full-hash twin from reading the
/// other book's cache. An unclaimed directory is a miss here; only
/// [`claim_v2_book_dir`] may adopt one.
pub fn open_v2_book_dir<
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &'v Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) -> Option<Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut dir = root.open_dir(CACHE_ROOT_DIR).ok()?;
    dir.change_dir(CACHE_V2_DIR).ok()?;
    dir.change_dir(owner.key).ok()?;
    match book_dir_claim(&dir, owner) {
        ClaimState::MineActive => Some(dir),
        // A released mine holds no artifacts (the release emptied them);
        // everything else is either not this book's or not provable. All of
        // it is a miss, and the build path re-claims.
        _ => None,
    }
}

/// Open, creating if needed, the book's cache directory as a writer: verify
/// or establish the claim. A directory claimed by another book is refused,
/// so a full-hash twin cannot overwrite the holder's cache; the refused
/// book still reads, it just keeps nothing on the card while the twin
/// holds the key. An unclaimed directory is emptied of cache artifacts
/// before it is claimed, so a repaired or torn claim can never assert over
/// another book's leftovers. Positions survive the emptying, as they do
/// everywhere.
pub fn claim_v2_book_dir<
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &'v Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) -> Result<Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, ClaimDenied>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    {
        let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).map_err(|_| ClaimDenied::Fault)?;
        let cache = open_or_make_dir(&cache_root, CACHE_V2_DIR).map_err(|_| ClaimDenied::Fault)?;
        let _ = open_or_make_dir(&cache, owner.key).map_err(|_| ClaimDenied::Fault)?;
    }
    // Re-walk with one handle borrowing only the root, like
    // `open_v2_book_dir`, so the directory can be handed back.
    let mut book = root
        .open_dir(CACHE_ROOT_DIR)
        .map_err(|_| ClaimDenied::Fault)?;
    book.change_dir(CACHE_V2_DIR)
        .map_err(|_| ClaimDenied::Fault)?;
    book.change_dir(owner.key).map_err(|_| ClaimDenied::Fault)?;
    match book_dir_claim(&book, owner) {
        ClaimState::MineActive => Ok(book),
        // The sweep retired this directory while its owner was off the
        // card; the owner is back. Reactivating resumes the positions the
        // claim proves are its own.
        ClaimState::MineReleased => {
            write_book_dir_claim(&book, owner, false, None).map_err(|_| ClaimDenied::Fault)?;
            Ok(book)
        }
        ClaimState::OtherActive => Err(ClaimDenied::Foreign),
        // Adoptable, but the surviving positions provably belong to the
        // departed owner, and adopting a place in another book is the
        // wrong-book failure this whole layer exists to refuse. They go
        // with the adoption.
        ClaimState::OtherReleased => {
            if !empty_book_dir_artifacts(&book) || !remove_position_files(&book) {
                return Err(ClaimDenied::Fault);
            }
            write_book_dir_claim(&book, owner, false, None).map_err(|_| ClaimDenied::Fault)?;
            Ok(book)
        }
        // No evidence at all. Rebuildables go as before, and surviving
        // positions are ambiguous evidence nobody may adopt: with a claim
        // torn, whose they were is exactly the question that cannot be
        // answered.
        ClaimState::Unclaimed => {
            if !empty_book_dir_artifacts(&book) || !remove_position_files(&book) {
                return Err(ClaimDenied::Fault);
            }
            write_book_dir_claim(&book, owner, false, None).map_err(|_| ClaimDenied::Fault)?;
            Ok(book)
        }
        // Failure to read the claim is not evidence that there is no owner.
        ClaimState::Fault => Err(ClaimDenied::Fault),
    }
}

/// What a cache directory's claim says, read by key alone: for the sweep,
/// which has no expected owner and must ask who is there.
// The 257-byte locator dwarfs the other variants by design: it is the
// answer the sweep asked for, and one instance lives briefly on a shallow
// sweep frame, not in a collection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum DirClaimant {
    /// A well-formed claim; whether it was released, and who holds it.
    Claimed {
        root: proto::library_path::BookRoot,
        locator: String<{ proto::library_path::MAX_PATH_BYTES }>,
        released: bool,
        /// What the claim records about the physical file: the bytes a
        /// book's first open reads, which the scan's move search matches a
        /// departed copy by. Empty for a book nobody has opened long
        /// enough, and for a claim written before the field existed.
        evidence: proto::cache::CacheEvidence,
    },
    /// No claim, or bytes that are not one: the pre-claim compatibility
    /// shape, swept by identity as before.
    Unclaimed,
    /// The card would not answer. Not evidence; the sweep leaves the
    /// directory alone.
    Fault,
}

/// Read who claims `READER/CACHE2/<key>`, if anyone.
pub fn read_book_dir_claimant<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> DirClaimant
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut dir = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return DirClaimant::Unclaimed,
        Err(_) => return DirClaimant::Fault,
    };
    if let Err(error) = dir
        .change_dir(CACHE_V2_DIR)
        .and_then(|()| dir.change_dir(key))
    {
        return match error {
            embedded_sdmmc::Error::NotFound => DirClaimant::Unclaimed,
            _ => DirClaimant::Fault,
        };
    }
    let file = match dir.open_file_in_dir(proto::cache::CACHE_CLAIM_FILE, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return DirClaimant::Unclaimed,
        Err(_) => return DirClaimant::Fault,
    };
    let stored_len = file.length() as usize;
    if stored_len > proto::cache::CACHE_CLAIM_MAX_BYTES {
        return DirClaimant::Unclaimed;
    }
    let mut bytes = [0u8; proto::cache::CACHE_CLAIM_MAX_BYTES];
    if read_exact_file(&file, &mut bytes[..stored_len]).is_err() {
        return DirClaimant::Fault;
    }
    match proto::cache::decode_cache_claimant(&bytes[..stored_len]) {
        Some(claim) => {
            let mut locator = String::new();
            if locator.push_str(claim.locator).is_err() {
                return DirClaimant::Unclaimed;
            }
            DirClaimant::Claimed {
                root: claim.root,
                locator,
                released: claim.released,
                evidence: claim.evidence,
            }
        }
        None => DirClaimant::Unclaimed,
    }
}

/// Release a directory's claim: the sweep found its owner gone from the
/// card. The claim is rewritten, not deleted, so it keeps naming the owner:
/// a returning owner resumes its positions, and any other adopter knows the
/// surviving place is not its own. True when the release landed.
pub fn release_book_dir_claim<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let DirClaimant::Claimed {
        root: claim_root,
        locator,
        released,
        evidence,
    } = read_book_dir_claimant(root, key)
    else {
        return false;
    };
    if released {
        return true;
    }
    let Ok(mut dir) = root.open_dir(CACHE_ROOT_DIR) else {
        return false;
    };
    if dir.change_dir(CACHE_V2_DIR).is_err() || dir.change_dir(key).is_err() {
        return false;
    }
    let owner = proto::cache::CacheOwner {
        key,
        root: claim_root,
        locator: locator.as_str(),
    };
    // Hand back the evidence already in hand rather than letting the write
    // read it again: a second read is a second chance to lose it.
    write_book_dir_claim(&dir, &owner, true, Some(&evidence)).is_ok()
}

/// What a directory's claim says about one expected owner, for callers
/// outside this module that gate destructive operations: the row-selected
/// cache clear, most of all.
pub fn cache_dir_claim<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) -> ClaimState
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut dir = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return ClaimState::Unclaimed,
        Err(_) => return ClaimState::Fault,
    };
    if let Err(error) = dir
        .change_dir(CACHE_V2_DIR)
        .and_then(|()| dir.change_dir(owner.key))
    {
        return match error {
            embedded_sdmmc::Error::NotFound => ClaimState::Unclaimed,
            _ => ClaimState::Fault,
        };
    }
    book_dir_claim(&dir, owner)
}

/// Open the book's `CONT.BIN` (settings-independent content cache) and run
/// `f` with it.
pub fn with_v2_content_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let dir = open_v2_book_dir(root, owner)?;
    let file = dir.open_file_in_dir(CACHE_CONTENT_FILE, mode).ok()?;
    Some(f(&file))
}

/// Delete the book's `CONT.BIN`. Failures are ignored — a stale or corrupt
/// content cache is only ever an accelerator, and the next full build
/// recreates it.
pub fn delete_v2_content_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(dir) = open_v2_book_dir(root, owner) else {
        return;
    };
    let _ = upload_store::remove_file_reclaiming_clusters(&dir, CACHE_CONTENT_FILE);
}

/// Captures the build's `push_block` stream into `<key>/CONT.BIN` so a later
/// type-settings change replays it instead of re-reading and re-parsing the
/// EPUB. Failure is one-way and silent: the capture disables itself, and
/// `finish` deletes the partial file — CONT.BIN is purely an accelerator, so
/// the build itself never fails on its account.
pub struct ContentCapture<
    'd,
    's,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// `None` once disabled (setup or write failure).
    file: Option<File<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    stage: &'s mut [u8],
    len: usize,
    source_identity: (u32, u32),
    spine_count: u16,
}

impl<'d, 's, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    ContentCapture<'d, 's, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Create `CONT.BIN` in the book's cache dir (from `open_v2_content_dir`)
    /// and write its header with `complete = false`; the flag flips in
    /// `finish` only after the whole spine walk captured. Any failure — or
    /// `None` for the dir — returns a disabled capture.
    pub fn begin(
        dir: Option<&'d Directory<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        source_identity: (u32, u32),
        stage: &'s mut [u8],
    ) -> Self {
        let mut capture = Self {
            file: None,
            stage,
            len: 0,
            source_identity,
            spine_count: 0,
        };
        let Some(dir) = dir else {
            return capture;
        };
        let Ok(file) = dir.open_file_in_dir(CACHE_CONTENT_FILE, Mode::ReadWriteCreateOrTruncate)
        else {
            return capture;
        };
        let mut header = [0u8; CONTENT_HEADER_BYTES];
        let encoded = encode_content_header(
            ContentHeader {
                source_hash: source_identity.0,
                source_size: source_identity.1,
                complete: false,
                spine_count: 0,
                content_len: 0,
            },
            &mut header,
        );
        if encoded.is_ok() && file.write(&header).is_ok() {
            capture.file = Some(file);
        }
        capture
    }

    /// Re-open the `CONT.BIN` an earlier step of the same progressive build
    /// left behind, positioned to append. `spine_count` is what that step
    /// reported through [`Self::suspend`]; carrying it is what lets the final
    /// step write an accurate header.
    //
    /// The header is deliberately not touched here. It has said
    /// `complete = false` since [`Self::begin`] and stays that way until the
    /// walk actually ends, so a build abandoned between steps — by sleep, by
    /// the sync loan — leaves a file replay will refuse rather than a
    /// truncated stream it would trust.
    pub fn resume(
        dir: Option<&'d Directory<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        source_identity: (u32, u32),
        stage: &'s mut [u8],
        spine_count: u16,
    ) -> Self {
        let mut capture = Self {
            file: None,
            stage,
            len: 0,
            source_identity,
            spine_count,
        };
        let Some(dir) = dir else {
            return capture;
        };
        if let Ok(file) = dir.open_file_in_dir(CACHE_CONTENT_FILE, Mode::ReadWriteAppend) {
            capture.file = Some(file);
        }
        capture
    }

    /// Flush what is staged and hand the file back to the next step of the
    /// same build, reporting whether the capture is still healthy and how many
    /// spine groups it has recorded.
    //
    /// A `false` here is not a build failure — CONT.BIN is only ever an
    /// accelerator — but it is one-way: the next step starts
    /// [`disabled`](Self::disabled) rather than appending to a file with a
    /// hole in it, and the incomplete header keeps replay away from it.
    pub fn suspend(mut self) -> (bool, u16) {
        if let Some(file) = self.file.as_ref() {
            if staged_flush(file, self.stage, &mut self.len).is_err() {
                self.file = None;
            }
        }
        let healthy = self.file.is_some();
        drop(self.file.take());
        (healthy, self.spine_count)
    }

    /// A capture that records nothing, for a continuation whose earlier step
    /// already gave up on CONT.BIN. The build carries on unaffected; only the
    /// next settings change pays a full rebuild instead of a replay.
    pub fn disabled(stage: &'s mut [u8], source_identity: (u32, u32)) -> Self {
        Self {
            file: None,
            stage,
            len: 0,
            source_identity,
            spine_count: 0,
        }
    }

    /// Record one `push_block` call. The text follows the fixed record
    /// header; see `proto::cache::ContentRecordHeader`.
    pub fn push_block_record(
        &mut self,
        spine_index: u16,
        text: &str,
        role: proto::text::TextRole,
        style: proto::text::FontStyle,
        align: proto::text::TextAlign,
        paragraph_end: bool,
    ) {
        if self.file.is_none() {
            return;
        }
        let Ok(text_len) = u16::try_from(text.len()) else {
            self.file = None;
            return;
        };
        if text.len() > crate::READER_XHTML_SCRATCH {
            self.file = None;
            return;
        }
        let mut header = [0u8; CONTENT_RECORD_HEADER_BYTES];
        if encode_content_record_header(
            ContentRecordHeader {
                spine_index,
                text_len,
                role,
                style,
                align,
                paragraph_end,
                spine_end: false,
            },
            &mut header,
        )
        .is_err()
        {
            self.file = None;
            return;
        }

        self.stage_push(&header);
        self.stage_push(text.as_bytes());
    }

    /// Record the end of one spine item, so replay knows where to finish
    /// the current section run.
    pub fn spine_end(&mut self, spine_index: u16) {
        if self.file.is_none() {
            return;
        }
        self.spine_count = self.spine_count.saturating_add(1);
        let mut header = [0u8; CONTENT_RECORD_HEADER_BYTES];
        if encode_content_record_header(
            ContentRecordHeader {
                spine_index,
                text_len: 0,
                role: proto::text::TextRole::Body,
                style: proto::text::FontStyle::Regular,
                align: proto::text::TextAlign::Left,
                paragraph_end: false,
                spine_end: true,
            },
            &mut header,
        )
        .is_err()
        {
            self.file = None;
            return;
        }
        self.stage_push(&header);
    }

    /// Batch small record writes through the caller-owned staging buffer
    /// via the shared `staged_write`; any write failure disables the
    /// capture.
    fn stage_push(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        if staged_write(file, self.stage, &mut self.len, bytes).is_err() {
            self.file = None;
        }
    }

    /// Flush and mark the capture complete (`keep = true`, the whole spine
    /// walk captured cleanly), or delete the partial file through the same
    /// directory handle the capture was opened from. Returns whether a
    /// complete CONT.BIN was kept.
    pub fn finish(
        mut self,
        dir: Option<&Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        keep: bool,
    ) -> bool {
        let mut kept = false;
        if keep {
            if let Some(file) = self.file.as_ref() {
                if staged_flush(file, self.stage, &mut self.len).is_err() {
                    self.file = None;
                }
            }
            if let Some(file) = self.file.as_ref() {
                let file_len = file.length();
                let mut header = [0u8; CONTENT_HEADER_BYTES];
                kept = encode_content_header(
                    ContentHeader {
                        source_hash: self.source_identity.0,
                        source_size: self.source_identity.1,
                        complete: true,
                        spine_count: self.spine_count,
                        content_len: file_len,
                    },
                    &mut header,
                )
                .is_ok()
                    && file.seek_from_start(0).is_ok()
                    && file.write(&header).is_ok();
            }
        }
        drop(self.file.take());
        if !kept {
            if let Some(dir) = dir {
                let _ = upload_store::remove_file_reclaiming_clusters(dir, CACHE_CONTENT_FILE);
            }
        }
        kept
    }
}

/// Load only the labels (title/author) and the resident TOC copy from a
/// book's v2 index, accepting any layout config or custom-font identity:
/// the content replay path runs precisely when the index is layout-invalid,
/// but its TOC and labels are settings-independent and must survive into
/// the rewritten index. Deliberately does not touch the section index.
pub fn load_v2_book_labels_and_toc<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || !v2_toc_label_bounds_ok(&header)
        {
            return false;
        }
        let toc_offset =
            BOOK_V2_HEADER_BYTES + header.section_count as usize * BOOK_V2_SECTION_RECORD_BYTES;
        if file.seek_from_start(toc_offset as u32).is_err() {
            return false;
        }
        read_v2_toc_into_library(file, &header, library)
            && read_v2_labels_into_library(file, &header, library)
    })
    .unwrap_or(false)
}

fn with_v2_section_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    spine: u16,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let book_dir = open_v2_book_dir(root, owner)?;
    let sections = book_dir.open_dir(CACHE_SECTIONS_DIR).ok()?;
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(spine, &mut name);
    let file = sections.open_file_in_dir(name.as_str(), mode).ok()?;
    Some(f(&file))
}

fn with_v2_book_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let book_dir = open_v2_book_dir(root, owner)?;
    let file = book_dir.open_file_in_dir(CACHE_BOOK_FILE, mode).ok()?;
    Some(f(&file))
}

fn with_v2_toc_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let book_dir = open_v2_book_dir(root, owner)?;
    let file = book_dir.open_file_in_dir(CACHE_TOC_FILE, mode).ok()?;
    Some(f(&file))
}

/// Load the on-disk chapter list (TOC.BIN) into the store's text buffer for
/// the Chapters overview. Reuses the section text buffer -- the reading
/// section is reloaded on exit -- so no resident RAM is spent on the list.
pub fn load_v2_toc_into_text<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
    window_start: usize,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            cache_log!("toc window: header read failed");
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            cache_log!("toc window: header decode failed");
            return false;
        };
        if header.source_hash != source_identity.0 || header.source_size != source_identity.1 {
            cache_log!("toc window: identity mismatch");
            return false;
        }
        let total = header.chapter_count as usize;
        let start = window_start.min(total.saturating_sub(1));
        let len = (total - start).min(crate::store::TOC_WINDOW_CAPACITY);
        let offset = TOC_FILE_HEADER_BYTES + start * TOC_CHAPTER_RECORD_BYTES;
        if file.seek_from_start(offset as u32).is_err() {
            cache_log!("toc window: seek failed");
            return false;
        }
        let bytes = len.saturating_mul(TOC_CHAPTER_RECORD_BYTES);
        let Some(buf) = library.cached_text_mut(bytes) else {
            return false;
        };
        if read_exact_file(file, buf).is_err() {
            cache_log!("toc window: body read failed");
            return false;
        }
        library.set_toc_window(start, len, total);
        true
    })
    .unwrap_or(false)
}

/// Fill the resident per-section `chapter_start` marks from TOC.BIN, so the
/// firmware can resolve the current chapter for any reading page across the
/// whole book -- past the 128-entry resident/event caps and past chapter 255
/// (the map is bounded by the section count, not the chapter count). The
/// book index must already be loaded so spines resolve to sections.
pub fn load_v2_toc_chapter_map<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0 || header.source_size != source_identity.1 {
            return false;
        }
        let section_count = library.book_section_count.min(MAX_BOOK_SECTIONS);
        library.chapter_start.fill(0);
        library.chapter_start_ready = false;
        if !read_records_batched(
            file,
            TOC_CHAPTER_RECORD_BYTES,
            header.chapter_count as usize,
            |index, bytes| {
                let spine = i16::from_le_bytes([bytes[0], bytes[1]]);
                proto::cache::mark_chapter_start(
                    &mut library.chapter_start[..section_count],
                    &library.book_sections[..section_count],
                    index as u16,
                    spine,
                );
                true
            },
        ) {
            return false;
        }
        library.chapter_start_ready = true;
        true
    })
    .unwrap_or(false)
}

/// Read one chapter's title straight from its TOC.BIN record (a single seek
/// and 48-byte read) into the resident current-chapter slot, so the Home and
/// sleep colophons can name a chapter the 128-entry resident list omits.
pub fn read_v2_toc_chapter_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    chapter: u16,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, owner, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || chapter >= header.chapter_count
        {
            return false;
        }
        let offset = (TOC_FILE_HEADER_BYTES + chapter as usize * TOC_CHAPTER_RECORD_BYTES) as u32;
        if file.seek_from_start(offset).is_err() {
            return false;
        }
        let mut record = [0u8; TOC_CHAPTER_RECORD_BYTES];
        if read_exact_file(file, &mut record).is_err() {
            return false;
        }
        let Ok(parsed) = decode_toc_chapter(&record) else {
            return false;
        };
        library.set_current_chapter(chapter, parsed.title_str(), source_identity);
        true
    })
    .unwrap_or(false)
}

/// Write the full chapter list to TOC.BIN: a header plus `chapter_count`
/// pre-encoded `TOC_CHAPTER_RECORD_BYTES` records (the caller assembles them
/// in a scratch buffer during the TOC parse). Keeping the list on the card
/// lets a long book's TOC stay out of the tight reader RAM.
pub fn write_v2_toc_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    source_identity: (u32, u32),
    chapter_count: usize,
    records: &[u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if ensure_v2_cache_dirs(root, owner).is_err() {
        return false;
    }
    with_v2_toc_file(root, owner, Mode::ReadWriteCreateOrTruncate, |file| {
        let header = TocFileHeader {
            source_hash: source_identity.0,
            source_size: source_identity.1,
            chapter_count: chapter_count.min(u16::MAX as usize) as u16,
        };
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if encode_toc_file_header(header, &mut header_bytes).is_err()
            || file.write(&header_bytes).is_err()
        {
            return false;
        }
        !records.is_empty() && file.write(records).is_ok() || records.is_empty()
    })
    .unwrap_or(false)
}

fn with_v2_cover_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    owner: &proto::cache::CacheOwner<'_>,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let book_dir = open_v2_book_dir(root, owner)?;
    let file = book_dir.open_file_in_dir(CACHE_COVER_FILE, mode).ok()?;
    Some(f(&file))
}

fn load_v2_section_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: SectionV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let page_count = header.page_count as usize;
    let block_count = header.block_count as usize;
    let text_bytes = header.text_bytes as usize;
    if !library.can_hold_section(page_count, block_count, text_bytes) {
        return false;
    }
    library.clear_lines();
    if !read_records_batched(file, PAGE_RECORD_BYTES, page_count, |index, bytes| {
        let Ok(page) = decode_page(bytes) else {
            return false;
        };
        library.set_cached_page(index, page, header.spine)
    }) {
        return false;
    }
    if !read_records_batched(file, BLOCK_RECORD_BYTES, block_count, |index, bytes| {
        let Ok(block) = decode_block(bytes) else {
            return false;
        };
        library.set_cached_block(
            index,
            block,
            display_style_for_proto_style(block.style),
            header.spine,
        )
    }) {
        return false;
    }
    if !read_records_batched(file, 1, block_count, |index, bytes| {
        library.set_cached_paragraph_end(index, bytes[0] & 0b01 != 0)
            && library.set_cached_paragraph_start(index, bytes[0] & 0b10 != 0)
    }) {
        return false;
    }
    let Some(text) = library.cached_text_mut(text_bytes) else {
        return false;
    };
    if read_exact_file(file, text).is_err() {
        return false;
    }
    library.finish_cached_section(
        header.spine,
        page_count,
        block_count,
        text_bytes,
        header.partial,
    );
    true
}

fn write_v2_section_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
    spine: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let header = SectionV2Header {
        source_hash: source_identity.0,
        source_size: source_identity.1,
        spine,
        page_count: library.page_count.min(u16::MAX as usize) as u16,
        block_count: library.block_count.min(u16::MAX as usize) as u16,
        text_bytes: library.text_len.min(u32::MAX as usize) as u32,
        viewport_width: 800,
        viewport_height: 480,
        font_config: layout::reader_layout_config(library.type_settings(), library.portrait()),
        custom_font_identity: library.custom_font_identity(),
        bytes_consumed: 0,
        total_bytes: 0,
        partial: library.section_partial,
    };
    let mut bytes = [0u8; SECTION_V2_HEADER_BYTES];
    if encode_section_v2_header(header, &mut bytes).is_err() || file.write(&bytes).is_err() {
        cache_log!("cache: v2 write header failed");
        return false;
    }
    write_section_records(file, library)
}

fn write_section_records<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut record = [0u8; 16];
    let mut stage = WriteStage::new(file);
    for page in library.pages.iter().take(library.page_count) {
        if encode_page(*page, &mut record[..PAGE_RECORD_BYTES]).is_err()
            || stage.push(&record[..PAGE_RECORD_BYTES]).is_err()
        {
            cache_log!("cache: write page record failed");
            return false;
        }
    }
    for block in library.blocks.iter().take(library.block_count) {
        if encode_block(*block, &mut record[..BLOCK_RECORD_BYTES]).is_err()
            || stage.push(&record[..BLOCK_RECORD_BYTES]).is_err()
        {
            cache_log!("cache: write block record failed");
            return false;
        }
    }
    // One flag byte per block: bit 0 marks a paragraph end, bit 1 a
    // paragraph start (the indented opening line).
    for index in 0..library.block_count {
        let end = library.block_paragraph_end[index];
        let start = library.block_paragraph_start[index];
        let flag = (end as u8) | ((start as u8) << 1);
        if stage.push(&[flag]).is_err() {
            cache_log!("cache: write paragraph flag failed");
            return false;
        }
    }
    if stage.flush().is_err() {
        cache_log!("cache: write staged records failed");
        return false;
    }
    if file.write(&library.text[..library.text_len]).is_err() {
        cache_log!("cache: write text failed");
        return false;
    }
    true
}

/// Staging size for batched record reads. Kept small: this sits on the
/// stack inside the EPUB open path, in the same tight budget region.
const RECORD_STAGE_BYTES: usize = 256;

/// Append `bytes` to `file` through a caller-owned staging buffer: flush
/// the stage when the bytes don't fit the remaining capacity, and bypass
/// it entirely for writes at least as large as the whole buffer. The one
/// implementation of the batching arithmetic — `WriteStage` and
/// `ContentCapture` both delegate here.
fn staged_write<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &mut [u8],
    len: &mut usize,
    bytes: &[u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if bytes.len() > buf.len() - *len {
        staged_flush(file, buf, len)?;
    }
    if bytes.len() >= buf.len() {
        return file.write(bytes).map_err(|_| ());
    }
    buf[*len..*len + bytes.len()].copy_from_slice(bytes);
    *len += bytes.len();
    Ok(())
}

/// Write out whatever `staged_write` has accumulated. `len` resets even on
/// failure so a disabled writer can't replay stale bytes.
fn staged_flush<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &[u8],
    len: &mut usize,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if *len == 0 {
        return Ok(());
    }
    let result = file.write(&buf[..*len]).map_err(|_| ());
    *len = 0;
    result
}

/// Batch small writes through one staging buffer — the write-side twin of
/// `read_records_batched`. The FAT layer pays the same per-call overhead on
/// writes (block lookup plus a read-modify-write of the current sector), so
/// 1-16 byte record writes dominate section write time without it.
struct WriteStage<
    'f,
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'f File<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: [u8; RECORD_STAGE_BYTES],
    len: usize,
}

impl<'f, 'v, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    WriteStage<'f, 'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn new(file: &'f File<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> Self {
        Self {
            file,
            buf: [0u8; RECORD_STAGE_BYTES],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        staged_write(self.file, &mut self.buf, &mut self.len, bytes)
    }

    fn flush(&mut self) -> Result<(), ()> {
        staged_flush(self.file, &self.buf, &mut self.len)
    }
}

/// Read `count` fixed-size records through one staging buffer instead of
/// one embedded-sdmmc read call per record; the FAT layer pays per-call
/// overhead, so 4-16 byte reads dominate section and index load time.
fn read_records_batched<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record_len: usize,
    count: usize,
    mut apply: impl FnMut(usize, &[u8]) -> bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if record_len == 0 || record_len > RECORD_STAGE_BYTES {
        return false;
    }
    let mut stage = [0u8; RECORD_STAGE_BYTES];
    let per_batch = (RECORD_STAGE_BYTES / record_len) * record_len;
    let mut index = 0usize;
    while index < count {
        let take = ((count - index) * record_len).min(per_batch);
        if read_exact_file(file, &mut stage[..take]).is_err() {
            return false;
        }
        for chunk in stage[..take].chunks_exact(record_len) {
            if !apply(index, chunk) {
                return false;
            }
            index += 1;
        }
    }
    true
}

#[allow(clippy::result_unit_err)] // Nothing to report but failure: the card gives no distinguishable reason and every caller only branches on success.
pub fn read_exact_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut out: &mut [u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !out.is_empty() {
        let read = file.read(out).map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        let tmp = out;
        out = &mut tmp[read..];
    }
    Ok(())
}

fn display_style_for_proto_style(style: proto::text::FontStyle) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => FontStyle::Regular,
    }
}
