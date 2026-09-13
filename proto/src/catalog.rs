//! On-card CATALOG.BIN format: the header and fixed-size book records the
//! firmware's library scan writes and every list/lookup path reads back.
//! Lives here (not in firmware) so the encode/decode round-trip, the title
//! field layout, and the orphan-sweep identity staging are host-testable.

use crate::identity::{BookId, BOOK_ID_BYTES};
use crate::library_path::BookRoot;
use heapless::String;

pub const CATALOG_MAGIC: &[u8; 4] = b"X4CT";
/// v3 widened the on-disk book count from a single byte to a `u16` at
/// `header[5..7]`. v4 rebuilt stale records written before long filenames
/// were safely bounded. v5 appends a 64-byte title field to every record so
/// the Library list reads labels straight from the catalog instead of
/// probing each book's cache per window crossing; the version check makes
/// an older catalog fail to load, and a fresh scan rebuilds it -- no
/// migration code needed.
///
/// v6 changes no bytes at all: the record layout is identical to v5, and the
/// bump exists to retire the *contents* of every v5 catalog. Scans before it
/// catalogued macOS AppleDouble sidecars (`._<book>.epub`) as books, so a
/// card written by one holds a phantom, unopenable duplicate of every book
/// copied there in Finder. The scan filter alone would not reach those
/// cards: boot loads `CATALOG.BIN` and only queues a scan when the load
/// fails, so a v5 snapshot would keep serving its sidecar records until the
/// user forced a rescan. Bumping the version is how this format retires a
/// snapshot; see `is_hidden_entry` in `proto::storage` for the rule the
/// rebuild applies.
///
/// v7 replaces the 8.3 alias and the "which of the two directories" flag
/// with a root and a locator, since a book can now live at any depth and
/// neither field can say where. The root stays a separate field because a
/// locator is library-root relative by contract, and spelling the library
/// directory into it would give one type two coordinate systems.
///
/// v8 changes no bytes. It retires v7 snapshots whose `source_hash` came
/// from the 64-byte display path rather than the root plus the full
/// locator: a nested locator can spend that budget before the filename, so
/// two books could share a truncated label and, at equal sizes, an identity
/// and a cache. Rejecting v7 forces the rescan that rewrites records and
/// caches under one derivation.
///
/// v9 changes no bytes either. It retires v8 snapshots written by the
/// flat-only scan. Boot rescans only when a snapshot fails to load, so a
/// flat v8 catalog would keep serving a library with every nested book
/// missing until a manual refresh.
///
/// v10 appends the book's [`BookId`] to every record, cached from the
/// library ledger so the reading path never opens the ledger. The scan
/// writes rows without one and `upload_store::ledger::assign_book_ids`
/// fills them in before the header is committed, so a committed v10 catalog
/// has an id on every row. The rescan a v9 snapshot forces is where the
/// first ids are minted.
pub const CATALOG_VERSION: u8 = 10;
pub const CATALOG_HEADER_BYTES: usize = 8;
pub const CATALOG_RECORD_BYTES: usize = 435;
// The record is encoded into a fixed buffer of that width, and the fields are
// written at offsets derived from other modules' widths. These say so, rather
// than letting a widened locator or alias run past the end of the buffer or
// leave a silent gap in the middle of it.
const _: () =
    assert!(CATALOG_RECORD_PATH_OFFSET + CATALOG_PATH_BYTES == CATALOG_RECORD_ALIAS_OFFSET);
const _: () =
    assert!(CATALOG_RECORD_ALIAS_OFFSET + CATALOG_ALIAS_BYTES == CATALOG_RECORD_ID_OFFSET);
const _: () = assert!(CATALOG_RECORD_ID_OFFSET + BOOK_ID_BYTES == CATALOG_RECORD_BYTES);
/// Byte range of the cached [`BookId`], exposed so the identity join can
/// write just the id into a row the scan has already staged. All zero
/// until then, which decodes as no id.
pub const CATALOG_RECORD_ID_OFFSET: usize = 419;
/// Byte range of the title field inside a record, exposed so the firmware
/// can rewrite just the title in place when a book open learns the real
/// EPUB title.
pub const CATALOG_RECORD_TITLE_OFFSET: usize = 76;
pub const CATALOG_TITLE_BYTES: usize = 64;
/// Byte range of the locator, which is `LibraryPath` text.
const CATALOG_RECORD_PATH_OFFSET: usize = 140;
const CATALOG_PATH_BYTES: usize = crate::library_path::MAX_PATH_BYTES;
/// Byte range of the 8.3 alias an uploaded book landed under.
const CATALOG_RECORD_ALIAS_OFFSET: usize = 396;
const CATALOG_ALIAS_BYTES: usize = crate::storage::MAX_ALIAS_UTF8_BYTES;

/// One catalog record decoded into owned fields, so it outlives the file
/// handle it was read through.
pub struct CatalogRecord {
    /// The label a row falls back to when no title is known. Presentation
    /// only: identity and the cache key derive from `root`, `path`, and
    /// `byte_size` (see `proto::cache::source_hash_at`), because this field
    /// truncates at 64 bytes and a truncated label can collide where the
    /// locator does not.
    pub display_name: String<64>,
    /// Which directory `path` is relative to. `None` is a byte this build
    /// does not recognize, which makes the record unopenable rather than
    /// resolvable against the wrong root, where it could name a real but
    /// different file.
    pub root: Option<BookRoot>,
    /// Where the book is, as `LibraryPath` text. A locator, not identity:
    /// moving the file on a computer changes this and nothing else.
    pub path: String<{ crate::library_path::MAX_PATH_BYTES }>,
    /// The EPUB title learned when the book was last opened (or the upload
    /// label stashed at upload). Empty when unknown; readers fall back to a
    /// label derived from the file stem.
    pub title: String<64>,
    /// The 8.3 alias the book landed under, which is what names its upload
    /// label sidecar. Not how the book is opened: that is `path`. Wide enough
    /// for an alias of accented characters, which takes two bytes each.
    pub upload_alias: String<{ crate::storage::MAX_ALIAS_UTF8_BYTES }>,
    pub byte_size: u32,
    pub source_hash: u32,
    /// Which library copy this row is, cached from the ledger.
    ///
    /// `None` only in a row the scan has staged and the identity join has
    /// not reached, which a committed catalog does not contain: the join
    /// decides every row it commits, giving it a copy's id or one of its
    /// own.
    pub book_id: Option<BookId>,
}

/// The library root is 0, so the common book costs a zero byte and a record
/// whose root byte was lost reads as unopenable rather than as a card-root
/// book that is not there.
const ROOT_LIBRARY: u8 = 0;
const ROOT_CARD: u8 = 1;

pub(crate) const fn root_byte(root: BookRoot) -> u8 {
    match root {
        BookRoot::Library => ROOT_LIBRARY,
        BookRoot::CardRoot => ROOT_CARD,
    }
}

pub(crate) const fn book_root(byte: u8) -> Option<BookRoot> {
    match byte {
        ROOT_LIBRARY => Some(BookRoot::Library),
        ROOT_CARD => Some(BookRoot::CardRoot),
        _ => None,
    }
}

/// The most books one catalog can name, because the header counts them in
/// two bytes.
pub const CATALOG_MAX_BOOKS: usize = u16::MAX as usize;

/// The header count for a scan that found `books`, or `None` for a card
/// holding more than this format can name.
///
/// Not a clamp, deliberately. A committed catalog is the whole book set: the
/// list reads no further than the count, and the orphan sweep treats every
/// identity outside it as garbage. Writing the first
/// [`CATALOG_MAX_BOOKS`] and calling that a complete catalog would hide the
/// rest of the library and hand the sweep the caches of books still on the
/// card, which is the failure
/// [`encode_catalog_placeholder_header`] exists to prevent for an
/// interrupted write. A card past the limit is a scan that fails, and a
/// snapshot that fails to load is a rescan.
pub fn catalog_count(books: usize) -> Option<u16> {
    u16::try_from(books).ok()
}

pub fn encode_catalog_header(count: u16, out: &mut [u8; CATALOG_HEADER_BYTES]) {
    out.fill(0);
    out[..4].copy_from_slice(CATALOG_MAGIC);
    out[4] = CATALOG_VERSION;
    out[5..7].copy_from_slice(&count.to_le_bytes());
}

/// A deliberately invalid header, written first and left in place while
/// records are still landing: version 0 is never accepted by readers, and
/// for a non-empty partial file a count of 0 cannot agree with the file
/// length either. The writer commits the real header by seeking back only
/// after every record is durable, so a scan interrupted mid-write leaves a
/// catalog that fails to load (and triggers a fresh scan) instead of one
/// that quietly truncates the library — or worse, drives the orphan sweep
/// into reclaiming caches of books that are still on the card.
pub fn encode_catalog_placeholder_header(out: &mut [u8; CATALOG_HEADER_BYTES]) {
    out.fill(0);
    out[..4].copy_from_slice(CATALOG_MAGIC);
}

/// The exact byte length of a catalog holding `count` records; readers
/// reject files whose length disagrees with their committed header.
pub fn catalog_file_len(count: u16) -> usize {
    CATALOG_HEADER_BYTES + count as usize * CATALOG_RECORD_BYTES
}

/// Why a header did not decode, for callers that report rather than just
/// rebuild. Both lead to the same fresh scan; they differ only in whether
/// anything went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogHeaderFault {
    /// Right magic, a version this build does not read, and not the
    /// placeholder: a catalog written by other firmware. Bumping
    /// `CATALOG_VERSION` *is* how this format migrates — the old snapshot
    /// stops loading and a scan rebuilds it — so this is the designed first
    /// boot after an upgrade, not damage.
    Stale,
    /// Wrong magic, or the version-0 placeholder a scan leaves in place while
    /// its records are still landing. Either means this is not a catalog the
    /// reader may trust, and the second means a scan was interrupted.
    Invalid,
}

/// The book count, or why the header was rejected. Callers that only need to
/// know whether to rescan can use [`decode_catalog_header`].
pub fn classify_catalog_header(
    header: &[u8; CATALOG_HEADER_BYTES],
) -> Result<u16, CatalogHeaderFault> {
    if &header[..4] != CATALOG_MAGIC {
        return Err(CatalogHeaderFault::Invalid);
    }
    let version = header[4];
    if version == CATALOG_VERSION {
        return Ok(u16::from_le_bytes([header[5], header[6]]));
    }
    // Version 0 is the placeholder, so it is a torn write rather than an
    // older format, however much the two look alike from here.
    Err(if version == 0 {
        CatalogHeaderFault::Invalid
    } else {
        CatalogHeaderFault::Stale
    })
}

/// The book count, or `None` when the magic or version doesn't match (the
/// caller then runs a fresh scan).
pub fn decode_catalog_header(header: &[u8; CATALOG_HEADER_BYTES]) -> Option<u16> {
    classify_catalog_header(header).ok()
}

#[allow(clippy::too_many_arguments)]
pub fn encode_catalog_record(
    out: &mut [u8; CATALOG_RECORD_BYTES],
    display_name: &str,
    root: BookRoot,
    path: &str,
    title: &str,
    upload_alias: &str,
    byte_size: u32,
    source_hash: u32,
) {
    out.fill(0);
    out[0] = root_byte(root);
    out[4..8].copy_from_slice(&byte_size.to_le_bytes());
    out[8..12].copy_from_slice(&source_hash.to_le_bytes());
    copy_fixed(display_name.as_bytes(), &mut out[12..76]);
    copy_fixed(
        title.as_bytes(),
        &mut out[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES],
    );
    copy_fixed(
        path.as_bytes(),
        &mut out[CATALOG_RECORD_PATH_OFFSET..CATALOG_RECORD_PATH_OFFSET + CATALOG_PATH_BYTES],
    );
    copy_fixed(
        upload_alias.as_bytes(),
        &mut out[CATALOG_RECORD_ALIAS_OFFSET..CATALOG_RECORD_ALIAS_OFFSET + CATALOG_ALIAS_BYTES],
    );
}

pub fn decode_catalog_record(record: &[u8; CATALOG_RECORD_BYTES]) -> CatalogRecord {
    let mut display_name = String::<64>::new();
    let _ = display_name.push_str(fixed_str(&record[12..76]));
    let mut title = String::<64>::new();
    let _ = title.push_str(fixed_str(
        &record[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES],
    ));
    let root = book_root(record[0]);
    let mut path = String::new();
    let _ = path.push_str(fixed_str(
        &record[CATALOG_RECORD_PATH_OFFSET..CATALOG_RECORD_PATH_OFFSET + CATALOG_PATH_BYTES],
    ));
    let mut upload_alias = String::new();
    let _ = upload_alias.push_str(fixed_str(
        &record[CATALOG_RECORD_ALIAS_OFFSET..CATALOG_RECORD_ALIAS_OFFSET + CATALOG_ALIAS_BYTES],
    ));
    CatalogRecord {
        display_name,
        root,
        path,
        title,
        upload_alias,
        byte_size: u32::from_le_bytes([record[4], record[5], record[6], record[7]]),
        source_hash: u32::from_le_bytes([record[8], record[9], record[10], record[11]]),
        book_id: catalog_record_book_id(record),
    }
}

/// The `(source_hash, byte_size)` identity of an encoded record.
pub fn catalog_record_identity(record: &[u8; CATALOG_RECORD_BYTES]) -> (u32, u32) {
    (
        u32::from_le_bytes([record[8], record[9], record[10], record[11]]),
        u32::from_le_bytes([record[4], record[5], record[6], record[7]]),
    )
}

/// The cached [`BookId`] of an encoded record, or `None` for a row the
/// identity join has not reached.
pub fn catalog_record_book_id(record: &[u8; CATALOG_RECORD_BYTES]) -> Option<BookId> {
    let mut bytes = [0u8; BOOK_ID_BYTES];
    bytes.copy_from_slice(
        &record[CATALOG_RECORD_ID_OFFSET..CATALOG_RECORD_ID_OFFSET + BOOK_ID_BYTES],
    );
    BookId::from_bytes(bytes)
}

/// The place of an encoded record, borrowed from its bytes: root, locator,
/// and the size the scan saw. `None` for a root byte this build does not
/// know, which is a row nothing may adopt or open.
///
/// For a caller comparing many rows against one place; decoding copies four
/// strings it would not read.
pub fn catalog_record_at(record: &[u8; CATALOG_RECORD_BYTES]) -> Option<(BookRoot, &str, u32)> {
    let root = book_root(record[0])?;
    let locator = fixed_str(
        &record[CATALOG_RECORD_PATH_OFFSET..CATALOG_RECORD_PATH_OFFSET + CATALOG_PATH_BYTES],
    );
    let byte_size = u32::from_le_bytes([record[4], record[5], record[6], record[7]]);
    Some((root, locator, byte_size))
}

/// The `(source_hash, byte_size)` identity pre-v8 firmware would have given
/// this record's book, when its display shape is one that firmware could
/// address; `None` for nested books, which have no pre-v8 identity to
/// answer to. Derived from the stored display name through the frozen rule
/// in [`crate::cache::legacy_source_hash`].
pub fn catalog_record_legacy_identity(record: &[u8; CATALOG_RECORD_BYTES]) -> Option<(u32, u32)> {
    let byte_size = u32::from_le_bytes([record[4], record[5], record[6], record[7]]);
    let display_name = fixed_str(&record[12..76]);
    Some((
        crate::cache::legacy_source_hash(display_name, byte_size)?,
        byte_size,
    ))
}

/// Resolves a current identity against records, offered one at a time in
/// catalog order, with the same one-match discipline as
/// [`LegacyIdentityScan`]: exactly one record may answer.
///
/// The identity is a 32-bit hash of the root, locator, and size, and two
/// legal locators can share it; a verified pair exists in the tests below.
/// Accepting the first or the hinted match would open one book as the
/// other, and the next save would persist that choice. Zero matches is a
/// book no longer on the card; two or more is a question the identity
/// cannot answer, so resolution refuses rather than guesses.
pub struct IdentityScan {
    wanted: (u32, u32),
    found: Option<u16>,
    matches: usize,
}

impl IdentityScan {
    pub fn new(source_hash: u32, byte_size: u32) -> Self {
        Self {
            wanted: (source_hash, byte_size),
            found: None,
            matches: 0,
        }
    }

    pub fn offer(&mut self, index: u16, record: &[u8; CATALOG_RECORD_BYTES]) {
        if catalog_record_identity(record) == self.wanted {
            self.matches += 1;
            if self.found.is_none() {
                self.found = Some(index);
            }
        }
    }

    pub fn finish(self) -> Option<u16> {
        if self.matches == 1 {
            self.found
        } else {
            None
        }
    }
}

/// Which catalogued row holds the book at an exact place.
///
/// For a caller that has a locator, which is every caller acting on a row a
/// reader just picked. [`IdentityScan`] is for the other case, where the
/// only persisted thing is a 32-bit identity.
///
/// The distinction is not stylistic: that identity is a lossy projection of
/// this locator, and two legal locators at one size can share it, so
/// resolving a live row through it makes both books unopenable.
///
/// The size matched is the listing's live one. A record that disagrees was
/// written before the file was replaced, and would open the old book's
/// metadata over the new book's bytes.
pub struct LocatorScan<'a> {
    root: BookRoot,
    locator: &'a str,
    byte_size: u32,
    found: Option<u16>,
    matches: usize,
}

impl<'a> LocatorScan<'a> {
    pub fn new(root: BookRoot, locator: &'a str, byte_size: u32) -> Self {
        Self {
            root,
            locator,
            byte_size,
            found: None,
            matches: 0,
        }
    }

    pub fn offer(&mut self, index: u16, record: &[u8; CATALOG_RECORD_BYTES]) {
        if u32::from_le_bytes([record[4], record[5], record[6], record[7]]) != self.byte_size {
            return;
        }
        if book_root(record[0]) != Some(self.root) {
            return;
        }
        let stored = fixed_str(
            &record[CATALOG_RECORD_PATH_OFFSET..CATALOG_RECORD_PATH_OFFSET + CATALOG_PATH_BYTES],
        );
        if stored != self.locator {
            return;
        }
        self.matches += 1;
        if self.found.is_none() {
            self.found = Some(index);
        }
    }

    /// Exactly one row, or nothing. A walk of a filesystem cannot produce
    /// one path twice, so two matches mean a catalog this has no model for,
    /// and refusing is what the rest of this module does when it stops
    /// understanding what it is reading.
    pub fn finish(self) -> Option<u16> {
        (self.matches == 1).then_some(self.found).flatten()
    }
}

/// Whether a candidate sits on the chain the departed claim recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainMatch {
    /// The same first cluster. On this filesystem a move within one volume
    /// rewrites directory entries and leaves the chain alone, so this is the
    /// same physical file under a new name.
    Same,
    /// A different chain, or a claim that recorded none. Whatever holds
    /// these bytes, it is not the file the claim was watching.
    Different,
    /// The card would not say.
    Unknown,
}

/// What a finished move search concludes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveVerdict {
    /// This row is the departed copy under a new name.
    Moved(u16),
    /// Nothing on the card is this copy.
    Gone,
    /// The card holds an answer this cannot read as one book.
    Undecided,
}

/// Decide from the rows whose bytes agreed with the witness.
///
/// Nothing here returns [`MoveVerdict::Moved`], which is the finding rather
/// than an oversight: a move is a claim about *which copy* a file is, and
/// neither fact available says that.
///
/// Bytes say which book. Delete a book that had a twin and exactly one row
/// agrees, and that row has its own reading life.
///
/// The chain says which cluster, and clusters are reused. Delete the book
/// and a file written afterwards can be handed its first cluster; a freed
/// cluster carries no record of who held it, so equality cannot separate
/// the file that kept it from the file that inherited it.
///
/// So the chain only refuses. It is consulted after the bytes have picked
/// out one row, which stops it choosing between two, and there it separates
/// an unreadable answer from an answer: a card that would not say leaves
/// the claim standing, one that answered lets it go.
///
/// Concluding a move needs a durable record of which copy is which, written
/// before the operation, which is the opaque book identity the Library
/// Identity work owns.
pub fn move_verdict(agreements: &[(u16, ChainMatch)]) -> MoveVerdict {
    match agreements {
        [] => MoveVerdict::Gone,
        // A chain that matches is a chain that may have been recycled, so
        // this is the same answer as a chain that does not.
        [(_, ChainMatch::Same)] | [(_, ChainMatch::Different)] => MoveVerdict::Gone,
        [(_, ChainMatch::Unknown)] => MoveVerdict::Undecided,
        _ => MoveVerdict::Undecided,
    }
}

/// The most same-size books one search will read through before it gives up
/// on telling them apart. A card holding more copies than this of one book
/// refuses rather than reading all of them.
pub const MOVE_CANDIDATES_MAX: usize = 4;

/// Which catalogued rows could be the departed book.
///
/// Gathers rather than chooses: the caller reads the bytes of every row it
/// hands back, and [`move_verdict`] decides. Length is the only filter, and
/// it comes free with the witness. Narrowing by the claim's recorded chain
/// would let a recycled cluster rule out the real continuation and leave
/// the bytes to be read from a stranger.
pub struct MoveSearch {
    want_bytes: u64,
    found: heapless::Vec<u16, MOVE_CANDIDATES_MAX>,
    seen: usize,
}

impl MoveSearch {
    pub fn new(witness: &crate::source::CachedSourceDigest) -> Self {
        Self {
            want_bytes: witness.byte_len(),
            found: heapless::Vec::new(),
            seen: 0,
        }
    }

    /// Offer one catalogued row the caller has already established is
    /// neither the departed book itself nor a directory holding a place of
    /// its own.
    pub fn offer(&mut self, index: u16, byte_size: u32) {
        if u64::from(byte_size) != self.want_bytes {
            return;
        }
        self.seen += 1;
        let _ = self.found.push(index);
    }

    /// The rows to read bytes from.
    ///
    /// Two answers rather than a list and an absence, because declining to
    /// look is not the same fact as having looked and found nothing, and the
    /// caller retires a reader's place on the second one.
    pub fn finish(self) -> MoveCandidates {
        if self.seen > MOVE_CANDIDATES_MAX {
            MoveCandidates::TooMany
        } else {
            MoveCandidates::Rows(self.found)
        }
    }
}

/// What a [`MoveSearch`] has to say when it is done.
pub enum MoveCandidates {
    /// Every row of the witnessed length, to be read in turn. Empty means
    /// the card holds no book of that length, which is a book that is gone.
    Rows(heapless::Vec<u16, MOVE_CANDIDATES_MAX>),
    /// More rows of that length than this will read through. It has looked
    /// at none of their bytes, so it knows nothing about where the book is,
    /// and in particular has not established that it left.
    TooMany,
}

/// Resolves a pre-v8 saved identity against v8 records, offered one at a
/// time in catalog order.
///
/// The global state record written by pre-v8 firmware still decodes after
/// the upgrade, carrying the identity that firmware derived, while the
/// rebuilt catalog holds only v8 identities. Restoring the active book
/// therefore needs this second reading. Exactly one record may answer:
/// zero is a book no longer on the card, and two or more is a guess
/// between real books, which restoration refuses rather than opens.
pub struct LegacyIdentityScan {
    wanted: (u32, u32),
    found: Option<u16>,
    matches: usize,
}

impl LegacyIdentityScan {
    pub fn new(source_hash: u32, byte_size: u32) -> Self {
        Self {
            wanted: (source_hash, byte_size),
            found: None,
            matches: 0,
        }
    }

    pub fn offer(&mut self, index: u16, record: &[u8; CATALOG_RECORD_BYTES]) {
        if catalog_record_legacy_identity(record) == Some(self.wanted) {
            self.matches += 1;
            if self.found.is_none() {
                self.found = Some(index);
            }
        }
    }

    pub fn finish(self) -> Option<u16> {
        if self.matches == 1 {
            self.found
        } else {
            None
        }
    }
}

/// Encode `title` into a standalone 64-byte title field, for rewriting the
/// field in place at `CATALOG_RECORD_TITLE_OFFSET` within a record.
pub fn encode_catalog_title(title: &str, out: &mut [u8; CATALOG_TITLE_BYTES]) {
    out.fill(0);
    copy_fixed(title.as_bytes(), out);
}

/// Bytes one staged `(source_hash, byte_size)` identity occupies in the
/// orphan sweep's scratch region.
pub const CATALOG_IDENTITY_BYTES: usize = 8;

/// Stage identity `index` into `scratch` for the orphan sweep's in-RAM
/// membership checks. Returns false (staging nothing) past capacity.
pub fn stage_catalog_identity(scratch: &mut [u8], index: usize, hash: u32, size: u32) -> bool {
    let at = index * CATALOG_IDENTITY_BYTES;
    let Some(slot) = scratch.get_mut(at..at + CATALOG_IDENTITY_BYTES) else {
        return false;
    };
    slot[..4].copy_from_slice(&hash.to_le_bytes());
    slot[4..].copy_from_slice(&size.to_le_bytes());
    true
}

/// Sort the first `count` staged identities so `catalog_identity_staged`
/// can binary-search them. Heapsort over the 8-byte entries: in place, no
/// allocation, no recursion -- O(N log N) with N bounded by the scratch
/// capacity (~2,048 identities in the 16 KB arena).
pub fn sort_catalog_identities(scratch: &mut [u8], count: usize) {
    let count = count.min(scratch.len() / CATALOG_IDENTITY_BYTES);
    for parent in (0..count / 2).rev() {
        sift_down_identity(scratch, parent, count);
    }
    for end in (1..count).rev() {
        swap_identities(scratch, 0, end);
        sift_down_identity(scratch, 0, end);
    }
}

/// Whether `(hash, size)` is among the first `count` staged identities,
/// which must already be ordered by `sort_catalog_identities` -- the check
/// is a binary search, O(log N) per cache dir instead of a linear scan. A
/// zero identity never matches, mirroring the streamed catalog lookup that
/// refuses to resolve `(0, 0)`.
pub fn catalog_identity_staged(scratch: &[u8], count: usize, hash: u32, size: u32) -> bool {
    if hash == 0 && size == 0 {
        return false;
    }
    if count > scratch.len() / CATALOG_IDENTITY_BYTES {
        return false;
    }
    let key = identity_key(hash, size);
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match staged_key(scratch, mid).cmp(&key) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return true,
        }
    }
    false
}

/// The sort/search key for one identity: the staged slot's little-endian
/// bytes as a `u64`, so `identity_key(hash, size)` and `staged_key` agree
/// byte for byte with what `stage_catalog_identity` wrote.
fn identity_key(hash: u32, size: u32) -> u64 {
    u64::from(hash) | (u64::from(size) << 32)
}

fn staged_key(scratch: &[u8], index: usize) -> u64 {
    let at = index * CATALOG_IDENTITY_BYTES;
    let mut bytes = [0u8; CATALOG_IDENTITY_BYTES];
    bytes.copy_from_slice(&scratch[at..at + CATALOG_IDENTITY_BYTES]);
    u64::from_le_bytes(bytes)
}

fn swap_identities(scratch: &mut [u8], a: usize, b: usize) {
    for offset in 0..CATALOG_IDENTITY_BYTES {
        scratch.swap(
            a * CATALOG_IDENTITY_BYTES + offset,
            b * CATALOG_IDENTITY_BYTES + offset,
        );
    }
}

fn sift_down_identity(scratch: &mut [u8], mut parent: usize, end: usize) {
    loop {
        let mut child = parent * 2 + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && staged_key(scratch, child) < staged_key(scratch, child + 1) {
            child += 1;
        }
        if staged_key(scratch, parent) >= staged_key(scratch, child) {
            return;
        }
        swap_identities(scratch, parent, child);
        parent = child;
    }
}

fn copy_fixed(src: &[u8], dst: &mut [u8]) {
    let len = src.len().min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
}

fn fixed_str(bytes: &[u8]) -> &str {
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scan stages a row with no id; the join writes one into the field
    /// the offset names, and every reader of the row sees it. The place
    /// comes back borrowed, exactly as encoded.
    #[test]
    fn a_row_carries_the_id_written_at_its_offset_and_none_before() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "spqr",
            BookRoot::Library,
            "History/Rome/SPQR.epub",
            "SPQR",
            "SPQR~1.EPU",
            2_048,
            0xDEAD_BEEF,
        );
        assert_eq!(catalog_record_book_id(&record), None);
        assert_eq!(decode_catalog_record(&record).book_id, None);
        assert_eq!(
            catalog_record_at(&record),
            Some((BookRoot::Library, "History/Rome/SPQR.epub", 2_048))
        );

        let id = BookId::from_bytes([0x5A; BOOK_ID_BYTES]).unwrap();
        record[CATALOG_RECORD_ID_OFFSET..CATALOG_RECORD_ID_OFFSET + BOOK_ID_BYTES]
            .copy_from_slice(&id.to_bytes());
        assert_eq!(catalog_record_book_id(&record), Some(id));
        let decoded = decode_catalog_record(&record);
        assert_eq!(decoded.book_id, Some(id));
        // The id sits past every other field, so none of them moved.
        assert_eq!(decoded.upload_alias.as_str(), "SPQR~1.EPU");
        assert_eq!(decoded.path.as_str(), "History/Rome/SPQR.epub");
        assert_eq!(catalog_record_identity(&record), (0xDEAD_BEEF, 2_048));

        record[0] = 0x7F;
        assert_eq!(
            catalog_record_at(&record),
            None,
            "an unknown root is no place"
        );
    }

    /// The two collision fixtures, as catalog records. Same size, same
    /// 32-bit identity, different places on the card.
    fn colliding_twins() -> (u32, [u8; CATALOG_RECORD_BYTES], [u8; CATALOG_RECORD_BYTES]) {
        let size = 1_234_567;
        let a = "Fiction/zIx6RBhQEK.epub";
        let b = "Fiction/nTfOyBwYzX.epub";
        let hash = crate::cache::source_hash_at(BookRoot::Library, a, size);
        assert_eq!(
            hash,
            crate::cache::source_hash_at(BookRoot::Library, b, size),
            "the collision these fixtures exist for"
        );
        let mut twin_a = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(&mut twin_a, "a", BookRoot::Library, a, "", "", size, hash);
        let mut twin_b = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(&mut twin_b, "b", BookRoot::Library, b, "", "", size, hash);
        (size, twin_a, twin_b)
    }

    /// The two lookups answer differently on purpose, and this is the case
    /// that separates them. Restoring a saved identity has only 32 bits and
    /// a size, so with both books on the card it has to refuse. A reader who
    /// picked one of them out of a folder listing named it exactly, and
    /// there is nothing to be ambiguous about.
    #[test]
    fn a_picked_row_resolves_where_a_saved_identity_has_to_refuse() {
        let (size, twin_a, twin_b) = colliding_twins();
        let hash = catalog_record_identity(&twin_a).0;

        let mut saved = IdentityScan::new(hash, size);
        saved.offer(0, &twin_a);
        saved.offer(1, &twin_b);
        assert_eq!(saved.finish(), None, "32 bits cannot choose between them");

        for (index, locator) in [
            (0, "Fiction/zIx6RBhQEK.epub"),
            (1, "Fiction/nTfOyBwYzX.epub"),
        ] {
            let mut picked = LocatorScan::new(BookRoot::Library, locator, size);
            picked.offer(0, &twin_a);
            picked.offer(1, &twin_b);
            assert_eq!(picked.finish(), Some(index), "{locator} opens its own row");
        }
    }

    /// The root is part of the place. Two cards' worth of books can carry the
    /// same relative locator, and the shelf copy is not the card-root one.
    #[test]
    fn the_same_locator_under_another_root_is_another_book() {
        let mut shelved = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut shelved,
            "d",
            BookRoot::Library,
            "Dune.epub",
            "",
            "",
            40,
            7,
        );
        let mut loose = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut loose,
            "d",
            BookRoot::CardRoot,
            "Dune.epub",
            "",
            "",
            40,
            9,
        );

        let mut scan = LocatorScan::new(BookRoot::CardRoot, "Dune.epub", 40);
        scan.offer(0, &shelved);
        scan.offer(1, &loose);
        assert_eq!(scan.finish(), Some(1));
    }

    /// A record written before the file was replaced names the right place
    /// and the wrong book, so the live size refuses it rather than opening
    /// the old book's metadata over the new book's bytes.
    #[test]
    fn a_record_stale_about_the_size_does_not_answer_for_the_file_there_now() {
        let mut stale = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut stale,
            "d",
            BookRoot::Library,
            "Dune.epub",
            "",
            "",
            40,
            7,
        );

        let mut scan = LocatorScan::new(BookRoot::Library, "Dune.epub", 41);
        scan.offer(0, &stale);
        assert_eq!(scan.finish(), None);
    }

    /// A walk of a filesystem cannot produce one path twice, so a catalog
    /// that names a place twice is one this code has no model for. It
    /// refuses rather than opening whichever record it happened to read
    /// first, which is the same answer the identity lookups give when they
    /// stop being able to tell two records apart.
    #[test]
    fn a_catalog_naming_one_place_twice_refuses() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "d",
            BookRoot::Library,
            "Dune.epub",
            "",
            "",
            40,
            7,
        );

        let mut scan = LocatorScan::new(BookRoot::Library, "Dune.epub", 40);
        scan.offer(0, &record);
        scan.offer(1, &record);
        assert_eq!(scan.finish(), None);
    }

    /// A locator that is a prefix of a catalogued one is a different place.
    /// The stored field is fixed width and zero padded, so a comparison that
    /// read the whole field rather than its text would match nothing, and one
    /// that stopped at the shorter length would match too much.
    #[test]
    fn a_locator_that_merely_starts_the_same_is_not_a_match() {
        let mut nested = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut nested,
            "d",
            BookRoot::Library,
            "Fiction/Dune.epub",
            "",
            "",
            40,
            7,
        );

        for probe in ["Fiction/Dune", "Fiction/Dune.epub2", "Fiction"] {
            let mut scan = LocatorScan::new(BookRoot::Library, probe, 40);
            scan.offer(0, &nested);
            assert_eq!(scan.finish(), None, "{probe} is not the nested book");
        }

        let mut exact = LocatorScan::new(BookRoot::Library, "Fiction/Dune.epub", 40);
        exact.offer(0, &nested);
        assert_eq!(exact.finish(), Some(0));
    }

    /// A digest of real bytes, since that is the only way to hold one.
    fn witness(bytes: &[u8], repeat: usize) -> crate::source::CachedSourceDigest {
        let mut hasher = crate::source::SourceHasher::new();
        for _ in 0..repeat {
            hasher.update(bytes);
        }
        crate::source::CachedSourceDigest::new(hasher.finish())
    }

    /// A reader is in A, an identical B sits on the card untouched, and A is
    /// deleted. Exactly one row now holds those bytes, and it is a book with
    /// a reading life of its own that was there the whole time. Bytes say
    /// which book, not which copy.
    #[test]
    fn a_lone_twin_does_not_inherit_a_deleted_books_place() {
        assert_eq!(
            move_verdict(&[(7, ChainMatch::Different)]),
            MoveVerdict::Gone,
            "the twin keeps its own reading life",
        );
    }

    /// And the recorded chain does not rescue it. Delete the book, and the
    /// cluster it held is free for the next file; a copy of the same book
    /// written afterwards can be handed it, and then it matches the claim on
    /// bytes and on chain while being a file that did not exist when the
    /// claim was written. A freed cluster carries no record of who held it,
    /// so equality with the recorded number proves nothing about which copy
    /// this is.
    #[test]
    fn a_recycled_chain_cannot_authorise_a_carry_either() {
        assert_eq!(
            move_verdict(&[(7, ChainMatch::Same)]),
            MoveVerdict::Gone,
            "a matching cluster number is not a matching copy",
        );
    }

    /// Which leaves nothing that says a book moved. That is the shape of the
    /// milestone without a durable per-copy identity, and it is pinned so
    /// the conclusion cannot drift back in one branch at a time.
    #[test]
    fn no_arrangement_of_evidence_concludes_a_move() {
        for chain in [ChainMatch::Same, ChainMatch::Different, ChainMatch::Unknown] {
            assert!(
                !matches!(move_verdict(&[(1, chain)]), MoveVerdict::Moved(_)),
                "{chain:?} authorised a carry",
            );
            for other in [ChainMatch::Same, ChainMatch::Different, ChainMatch::Unknown] {
                assert!(
                    !matches!(
                        move_verdict(&[(1, chain), (2, other)]),
                        MoveVerdict::Moved(_)
                    ),
                    "{chain:?} beside {other:?} authorised a carry",
                );
            }
        }
    }

    /// Two rows holding the bytes is undecided before the chain is consulted
    /// at all, which is what stops a freed chain handed to a newer file from
    /// selecting that file over the real continuation.
    #[test]
    fn the_chain_is_asked_last_and_can_only_refuse() {
        assert_eq!(
            move_verdict(&[(3, ChainMatch::Same), (9, ChainMatch::Different)]),
            MoveVerdict::Undecided,
            "a chain match does not break a tie the bytes could not break",
        );
        assert_eq!(
            move_verdict(&[(3, ChainMatch::Different), (9, ChainMatch::Different)]),
            MoveVerdict::Undecided
        );
    }

    /// A card that would not say which chain a row sits on has not said the
    /// book left, so the claim stays for the next scan.
    #[test]
    fn an_unreadable_chain_leaves_the_question_open() {
        assert_eq!(
            move_verdict(&[(7, ChainMatch::Unknown)]),
            MoveVerdict::Undecided
        );
    }

    /// Nothing holds the bytes, so nothing on the card is this copy.
    #[test]
    fn no_agreement_is_a_book_that_is_gone() {
        assert_eq!(move_verdict(&[]), MoveVerdict::Gone);
    }

    /// Two rows of the length the witness records are two books that have to
    /// be read before either can be chosen, and the chain the claim recorded
    /// is not allowed to break the tie. A computer can delete the book, free
    /// that chain, and hand it to a file created afterwards, and then the row
    /// the chain points at is a stranger whose bytes agree for the ordinary
    /// reason that it is another copy of the same book. Ruling the others out
    /// on it would carry a reader's place to that stranger.
    #[test]
    fn every_row_of_the_length_is_gathered_for_the_bytes_to_judge() {
        let w = witness(b"x", 8);
        let mut search = MoveSearch::new(&w);
        search.offer(3, 8);
        search.offer(9, 8);
        let MoveCandidates::Rows(rows) = search.finish() else {
            panic!("within the cap")
        };
        assert_eq!(
            rows.as_slice(),
            &[3, 9],
            "both are read; neither is ruled out unread"
        );
    }

    /// More copies than the search will read through. It cannot show that
    /// only one of them is this book without reading all of them, so it
    /// declines rather than reading an unbounded number or choosing early.
    #[test]
    fn more_copies_than_it_will_read_is_a_refusal() {
        let w = witness(b"x", 8);
        let mut search = MoveSearch::new(&w);
        for index in 0..=MOVE_CANDIDATES_MAX as u16 {
            search.offer(index, 8);
        }
        assert!(matches!(search.finish(), MoveCandidates::TooMany));

        let mut just_fits = MoveSearch::new(&w);
        for index in 0..MOVE_CANDIDATES_MAX as u16 {
            just_fits.offer(index, 8);
        }
        let MoveCandidates::Rows(rows) = just_fits.finish() else {
            panic!("exactly the cap is not too many")
        };
        assert_eq!(rows.len(), 4);
    }

    /// The length comes from the witness, and it is the only filter, so a row
    /// of another size is not gathered and every row of that size is.
    #[test]
    fn only_rows_of_the_witnessed_length_are_gathered() {
        let w = witness(b"x", 8);
        let mut search = MoveSearch::new(&w);
        search.offer(1, 9);
        let MoveCandidates::Rows(none) = search.finish() else {
            panic!("within the cap")
        };
        assert!(none.is_empty());

        let mut right = MoveSearch::new(&w);
        right.offer(1, 8);
        let MoveCandidates::Rows(one) = right.finish() else {
            panic!("within the cap")
        };
        assert_eq!(one.as_slice(), &[1]);
    }

    /// Nothing offered is nothing found. A departed book is usually a
    /// deleted book.
    #[test]
    fn an_empty_search_finds_nothing() {
        let w = witness(b"x", 8);
        let MoveCandidates::Rows(rows) = MoveSearch::new(&w).finish() else {
            panic!("nothing offered is not too many")
        };
        assert!(rows.is_empty());
    }

    /// The count is where a library too large for the format has to stop.
    /// Clamping here would publish the first 65,535 books as the whole set,
    /// which hides the rest and tells the orphan sweep that their caches
    /// belong to nothing.
    #[test]
    fn a_library_past_the_count_field_has_no_catalog_rather_than_a_short_one() {
        assert_eq!(catalog_count(0), Some(0));
        assert_eq!(catalog_count(CATALOG_MAX_BOOKS), Some(u16::MAX));
        assert_eq!(catalog_count(CATALOG_MAX_BOOKS + 1), None);
    }

    #[test]
    fn header_roundtrips_and_rejects_other_versions() {
        let mut header = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_header(1234, &mut header);
        assert_eq!(decode_catalog_header(&header), Some(1234));

        // The version byte is the migration mechanism: an old catalog fails
        // the decode and the caller rescans.
        let mut stale = header;
        stale[4] = CATALOG_VERSION - 1;
        assert_eq!(decode_catalog_header(&stale), None);

        let mut wrong_magic = header;
        wrong_magic[0] = b'Y';
        assert_eq!(decode_catalog_header(&wrong_magic), None);
    }

    #[test]
    fn placeholder_header_never_decodes() {
        let mut header = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_placeholder_header(&mut header);
        assert_eq!(decode_catalog_header(&header), None);
    }

    #[test]
    fn header_faults_separate_another_version_from_damage() {
        // All four rescan; only the version cases are the format doing its
        // job. A reader that reports must not call the designed migration a
        // fault, or the first boot after every CATALOG_VERSION bump fails a
        // strict capture.
        let mut header = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_header(1234, &mut header);
        assert_eq!(classify_catalog_header(&header), Ok(1234));

        let mut stale = header;
        stale[4] = CATALOG_VERSION - 1;
        assert_eq!(
            classify_catalog_header(&stale),
            Err(CatalogHeaderFault::Stale)
        );

        // A version this build has never seen is still just a version.
        let mut newer = header;
        newer[4] = CATALOG_VERSION + 1;
        assert_eq!(
            classify_catalog_header(&newer),
            Err(CatalogHeaderFault::Stale)
        );

        // The placeholder shares the magic and means an interrupted scan.
        let mut placeholder = [0u8; CATALOG_HEADER_BYTES];
        encode_catalog_placeholder_header(&mut placeholder);
        assert_eq!(
            classify_catalog_header(&placeholder),
            Err(CatalogHeaderFault::Invalid)
        );

        let mut wrong_magic = header;
        wrong_magic[0] = b'Y';
        assert_eq!(
            classify_catalog_header(&wrong_magic),
            Err(CatalogHeaderFault::Invalid)
        );
    }

    #[test]
    fn file_len_matches_header_plus_records() {
        assert_eq!(catalog_file_len(0), CATALOG_HEADER_BYTES);
        assert_eq!(
            catalog_file_len(3),
            CATALOG_HEADER_BYTES + 3 * CATALOG_RECORD_BYTES
        );
    }

    #[test]
    fn record_roundtrips_all_fields_including_title() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/books/wuthering-heights.epub",
            BookRoot::Library,
            "Bront\u{eb}/wuthering-heights.epub",
            "Wuthering Heights",
            "WUTHE~01.EPU",
            123_456,
            0xdead_beef,
        );
        let decoded = decode_catalog_record(&record);
        assert_eq!(
            decoded.display_name.as_str(),
            "/books/wuthering-heights.epub"
        );
        // Library-root relative: the library directory's own name is not a
        // component, so the record and a listing of it agree.
        assert_eq!(decoded.root, Some(BookRoot::Library));
        assert_eq!(decoded.path.as_str(), "Bront\u{eb}/wuthering-heights.epub");
        assert_eq!(decoded.title.as_str(), "Wuthering Heights");
        assert_eq!(decoded.upload_alias.as_str(), "WUTHE~01.EPU");
        assert_eq!(decoded.byte_size, 123_456);
        assert_eq!(decoded.source_hash, 0xdead_beef);
        assert_eq!(catalog_record_identity(&record), (0xdead_beef, 123_456));
    }

    #[test]
    fn empty_title_decodes_empty_for_the_stem_fallback() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/plain.epub",
            BookRoot::CardRoot,
            "plain.epub",
            "",
            "PLAIN.EPU",
            9,
            7,
        );
        let decoded = decode_catalog_record(&record);
        assert!(decoded.title.is_empty());
        assert_eq!(decoded.root, Some(BookRoot::CardRoot));
        assert_eq!(decoded.path.as_str(), "plain.epub");
    }

    #[test]
    fn overlong_fields_truncate_to_their_budgets() {
        // Over every field budget, including the widest.
        let long = "x".repeat(CATALOG_PATH_BYTES + 10);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            &long,
            BookRoot::Library,
            &long,
            &long,
            &long,
            1,
            2,
        );
        let decoded = decode_catalog_record(&record);
        assert_eq!(decoded.display_name.len(), 64);
        assert_eq!(decoded.title.len(), 64);
        assert_eq!(decoded.path.len(), CATALOG_PATH_BYTES);
        assert_eq!(decoded.upload_alias.len(), CATALOG_ALIAS_BYTES);
    }

    #[test]
    fn an_unknown_root_makes_a_record_unopenable_rather_than_misplaced() {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/books/d.epub",
            BookRoot::Library,
            "d.epub",
            "",
            "D.EPU",
            1,
            2,
        );
        record[0] = 0x7f;

        // Resolving "d.epub" against the wrong root can name a real file that
        // is not this book, so an unreadable root answers nothing at all.
        assert_eq!(decode_catalog_record(&record).root, None);
    }

    #[test]
    fn title_field_rewrite_in_place_matches_a_full_reencode() {
        // The book-open path patches only the 64-byte title field; it must
        // land exactly where a from-scratch encode puts the title.
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut record,
            "/b.epub",
            BookRoot::Library,
            "b.epub",
            "",
            "B.EPU",
            10,
            20,
        );
        let mut field = [0u8; CATALOG_TITLE_BYTES];
        encode_catalog_title("Bleak House", &mut field);
        record[CATALOG_RECORD_TITLE_OFFSET..CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES]
            .copy_from_slice(&field);

        let mut expected = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut expected,
            "/b.epub",
            BookRoot::Library,
            "b.epub",
            "Bleak House",
            "B.EPU",
            10,
            20,
        );
        assert_eq!(record, expected);
        assert_eq!(decode_catalog_record(&record).title.as_str(), "Bleak House");
    }

    #[test]
    fn staged_identities_answer_membership_like_a_catalog_walk() {
        // Staged deliberately out of key order: membership must come from
        // the sort, not the staging order.
        let mut scratch = [0u8; 64];
        assert!(stage_catalog_identity(&mut scratch, 0, 0xcccc, 300));
        assert!(stage_catalog_identity(&mut scratch, 1, 0xaaaa, 100));
        assert!(stage_catalog_identity(&mut scratch, 2, 0xbbbb, 200));
        sort_catalog_identities(&mut scratch, 3);

        assert!(catalog_identity_staged(&scratch, 3, 0xbbbb, 200));
        assert!(!catalog_identity_staged(&scratch, 3, 0xbbbb, 201));
        assert!(
            !catalog_identity_staged(&scratch, 1, 0xbbbb, 200),
            "past count"
        );
        // The zero identity never matches: an unreadable cache header must
        // not accidentally resolve to a zeroed record.
        assert!(stage_catalog_identity(&mut scratch, 3, 0, 0));
        sort_catalog_identities(&mut scratch, 4);
        assert!(!catalog_identity_staged(&scratch, 4, 0, 0));
    }

    #[test]
    fn sorted_lookup_matches_a_linear_scan_over_many_identities() {
        // Enough entries (descending, with duplicates) that the binary
        // search crosses several pivot levels on both hit and miss paths.
        const N: usize = 33;
        // Adjacent staged indices share one key, so every key appears twice
        // (bar one) and the search must still find it.
        fn identity_for(index: usize) -> (u32, u32) {
            let v = (N as u32 - index as u32).div_ceil(2);
            (v * 3, v % 5)
        }
        let mut scratch = [0u8; N * CATALOG_IDENTITY_BYTES];
        for index in 0..N {
            let (hash, size) = identity_for(index);
            assert!(stage_catalog_identity(&mut scratch, index, hash, size));
        }
        sort_catalog_identities(&mut scratch, N);

        for index in 0..N {
            let (hash, size) = identity_for(index);
            assert!(
                catalog_identity_staged(&scratch, N, hash, size),
                "staged identity {index} must be found after sorting"
            );
            // Same hash, different size: the full 8-byte key must match.
            assert!(!catalog_identity_staged(&scratch, N, hash, size + 7));
        }
        assert!(!catalog_identity_staged(&scratch, N, 1, 1));
    }

    #[test]
    fn staging_past_capacity_reports_the_overflow() {
        let mut scratch = [0u8; CATALOG_IDENTITY_BYTES * 2];
        assert!(stage_catalog_identity(&mut scratch, 0, 1, 1));
        assert!(stage_catalog_identity(&mut scratch, 1, 2, 2));
        assert!(!stage_catalog_identity(&mut scratch, 2, 3, 3));
    }

    /// A v8 record for a book pre-v8 firmware could address, alongside the
    /// identity that firmware would have saved for it.
    fn flat_record(display: &str, size: u32) -> ([u8; CATALOG_RECORD_BYTES], u32) {
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        let name = display.rsplit('/').next().unwrap_or(display);
        encode_catalog_record(
            &mut record,
            display,
            BookRoot::Library,
            name,
            "",
            "",
            size,
            crate::cache::source_hash_at(BookRoot::Library, name, size),
        );
        let old_hash =
            crate::cache::legacy_source_hash(display, size).expect("flat shape has a legacy hash");
        (record, old_hash)
    }

    /// A record's pre-v8 identity is reconstructible exactly when its
    /// display shape is one old firmware could have produced.
    #[test]
    fn a_records_legacy_identity_follows_its_display_shape() {
        let (record, old_hash) = flat_record("/books/Book.epub", 12_345);
        assert_eq!(
            catalog_record_legacy_identity(&record),
            Some((old_hash, 12_345)),
        );

        let mut nested = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut nested,
            "/books/fiction/dune.epub",
            BookRoot::Library,
            "fiction/dune.epub",
            "",
            "",
            12_345,
            crate::cache::source_hash_at(BookRoot::Library, "fiction/dune.epub", 12_345),
        );
        assert_eq!(
            catalog_record_legacy_identity(&nested),
            None,
            "a nested book has no pre-v8 identity to answer to",
        );
    }

    /// The reviewer's upgrade sequence: a pre-v8 state record's identity
    /// against a v8 catalog holding the same flat book under its new
    /// identity must resolve to that book, and only that shape may answer.
    #[test]
    fn a_pre_v8_saved_identity_resolves_against_the_v8_catalog() {
        let (dune, old_dune) = flat_record("/books/dune.epub", 4_096);
        let (other, _) = flat_record("/books/other.epub", 9_000);
        // A nested book whose display text and size mirror the flat one as
        // closely as v8 allows; it must not be eligible.
        let mut nested = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut nested,
            "/books/shelf/dune.epub",
            BookRoot::Library,
            "shelf/dune.epub",
            "",
            "",
            4_096,
            crate::cache::source_hash_at(BookRoot::Library, "shelf/dune.epub", 4_096),
        );

        let mut scan = LegacyIdentityScan::new(old_dune, 4_096);
        scan.offer(0, &other);
        scan.offer(1, &nested);
        scan.offer(2, &dune);
        assert_eq!(scan.finish(), Some(2), "the flat book resolves");

        // Two records answering the same legacy identity is a guess between
        // real books, which restoration refuses.
        let mut ambiguous = LegacyIdentityScan::new(old_dune, 4_096);
        ambiguous.offer(0, &dune);
        ambiguous.offer(1, &dune);
        assert_eq!(ambiguous.finish(), None);

        // An identity naming nothing on the card stays a miss.
        let mut missing = LegacyIdentityScan::new(old_dune ^ 1, 4_096);
        missing.offer(0, &dune);
        assert_eq!(missing.finish(), None);
    }

    /// The current rule collides with itself too: this pair of legal
    /// library locators shares one 32-bit identity at one size. A lookup
    /// that trusted a hint or the first match could resolve a saved
    /// identity to the other book after the catalog was rebuilt in a
    /// different order, and the next save would persist the swap. One
    /// match resolves; two refuse.
    #[test]
    fn a_same_domain_hash_collision_refuses_rather_than_guesses() {
        let size = 1_234_567;
        let hash_a =
            crate::cache::source_hash_at(BookRoot::Library, "Fiction/zIx6RBhQEK.epub", size);
        let hash_b =
            crate::cache::source_hash_at(BookRoot::Library, "Fiction/nTfOyBwYzX.epub", size);
        assert_eq!(hash_a, hash_b, "the collision this test exists for");

        let mut twin_a = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut twin_a,
            "/books/fiction/zix6rbhqek.epub",
            BookRoot::Library,
            "Fiction/zIx6RBhQEK.epub",
            "",
            "",
            size,
            hash_a,
        );
        let mut twin_b = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut twin_b,
            "/books/fiction/ntfoybwyzx.epub",
            BookRoot::Library,
            "Fiction/nTfOyBwYzX.epub",
            "",
            "",
            size,
            hash_b,
        );

        let mut alone = IdentityScan::new(hash_a, size);
        alone.offer(0, &twin_a);
        assert_eq!(alone.finish(), Some(0), "one match resolves");

        let mut twins = IdentityScan::new(hash_a, size);
        twins.offer(0, &twin_a);
        twins.offer(1, &twin_b);
        assert_eq!(
            twins.finish(),
            None,
            "two rows answering one identity is a guess between real books",
        );
    }

    /// The two hash domains genuinely collide: this pair of legal books
    /// shares one 32-bit value across the pre-v8 and v8 rules. An
    /// unversioned saved identity read under the current rule first would
    /// restore the unrelated nested book and then persist that mistake at
    /// the next save. The record's version byte says which rule wrote the
    /// identity, and the legacy reading cannot see the nested book at all.
    #[test]
    fn a_cross_domain_hash_collision_cannot_steal_a_restoration() {
        let size = 1_234_567;
        let legacy_a = crate::cache::legacy_source_hash("/books/6HawKebl.epub", size)
            .expect("flat shape has a legacy identity");
        let current_b =
            crate::cache::source_hash_at(BookRoot::Library, "Fiction/XpipGrkt.epub", size);
        assert_eq!(legacy_a, current_b, "the collision this test exists for");

        let (a, old_a) = flat_record("/books/6HawKebl.epub", size);
        assert_eq!(old_a, legacy_a);
        let mut b = [0u8; CATALOG_RECORD_BYTES];
        encode_catalog_record(
            &mut b,
            "/books/fiction/xpipgrkt.epub",
            BookRoot::Library,
            "Fiction/XpipGrkt.epub",
            "",
            "",
            size,
            current_b,
        );
        // The nested book owns the colliding value as its current identity.
        assert_eq!(catalog_record_identity(&b), (legacy_a, size));

        // Read under the rule that wrote the saved identity, only A answers.
        let mut scan = LegacyIdentityScan::new(legacy_a, size);
        scan.offer(0, &b);
        scan.offer(1, &a);
        assert_eq!(
            scan.finish(),
            Some(1),
            "the legacy reading finds the flat book, not the collider",
        );
    }
}
