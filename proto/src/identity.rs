//! Persistent identity for one library copy of a book, and the on-card
//! ledger format that makes it durable.
//!
//! Three facts about a book are easy to run together, and this crate keeps
//! them apart. Where its file is, which is a locator
//! ([`crate::library_path::LibraryPath`]). Which bytes it holds, which is a
//! [`crate::source::SourceDigest`]. And which user-visible copy it is, which
//! is a [`BookId`]: random, opaque, minted once when a physical copy is
//! adopted into the library, and kept across renames and moves. Two
//! byte-identical files are two ids with independent user state, and moving
//! a file changes its locator and nothing else.
//!
//! A `BookId` is deliberately derived from nothing. A path is where a file
//! was. A digest says which book, and a card can hold the same book twice.
//! A FAT cluster is recycled. So the id cannot be rebuilt from the card, and
//! the record binding it to a copy is durable user state rather than a
//! cache. That is the ledger: fixed-size records that each carry their own
//! checksum, under a header that is committed last, in two alternating files
//! so that an interrupted rewrite loses only the generation it was writing.
//! The I/O lives in `upload-store`; this module is the bytes.
//!
//! `CATALOG.BIN` caches the id beside each row so nothing on the reading path
//! has to read the ledger. The catalog is rebuildable and the ledger is not:
//! a rebuild reads the ledger once, joins it to the fresh rows by place, and
//! mints ids only for rows no record names.

use crate::catalog::{book_root, root_byte};
use crate::library_path::{BookRoot, MAX_PATH_BYTES};
use crate::source::{CachedSourceDigest, SourceDigest, SHA256_BYTES};

/// Bytes in a [`BookId`].
pub const BOOK_ID_BYTES: usize = 16;

/// The identity of one library copy.
///
/// Ordered so that ids can be sorted and searched; the order means nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BookId([u8; BOOK_ID_BYTES]);

impl BookId {
    /// Mint a fresh id.
    ///
    /// `random` must be a hardware or otherwise unpredictable source: two
    /// copies handed the same id are one book to everything above this
    /// layer, and nothing here can tell. The all-zero id encodes "no id" on
    /// the card, so a draw that lands on it is drawn again.
    pub fn mint(random: &mut impl FnMut() -> u32) -> Self {
        loop {
            let mut bytes = [0u8; BOOK_ID_BYTES];
            let mut at = 0;
            while at < BOOK_ID_BYTES {
                bytes[at..at + 4].copy_from_slice(&random().to_le_bytes());
                at += 4;
            }
            if let Some(id) = Self::from_bytes(bytes) {
                return id;
            }
        }
    }

    /// An id read back off the card. `None` for the all-zero bytes, which is
    /// how a row or record says it has none.
    pub const fn from_bytes(bytes: [u8; BOOK_ID_BYTES]) -> Option<Self> {
        let mut at = 0;
        while at < BOOK_ID_BYTES {
            if bytes[at] != 0 {
                return Some(Self(bytes));
            }
            at += 1;
        }
        None
    }

    pub const fn to_bytes(self) -> [u8; BOOK_ID_BYTES] {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Ledger format
// ---------------------------------------------------------------------------

pub const LEDGER_MAGIC: [u8; 4] = *b"X4LG";
/// A ledger of a version this build does not read is refused, not rebuilt:
/// unlike the catalog, its contents cannot come back from the card. So a
/// change to this format is a migration, a reader for the old layout that
/// writes the new one, and a bump alone is not one.
pub const LEDGER_VERSION: u8 = 1;
pub const LEDGER_HEADER_BYTES: usize = 16;
pub const LEDGER_RECORD_BYTES: usize = 325;

// Header: magic | version | reserved (zero) | count u16 | generation u32 |
// checksum u32 over everything before it.
const HEADER_VERSION: usize = 4;
const HEADER_RESERVED: usize = 5;
const HEADER_COUNT: usize = 6;
const HEADER_GENERATION: usize = 8;
const HEADER_CHECKSUM: usize = 12;
const _: () = assert!(HEADER_CHECKSUM + 4 == LEDGER_HEADER_BYTES);

// Record: id | root | misses | locator length u16 | locator, zero padded |
// byte size at adoption u32 | digest present u8 | digest byte length u64 |
// sha256 | checksum u32 over everything before it.
const RECORD_ID: usize = 0;
const RECORD_ROOT: usize = BOOK_ID_BYTES;
const RECORD_MISSES: usize = RECORD_ROOT + 1;
const RECORD_LOCATOR_LEN: usize = RECORD_MISSES + 1;
const RECORD_LOCATOR: usize = RECORD_LOCATOR_LEN + 2;
const RECORD_SIZE: usize = RECORD_LOCATOR + MAX_PATH_BYTES;
const RECORD_HAS_SOURCE: usize = RECORD_SIZE + 4;
const RECORD_SOURCE_LEN: usize = RECORD_HAS_SOURCE + 1;
const RECORD_SOURCE_SHA: usize = RECORD_SOURCE_LEN + 8;
const RECORD_CHECKSUM: usize = RECORD_SOURCE_SHA + SHA256_BYTES;
const _: () = assert!(RECORD_CHECKSUM + 4 == LEDGER_RECORD_BYTES);

/// One adopted copy: which id it carries, the place and size it had when
/// the record was written, how long it has been missing from there, and
/// which bytes it held when they were last read whole.
///
/// The size is the cheap evidence a rebuild has that the file at a known
/// locator is still the file that was adopted there. It is a filter and
/// not proof: a same-sized replacement passes it, and only a source digest
/// can say otherwise. It is enough to keep a different book dropped at a
/// known path by a computer from inheriting the id of the one that was
/// there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerRecord<'a> {
    pub id: BookId,
    pub root: BookRoot,
    /// Root-relative, exactly as the catalog stores it.
    pub locator: &'a str,
    pub byte_size: u32,
    /// Consecutive scans in which no row on the card was this place. Zero
    /// for a copy that is there. A record is carried while this stays
    /// within [`MISSING_SCANS_RETAINED`] and left behind once it does not,
    /// which is what keeps the ledger near the size of the live library
    /// rather than the size of every book that ever passed through it.
    pub misses: u8,
    /// The identity of the copy's bytes, when something has read them: an
    /// upload hashes what it streams, and a managed replacement records what
    /// landed. `None` for a copy adopted from the card by place alone, whose
    /// digest is computed only when something needs it. Evidence rather
    /// than fact once it is on the card, which the type says.
    pub source: Option<CachedSourceDigest>,
}

/// How many consecutive scans a copy may be missing before its record is
/// left out of the next generation.
///
/// A missing record is what lets a book that was taken off the card and put
/// back, or moved while the card was in a computer, be recognised as the
/// copy it was rather than adopted afresh; the reconciliation that does the
/// recognising is later work, and this keeps its evidence for it. The bound
/// is what the identity design asks for and leaves to the implementation:
/// a scan runs once per change to the card, so this is eight card edits
/// during which the book stayed away. Missing records are also the first to
/// go when a generation would not fit; see [`carry_missing`].
pub const MISSING_SCANS_RETAINED: u8 = 8;

/// Whether a record that named no row this scan is carried into the next
/// generation, and with what `misses`.
///
/// `room` is how many more records the generation can hold once every live
/// copy and every newly adopted one is in it. Live and new copies are what
/// the library is, so they are counted first and a missing record only
/// takes a slot that is left. Without that, a card that once held the most
/// books a header can count and then lost them all would carry every stale
/// record forever, and the first book added afterwards could not be
/// adopted at all.
pub fn carry_missing(misses: u8, room: usize) -> Option<u8> {
    let aged = misses.saturating_add(1);
    (aged <= MISSING_SCANS_RETAINED && room > 0).then_some(aged)
}

/// Encode one record. `None` when the locator does not fit, which a legal
/// [`crate::library_path::LibraryPath`] cannot reach.
pub fn encode_ledger_record(
    record: &LedgerRecord<'_>,
    out: &mut [u8; LEDGER_RECORD_BYTES],
) -> Option<()> {
    let locator = record.locator.as_bytes();
    if locator.len() > MAX_PATH_BYTES {
        return None;
    }
    out.fill(0);
    out[RECORD_ID..RECORD_ROOT].copy_from_slice(&record.id.to_bytes());
    out[RECORD_ROOT] = root_byte(record.root);
    out[RECORD_MISSES] = record.misses;
    out[RECORD_LOCATOR_LEN..RECORD_LOCATOR].copy_from_slice(&(locator.len() as u16).to_le_bytes());
    out[RECORD_LOCATOR..RECORD_LOCATOR + locator.len()].copy_from_slice(locator);
    out[RECORD_SIZE..RECORD_HAS_SOURCE].copy_from_slice(&record.byte_size.to_le_bytes());
    if let Some(source) = &record.source {
        out[RECORD_HAS_SOURCE] = 1;
        out[RECORD_SOURCE_LEN..RECORD_SOURCE_SHA].copy_from_slice(&source.byte_len().to_le_bytes());
        out[RECORD_SOURCE_SHA..RECORD_CHECKSUM].copy_from_slice(source.sha256());
    }
    let sum = fnv1a(&out[..RECORD_CHECKSUM]);
    out[RECORD_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
    Some(())
}

/// Decode one record, its locator borrowed from the stored bytes. `None` for
/// anything that is not a record this build wrote: a failed checksum, an id
/// of zero, a root byte it does not know, a locator length past the field,
/// or locator bytes that are not UTF-8.
pub fn decode_ledger_record(bytes: &[u8; LEDGER_RECORD_BYTES]) -> Option<LedgerRecord<'_>> {
    let stored = u32::from_le_bytes(bytes[RECORD_CHECKSUM..].try_into().ok()?);
    if fnv1a(&bytes[..RECORD_CHECKSUM]) != stored {
        return None;
    }
    let id = BookId::from_bytes(bytes[RECORD_ID..RECORD_ROOT].try_into().ok()?)?;
    let root = book_root(bytes[RECORD_ROOT])?;
    let len =
        u16::from_le_bytes([bytes[RECORD_LOCATOR_LEN], bytes[RECORD_LOCATOR_LEN + 1]]) as usize;
    if len > MAX_PATH_BYTES {
        return None;
    }
    let locator = core::str::from_utf8(&bytes[RECORD_LOCATOR..RECORD_LOCATOR + len]).ok()?;
    let byte_size = u32::from_le_bytes(bytes[RECORD_SIZE..RECORD_HAS_SOURCE].try_into().ok()?);
    let source = match bytes[RECORD_HAS_SOURCE] {
        0 => None,
        1 => Some(CachedSourceDigest::new(SourceDigest::from_parts(
            u64::from_le_bytes(
                bytes[RECORD_SOURCE_LEN..RECORD_SOURCE_SHA]
                    .try_into()
                    .ok()?,
            ),
            bytes[RECORD_SOURCE_SHA..RECORD_CHECKSUM].try_into().ok()?,
        ))),
        _ => return None,
    };
    Some(LedgerRecord {
        id,
        root,
        locator,
        byte_size,
        misses: bytes[RECORD_MISSES],
        source,
    })
}

/// What a committed generation says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerHeader {
    /// Compared with [`crate::durable::generation_is_newer`], so the counter
    /// survives wrapping.
    pub generation: u32,
    pub count: u16,
}

pub fn encode_ledger_header(header: LedgerHeader, out: &mut [u8; LEDGER_HEADER_BYTES]) {
    out.fill(0);
    out[..4].copy_from_slice(&LEDGER_MAGIC);
    out[HEADER_VERSION] = LEDGER_VERSION;
    out[HEADER_COUNT..HEADER_GENERATION].copy_from_slice(&header.count.to_le_bytes());
    out[HEADER_GENERATION..HEADER_CHECKSUM].copy_from_slice(&header.generation.to_le_bytes());
    let sum = fnv1a(&out[..HEADER_CHECKSUM]);
    out[HEADER_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
}

/// The header a writer puts down first and replaces last: all zero, so that a
/// generation whose write was interrupted reads as one that was never
/// committed rather than as a shorter library.
///
/// It is the only header that means that. A header block is written whole,
/// so a torn commit leaves this placeholder and a landed commit leaves a
/// header that decodes; bytes that are neither are a header that landed and
/// was damaged since.
pub fn encode_ledger_placeholder_header(out: &mut [u8; LEDGER_HEADER_BYTES]) {
    out.fill(0);
}

/// What the sixteen bytes at the start of a ledger file say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerHeaderReading {
    /// The all-zero placeholder: a generation that was never committed.
    Placeholder,
    /// A committed generation this build reads.
    Committed(LedgerHeader),
    /// The ledger's magic under a version this build does not read: another
    /// build's generation, whose ids cannot be rebuilt from the card.
    UnknownVersion(u8),
    /// None of the above: a committed header damaged after it landed, or a
    /// file that was never a ledger. Either way not a reason to trust the
    /// other side instead.
    Damaged,
}

/// Read a header without guessing. A version this build does not read is
/// reported before the checksum is consulted, since another version may
/// frame its header differently.
pub fn classify_ledger_header(bytes: &[u8; LEDGER_HEADER_BYTES]) -> LedgerHeaderReading {
    if bytes.iter().all(|byte| *byte == 0) {
        return LedgerHeaderReading::Placeholder;
    }
    if bytes[..4] != LEDGER_MAGIC {
        return LedgerHeaderReading::Damaged;
    }
    // No build writes version zero. A write torn between the magic and the
    // version byte, over the zeros of a placeholder, leaves exactly that,
    // and it is damage rather than another build's ledger.
    if bytes[HEADER_VERSION] == 0 {
        return LedgerHeaderReading::Damaged;
    }
    if bytes[HEADER_VERSION] != LEDGER_VERSION {
        return LedgerHeaderReading::UnknownVersion(bytes[HEADER_VERSION]);
    }
    let stored = u32::from_le_bytes([
        bytes[HEADER_CHECKSUM],
        bytes[HEADER_CHECKSUM + 1],
        bytes[HEADER_CHECKSUM + 2],
        bytes[HEADER_CHECKSUM + 3],
    ]);
    if bytes[HEADER_RESERVED] != 0 || fnv1a(&bytes[..HEADER_CHECKSUM]) != stored {
        return LedgerHeaderReading::Damaged;
    }
    LedgerHeaderReading::Committed(LedgerHeader {
        generation: u32::from_le_bytes([
            bytes[HEADER_GENERATION],
            bytes[HEADER_GENERATION + 1],
            bytes[HEADER_GENERATION + 2],
            bytes[HEADER_GENERATION + 3],
        ]),
        count: u16::from_le_bytes([bytes[HEADER_COUNT], bytes[HEADER_COUNT + 1]]),
    })
}

/// The exact length of a committed generation holding `count` records.
pub fn ledger_file_len(count: u16) -> usize {
    LEDGER_HEADER_BYTES + count as usize * LEDGER_RECORD_BYTES
}

// ---------------------------------------------------------------------------
// Ledger journal: which side is live, or which side is being written
// ---------------------------------------------------------------------------

pub const LEDGER_JOURNAL_MAGIC: [u8; 4] = *b"X4LJ";
pub const LEDGER_JOURNAL_VERSION: u8 = 1;
/// One entry, at the start of its slot.
pub const LEDGER_JOURNAL_BYTES: usize = 24;
/// A slot is one sector, so a write to it lands whole or is torn inside it
/// and nowhere else. Two of them alternate, so a torn write damages the
/// slot being written and the entry before it still reads.
pub const LEDGER_JOURNAL_SLOT_BYTES: usize = 512;
pub const LEDGER_JOURNAL_SLOTS: usize = 2;
pub const LEDGER_JOURNAL_FILE_BYTES: usize = LEDGER_JOURNAL_SLOT_BYTES * LEDGER_JOURNAL_SLOTS;

// Journal entry: magic | version | state | side | has standing |
// generation u32 | count u16 | reserved (zero) u16 | sequence u32 |
// checksum u32 over everything before it.
const JOURNAL_VERSION: usize = 4;
const JOURNAL_STATE: usize = 5;
const JOURNAL_SIDE: usize = 6;
const JOURNAL_HAS_STANDING: usize = 7;
const JOURNAL_GENERATION: usize = 8;
const JOURNAL_COUNT: usize = 12;
const JOURNAL_RESERVED: usize = 14;
const JOURNAL_SEQUENCE: usize = 16;
const JOURNAL_CHECKSUM: usize = 20;
const _: () = assert!(JOURNAL_CHECKSUM + 4 == LEDGER_JOURNAL_BYTES);
const _: () = assert!(LEDGER_JOURNAL_BYTES <= LEDGER_JOURNAL_SLOT_BYTES);
const JOURNAL_COMMITTED: u8 = 1;
const JOURNAL_REWRITING: u8 = 2;

/// What the ledger journal says about the two sides.
///
/// Two ledger files alone cannot say which of them is live. A side that
/// holds the placeholder, or nothing, looks the same whether a rewrite of
/// it was interrupted, which is harmless, or it was the live generation and
/// lost its header, which is the loss of every id it added. The journal is
/// the durable fact that tells them apart: after every commit it names the
/// live side and its header, and for the length of a rewrite it names the
/// side being written and what stood on the other side when the rewrite
/// began. A side is then believed only when the journal accounts for it.
///
/// The journal has to survive its own interrupted write, or it would be the
/// one durable write in the protocol that does not. So an entry lives in
/// one of two sector-sized slots, written alternately, each carrying a
/// sequence number: a torn write damages the slot being written, and the
/// entry before it, in the other slot, still reads. Falling back one entry
/// is safe by construction, because a generation's ids reach a committed
/// catalog only after the journal has named that generation live, so the
/// entry before names either the same live side or a rewrite whose outcome
/// the target's own header decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerJournal {
    /// `side` holds the live generation, and this is its header.
    Committed { side: u8, header: LedgerHeader },
    /// `target` is being rewritten. `standing` is the committed generation
    /// on the other side when the rewrite began, or `None` when nothing had
    /// been committed yet.
    Rewriting {
        target: u8,
        standing: Option<LedgerHeader>,
    },
}

/// Encode one entry with its sequence number, which the writer takes as one
/// past the entry it is superseding.
pub fn encode_ledger_journal(
    entry: LedgerJournal,
    sequence: u32,
    out: &mut [u8; LEDGER_JOURNAL_BYTES],
) {
    out.fill(0);
    out[..4].copy_from_slice(&LEDGER_JOURNAL_MAGIC);
    out[JOURNAL_VERSION] = LEDGER_JOURNAL_VERSION;
    let header = match entry {
        LedgerJournal::Committed { side, header } => {
            out[JOURNAL_STATE] = JOURNAL_COMMITTED;
            out[JOURNAL_SIDE] = side;
            Some(header)
        }
        LedgerJournal::Rewriting { target, standing } => {
            out[JOURNAL_STATE] = JOURNAL_REWRITING;
            out[JOURNAL_SIDE] = target;
            out[JOURNAL_HAS_STANDING] = u8::from(standing.is_some());
            standing
        }
    };
    if let Some(header) = header {
        out[JOURNAL_GENERATION..JOURNAL_COUNT].copy_from_slice(&header.generation.to_le_bytes());
        out[JOURNAL_COUNT..JOURNAL_RESERVED].copy_from_slice(&header.count.to_le_bytes());
    }
    out[JOURNAL_SEQUENCE..JOURNAL_CHECKSUM].copy_from_slice(&sequence.to_le_bytes());
    let sum = fnv1a(&out[..JOURNAL_CHECKSUM]);
    out[JOURNAL_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
}

/// What the bytes at the start of one journal slot say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerJournalReading {
    /// All zero. Nothing was ever written here.
    Blank,
    /// An entry, with the sequence number that orders it against the other
    /// slot's.
    Entry { entry: LedgerJournal, sequence: u32 },
    /// The journal's magic under a version this build does not read.
    UnknownVersion(u8),
    /// None of the above: a write torn by a power cut, an entry damaged
    /// after it landed, or not a journal. With the other slot intact the
    /// first of those is the common case, and the reader falls back to it.
    Damaged,
}

/// Read one slot without guessing, the way [`classify_ledger_header`] reads
/// a header.
pub fn classify_ledger_journal(bytes: &[u8; LEDGER_JOURNAL_BYTES]) -> LedgerJournalReading {
    if bytes.iter().all(|byte| *byte == 0) {
        return LedgerJournalReading::Blank;
    }
    if bytes[..4] != LEDGER_JOURNAL_MAGIC {
        return LedgerJournalReading::Damaged;
    }
    // As for a header: a write torn just past the magic, over a blank slot,
    // leaves version zero, which no build writes.
    if bytes[JOURNAL_VERSION] == 0 {
        return LedgerJournalReading::Damaged;
    }
    if bytes[JOURNAL_VERSION] != LEDGER_JOURNAL_VERSION {
        return LedgerJournalReading::UnknownVersion(bytes[JOURNAL_VERSION]);
    }
    let stored = u32::from_le_bytes([
        bytes[JOURNAL_CHECKSUM],
        bytes[JOURNAL_CHECKSUM + 1],
        bytes[JOURNAL_CHECKSUM + 2],
        bytes[JOURNAL_CHECKSUM + 3],
    ]);
    if fnv1a(&bytes[..JOURNAL_CHECKSUM]) != stored
        || bytes[JOURNAL_RESERVED] != 0
        || bytes[JOURNAL_RESERVED + 1] != 0
        || bytes[JOURNAL_SIDE] > 1
    {
        return LedgerJournalReading::Damaged;
    }
    let header = LedgerHeader {
        generation: u32::from_le_bytes([
            bytes[JOURNAL_GENERATION],
            bytes[JOURNAL_GENERATION + 1],
            bytes[JOURNAL_GENERATION + 2],
            bytes[JOURNAL_GENERATION + 3],
        ]),
        count: u16::from_le_bytes([bytes[JOURNAL_COUNT], bytes[JOURNAL_COUNT + 1]]),
    };
    let sequence = u32::from_le_bytes([
        bytes[JOURNAL_SEQUENCE],
        bytes[JOURNAL_SEQUENCE + 1],
        bytes[JOURNAL_SEQUENCE + 2],
        bytes[JOURNAL_SEQUENCE + 3],
    ]);
    let entry = match (bytes[JOURNAL_STATE], bytes[JOURNAL_HAS_STANDING]) {
        (JOURNAL_COMMITTED, 0) => LedgerJournal::Committed {
            side: bytes[JOURNAL_SIDE],
            header,
        },
        (JOURNAL_REWRITING, has_standing @ (0 | 1)) => LedgerJournal::Rewriting {
            target: bytes[JOURNAL_SIDE],
            standing: (has_standing == 1).then_some(header),
        },
        _ => return LedgerJournalReading::Damaged,
    };
    LedgerJournalReading::Entry { entry, sequence }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

// ---------------------------------------------------------------------------
// Managed replacement: the library intent that spans an install
// ---------------------------------------------------------------------------

pub const REPLACE_JOURNAL_MAGIC: [u8; 4] = *b"X4RI";
pub const REPLACE_JOURNAL_VERSION: u8 = 1;
/// One entry, at the start of its slot.
pub const REPLACE_JOURNAL_BYTES: usize = 645;
/// Kept the way the ledger journal is: two slots written alternately with a
/// sequence number, so a torn write leaves the entry before it. Two sectors
/// each, since an entry carries two locators; the checksum covers the whole
/// entry, so a write torn in either sector reads as damage and the other
/// slot answers.
pub const REPLACE_JOURNAL_SLOT_BYTES: usize = 1024;
pub const REPLACE_JOURNAL_SLOTS: usize = 2;
pub const REPLACE_JOURNAL_FILE_BYTES: usize = REPLACE_JOURNAL_SLOT_BYTES * REPLACE_JOURNAL_SLOTS;

// Entry: magic | version | state | root | predecessor | id | has evict |
// evict id | locator length u16 | locator, zero padded | predecessor locator
// length u16 | predecessor locator, zero padded | old byte length u64 | old
// sha256 | new byte length u64 | new sha256 | sequence u32 | checksum u32
// over everything before it. A cleared entry carries only its sequence.
const REPLACE_VERSION: usize = 4;
const REPLACE_STATE: usize = 5;
const REPLACE_ROOT: usize = 6;
const REPLACE_PREDECESSOR: usize = 7;
const REPLACE_ID: usize = 8;
const REPLACE_HAS_EVICT: usize = REPLACE_ID + BOOK_ID_BYTES;
const REPLACE_EVICT: usize = REPLACE_HAS_EVICT + 1;
const REPLACE_LOCATOR_LEN: usize = REPLACE_EVICT + BOOK_ID_BYTES;
const REPLACE_LOCATOR: usize = REPLACE_LOCATOR_LEN + 2;
const REPLACE_PRED_LOCATOR_LEN: usize = REPLACE_LOCATOR + MAX_PATH_BYTES;
const REPLACE_PRED_LOCATOR: usize = REPLACE_PRED_LOCATOR_LEN + 2;
const REPLACE_OLD_LEN: usize = REPLACE_PRED_LOCATOR + MAX_PATH_BYTES;
const REPLACE_OLD_SHA: usize = REPLACE_OLD_LEN + 8;
const REPLACE_NEW_LEN: usize = REPLACE_OLD_SHA + SHA256_BYTES;
const REPLACE_NEW_SHA: usize = REPLACE_NEW_LEN + 8;
const REPLACE_SEQUENCE: usize = REPLACE_NEW_SHA + SHA256_BYTES;
const REPLACE_CHECKSUM: usize = REPLACE_SEQUENCE + 4;
const _: () = assert!(REPLACE_CHECKSUM + 4 == REPLACE_JOURNAL_BYTES);
const _: () = assert!(REPLACE_JOURNAL_BYTES <= REPLACE_JOURNAL_SLOT_BYTES);
const REPLACE_CLEARED: u8 = 1;
const REPLACE_STANDING: u8 = 2;
const PREDECESSOR_NONE: u8 = 0;
const PREDECESSOR_UNKNOWN: u8 = 1;
const PREDECESSOR_KNOWN: u8 = 2;

/// What held the destination when a managed replacement began.
///
/// The three cases resolve a landing differently, so the intent has to
/// record which it was. A predecessor whose digest was read in the same
/// session can be recognised by it. One whose bytes were not read can only
/// be recognised as "not the new bytes", which the sole-writer contract
/// makes sufficient: nothing else was permitted to write while the intent
/// stood. No predecessor at all means an install that rolled back leaves
/// nothing at the destination.
///
/// `Known` is for a digest read in the session that publishes the intent,
/// and not for one the ledger recorded earlier. Between transactions a
/// computer may replace a file with another of the same size, which the
/// ledger cannot see, and an intent that carried the recorded digest as the
/// predecessor's would then recognise neither side of a rollback that
/// recovered perfectly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Predecessor {
    None,
    Unknown,
    Known(CachedSourceDigest),
}

/// A managed replacement in flight: which copy is being replaced, where, by
/// what bytes, and what was there before.
///
/// Written before the install begins, standing after `INSTALL.JNL` clears,
/// and cleared once the ledger record says what landed. It is what lets a
/// power cut anywhere in between keep the copy's id: the filesystem
/// transaction decides which bytes the destination holds, and this decides
/// what that means for identity.
///
/// Two locators, because the card matches names by FAT's rules and the
/// ledger exactly: an upload spelled `dune.epub` replaces `Dune.epub`, and
/// which spelling the file ends up under depends on which side landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplaceIntent<'a> {
    pub id: BookId,
    pub root: BookRoot,
    /// Where the install lands, spelled as typed. Root-relative, exactly as
    /// the catalog stores it.
    pub locator: &'a str,
    pub predecessor: Predecessor,
    /// The exact spelling the predecessor held the place under. `Some` if
    /// and only if there was a predecessor.
    pub predecessor_locator: Option<&'a str>,
    /// The missing copy whose record makes room for this one in a ledger
    /// that has none: verified absent from the card when the intent was
    /// published, which the sole-writer contract keeps true while it stands.
    /// Only for a copy the ledger has not adopted; a replacement of an
    /// adopted copy needs no room.
    pub evict: Option<BookId>,
    /// The bytes that are meant to land, hashed as they were streamed.
    pub new: CachedSourceDigest,
}

/// One entry of the replacement journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceJournal<'a> {
    /// No replacement in flight.
    Cleared,
    Standing(ReplaceIntent<'a>),
}

/// Encode one entry with its sequence number. `None` when a locator does not
/// fit, which a legal [`crate::library_path::LibraryPath`] cannot reach, or
/// when the predecessor's spelling is given for no predecessor or withheld
/// for one.
pub fn encode_replace_journal(
    entry: &ReplaceJournal<'_>,
    sequence: u32,
    out: &mut [u8; REPLACE_JOURNAL_BYTES],
) -> Option<()> {
    out.fill(0);
    out[..4].copy_from_slice(&REPLACE_JOURNAL_MAGIC);
    out[REPLACE_VERSION] = REPLACE_JOURNAL_VERSION;
    match entry {
        ReplaceJournal::Cleared => out[REPLACE_STATE] = REPLACE_CLEARED,
        ReplaceJournal::Standing(intent) => {
            let locator = intent.locator.as_bytes();
            if locator.len() > MAX_PATH_BYTES {
                return None;
            }
            if intent.predecessor_locator.is_some()
                == matches!(intent.predecessor, Predecessor::None)
            {
                return None;
            }
            out[REPLACE_STATE] = REPLACE_STANDING;
            out[REPLACE_ROOT] = root_byte(intent.root);
            out[REPLACE_ID..REPLACE_HAS_EVICT].copy_from_slice(&intent.id.to_bytes());
            if let Some(evict) = intent.evict {
                out[REPLACE_HAS_EVICT] = 1;
                out[REPLACE_EVICT..REPLACE_LOCATOR_LEN].copy_from_slice(&evict.to_bytes());
            }
            out[REPLACE_LOCATOR_LEN..REPLACE_LOCATOR]
                .copy_from_slice(&(locator.len() as u16).to_le_bytes());
            out[REPLACE_LOCATOR..REPLACE_LOCATOR + locator.len()].copy_from_slice(locator);
            if let Some(spelled) = intent.predecessor_locator {
                let spelled = spelled.as_bytes();
                if spelled.len() > MAX_PATH_BYTES {
                    return None;
                }
                out[REPLACE_PRED_LOCATOR_LEN..REPLACE_PRED_LOCATOR]
                    .copy_from_slice(&(spelled.len() as u16).to_le_bytes());
                out[REPLACE_PRED_LOCATOR..REPLACE_PRED_LOCATOR + spelled.len()]
                    .copy_from_slice(spelled);
            }
            out[REPLACE_PREDECESSOR] = match intent.predecessor {
                Predecessor::None => PREDECESSOR_NONE,
                Predecessor::Unknown => PREDECESSOR_UNKNOWN,
                Predecessor::Known(old) => {
                    out[REPLACE_OLD_LEN..REPLACE_OLD_SHA]
                        .copy_from_slice(&old.byte_len().to_le_bytes());
                    out[REPLACE_OLD_SHA..REPLACE_NEW_LEN].copy_from_slice(old.sha256());
                    PREDECESSOR_KNOWN
                }
            };
            out[REPLACE_NEW_LEN..REPLACE_NEW_SHA]
                .copy_from_slice(&intent.new.byte_len().to_le_bytes());
            out[REPLACE_NEW_SHA..REPLACE_SEQUENCE].copy_from_slice(intent.new.sha256());
        }
    }
    out[REPLACE_SEQUENCE..REPLACE_CHECKSUM].copy_from_slice(&sequence.to_le_bytes());
    let sum = fnv1a(&out[..REPLACE_CHECKSUM]);
    out[REPLACE_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
    Some(())
}

/// What the bytes at the start of one replacement-journal slot say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceJournalReading<'a> {
    /// All zero. Nothing was ever written here.
    Blank,
    Entry {
        entry: ReplaceJournal<'a>,
        sequence: u32,
    },
    /// The journal's magic under a version this build does not read.
    UnknownVersion(u8),
    /// None of the above: a write torn by a power cut, an entry damaged
    /// after it landed, or not a journal.
    Damaged,
}

/// Read one slot without guessing, the way [`classify_ledger_journal`] does.
pub fn classify_replace_journal(bytes: &[u8; REPLACE_JOURNAL_BYTES]) -> ReplaceJournalReading<'_> {
    if bytes.iter().all(|byte| *byte == 0) {
        return ReplaceJournalReading::Blank;
    }
    if bytes[..4] != REPLACE_JOURNAL_MAGIC {
        return ReplaceJournalReading::Damaged;
    }
    // Version zero is a write torn just past the magic, over zeros.
    if bytes[REPLACE_VERSION] == 0 {
        return ReplaceJournalReading::Damaged;
    }
    if bytes[REPLACE_VERSION] != REPLACE_JOURNAL_VERSION {
        return ReplaceJournalReading::UnknownVersion(bytes[REPLACE_VERSION]);
    }
    let stored = u32::from_le_bytes([
        bytes[REPLACE_CHECKSUM],
        bytes[REPLACE_CHECKSUM + 1],
        bytes[REPLACE_CHECKSUM + 2],
        bytes[REPLACE_CHECKSUM + 3],
    ]);
    if fnv1a(&bytes[..REPLACE_CHECKSUM]) != stored {
        return ReplaceJournalReading::Damaged;
    }
    let sequence = u32::from_le_bytes([
        bytes[REPLACE_SEQUENCE],
        bytes[REPLACE_SEQUENCE + 1],
        bytes[REPLACE_SEQUENCE + 2],
        bytes[REPLACE_SEQUENCE + 3],
    ]);
    let entry = match bytes[REPLACE_STATE] {
        REPLACE_CLEARED => ReplaceJournal::Cleared,
        REPLACE_STANDING => {
            let id_at = |at: usize| {
                let mut id = [0u8; BOOK_ID_BYTES];
                id.copy_from_slice(&bytes[at..at + BOOK_ID_BYTES]);
                BookId::from_bytes(id)
            };
            let Some(id) = id_at(REPLACE_ID) else {
                return ReplaceJournalReading::Damaged;
            };
            let evict = match bytes[REPLACE_HAS_EVICT] {
                0 => None,
                1 => match id_at(REPLACE_EVICT) {
                    Some(evict) => Some(evict),
                    None => return ReplaceJournalReading::Damaged,
                },
                _ => return ReplaceJournalReading::Damaged,
            };
            let Some(root) = book_root(bytes[REPLACE_ROOT]) else {
                return ReplaceJournalReading::Damaged;
            };
            let len =
                u16::from_le_bytes([bytes[REPLACE_LOCATOR_LEN], bytes[REPLACE_LOCATOR_LEN + 1]])
                    as usize;
            if len > MAX_PATH_BYTES {
                return ReplaceJournalReading::Damaged;
            }
            let Ok(locator) = core::str::from_utf8(&bytes[REPLACE_LOCATOR..REPLACE_LOCATOR + len])
            else {
                return ReplaceJournalReading::Damaged;
            };
            let spelled_len = u16::from_le_bytes([
                bytes[REPLACE_PRED_LOCATOR_LEN],
                bytes[REPLACE_PRED_LOCATOR_LEN + 1],
            ]) as usize;
            if spelled_len > MAX_PATH_BYTES {
                return ReplaceJournalReading::Damaged;
            }
            let predecessor_locator = if bytes[REPLACE_PREDECESSOR] == PREDECESSOR_NONE {
                if spelled_len != 0 {
                    return ReplaceJournalReading::Damaged;
                }
                None
            } else {
                match core::str::from_utf8(
                    &bytes[REPLACE_PRED_LOCATOR..REPLACE_PRED_LOCATOR + spelled_len],
                ) {
                    Ok(spelled) => Some(spelled),
                    Err(_) => return ReplaceJournalReading::Damaged,
                }
            };
            let digest_at = |len_at: usize, sha_at: usize| {
                let mut len = [0u8; 8];
                len.copy_from_slice(&bytes[len_at..len_at + 8]);
                let mut sha = [0u8; SHA256_BYTES];
                sha.copy_from_slice(&bytes[sha_at..sha_at + SHA256_BYTES]);
                CachedSourceDigest::new(SourceDigest::from_parts(u64::from_le_bytes(len), sha))
            };
            let predecessor = match bytes[REPLACE_PREDECESSOR] {
                PREDECESSOR_NONE => Predecessor::None,
                PREDECESSOR_UNKNOWN => Predecessor::Unknown,
                PREDECESSOR_KNOWN => {
                    Predecessor::Known(digest_at(REPLACE_OLD_LEN, REPLACE_OLD_SHA))
                }
                _ => return ReplaceJournalReading::Damaged,
            };
            ReplaceJournal::Standing(ReplaceIntent {
                id,
                root,
                locator,
                predecessor,
                predecessor_locator,
                evict,
                new: digest_at(REPLACE_NEW_LEN, REPLACE_NEW_SHA),
            })
        }
        _ => return ReplaceJournalReading::Damaged,
    };
    ReplaceJournalReading::Entry { entry, sequence }
}

/// Which side of a replacement the destination holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    /// The new bytes: the copy keeps its id and takes the new digest.
    New,
    /// What stood before, or nothing where nothing stood: the record is left
    /// as it was.
    Old,
}

/// Decide a landing from what the destination holds now, or `None` when
/// neither side can be established and the intent has to stand.
///
/// `destination` is the digest of the file at the locator, freshly computed,
/// or `None` for no file. The rules are the identity design's: the new
/// digest is decisive; a known predecessor is recognised by its digest; an
/// unknown one is recognised as any file that is not the new bytes, which
/// holds only because nothing else may write while the intent stands; and
/// where nothing stood, nothing standing is the old landing.
pub fn landing(
    predecessor: &Predecessor,
    new: &CachedSourceDigest,
    destination: Option<&SourceDigest>,
) -> Option<Landing> {
    match (destination, predecessor) {
        (Some(found), _) if new.agrees_with(found) => Some(Landing::New),
        (Some(found), Predecessor::Known(old)) if old.agrees_with(found) => Some(Landing::Old),
        (Some(_), Predecessor::Unknown) => Some(Landing::Old),
        (None, Predecessor::None) => Some(Landing::Old),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Row keys: the in-RAM side of the join between a fresh catalog and the ledger
// ---------------------------------------------------------------------------

/// Bytes one staged `(source_hash, row)` key occupies in the caller's
/// scratch: 2,730 rows in the 16 KB arena.
pub const ROW_KEY_BYTES: usize = 6;

/// Stage the key of catalog row `row`, whose place hashes to `hash`, at
/// `index` in `scratch`. Returns false (staging nothing) past capacity.
///
/// The join reads the ledger once per slice of rows and needs, per record,
/// the rows whose place could be the record's. The 32-bit place hash is a
/// lossy projection and two locators can share one, so what comes back
/// from [`rows_with_hash`] is candidates to compare in full, not answers.
pub fn stage_row_key(scratch: &mut [u8], index: usize, hash: u32, row: u16) -> bool {
    let at = index * ROW_KEY_BYTES;
    let Some(slot) = scratch.get_mut(at..at + ROW_KEY_BYTES) else {
        return false;
    };
    slot[..4].copy_from_slice(&hash.to_le_bytes());
    slot[4..].copy_from_slice(&row.to_le_bytes());
    true
}

/// Order the first `count` staged keys by hash, then row, so
/// [`rows_with_hash`] can binary-search them. Heapsort: in place, no
/// allocation, no recursion.
pub fn sort_row_keys(scratch: &mut [u8], count: usize) {
    let count = count.min(scratch.len() / ROW_KEY_BYTES);
    for parent in (0..count / 2).rev() {
        sift_down_row_key(scratch, parent, count);
    }
    for end in (1..count).rev() {
        swap_row_keys(scratch, 0, end);
        sift_down_row_key(scratch, 0, end);
    }
}

/// Every staged row whose place hashes to `hash`, in row order. The keys
/// must have been through [`sort_row_keys`].
pub fn rows_with_hash(scratch: &[u8], count: usize, hash: u32) -> RowsWithHash<'_> {
    let count = count.min(scratch.len() / ROW_KEY_BYTES);
    // Lower bound of the hash: the first key at or above `(hash, row 0)`.
    let wanted = row_key(hash, 0);
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if staged_row_key(scratch, mid) < wanted {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    RowsWithHash {
        scratch,
        count,
        hash,
        next: lo,
    }
}

/// See [`rows_with_hash`].
pub struct RowsWithHash<'a> {
    scratch: &'a [u8],
    count: usize,
    hash: u32,
    next: usize,
}

impl Iterator for RowsWithHash<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.next >= self.count {
            return None;
        }
        let key = staged_row_key(self.scratch, self.next);
        if (key >> 16) as u32 != self.hash {
            return None;
        }
        self.next += 1;
        Some(key as u16)
    }
}

/// The sort key: hash in the high bits so equal hashes are adjacent, row
/// below so the walk out of [`rows_with_hash`] comes back in row order.
fn row_key(hash: u32, row: u16) -> u64 {
    (u64::from(hash) << 16) | u64::from(row)
}

fn staged_row_key(scratch: &[u8], index: usize) -> u64 {
    let at = index * ROW_KEY_BYTES;
    let hash = u32::from_le_bytes([
        scratch[at],
        scratch[at + 1],
        scratch[at + 2],
        scratch[at + 3],
    ]);
    let row = u16::from_le_bytes([scratch[at + 4], scratch[at + 5]]);
    row_key(hash, row)
}

fn swap_row_keys(scratch: &mut [u8], a: usize, b: usize) {
    for offset in 0..ROW_KEY_BYTES {
        scratch.swap(a * ROW_KEY_BYTES + offset, b * ROW_KEY_BYTES + offset);
    }
}

fn sift_down_row_key(scratch: &mut [u8], mut parent: usize, end: usize) {
    loop {
        let mut child = parent * 2 + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && staged_row_key(scratch, child) < staged_row_key(scratch, child + 1) {
            child += 1;
        }
        if staged_row_key(scratch, parent) >= staged_row_key(scratch, child) {
            return;
        }
        swap_row_keys(scratch, parent, child);
        parent = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic "random" source for tests: hands out the words it
    /// was given, in order.
    fn words(seq: &'static [u32]) -> impl FnMut() -> u32 {
        let mut at = 0;
        move || {
            let word = seq[at];
            at += 1;
            word
        }
    }

    #[test]
    fn minting_redraws_the_zero_id_rather_than_handing_it_out() {
        let mut random = words(&[0, 0, 0, 0, 1, 2, 3, 4]);
        let id = BookId::mint(&mut random);
        let mut expected = [0u8; BOOK_ID_BYTES];
        expected[0] = 1;
        expected[4] = 2;
        expected[8] = 3;
        expected[12] = 4;
        assert_eq!(id.to_bytes(), expected);
        assert_eq!(BookId::from_bytes([0u8; BOOK_ID_BYTES]), None);
        assert_eq!(BookId::from_bytes(expected), Some(id));
    }

    #[test]
    fn two_mints_are_two_ids() {
        let mut random = words(&[9, 9, 9, 9, 9, 9, 9, 8]);
        let a = BookId::mint(&mut random);
        let b = BookId::mint(&mut random);
        assert_ne!(a, b);
    }

    fn sample() -> LedgerRecord<'static> {
        LedgerRecord {
            id: BookId::from_bytes([7u8; BOOK_ID_BYTES]).unwrap(),
            root: BookRoot::Library,
            locator: "History/Rome/SPQR.epub",
            byte_size: 1_234_567,
            misses: 0,
            source: None,
        }
    }

    fn digest(seed: u8) -> CachedSourceDigest {
        CachedSourceDigest::new(SourceDigest::from_parts(1_234_567, [seed; SHA256_BYTES]))
    }

    #[test]
    fn a_record_round_trips() {
        let record = sample();
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        encode_ledger_record(&record, &mut bytes).unwrap();
        assert_eq!(decode_ledger_record(&bytes), Some(record));

        let loose = LedgerRecord {
            root: BookRoot::CardRoot,
            locator: "Dune.epub",
            misses: 3,
            source: Some(digest(0xA5)),
            ..record
        };
        encode_ledger_record(&loose, &mut bytes).unwrap();
        assert_eq!(decode_ledger_record(&bytes), Some(loose));
        // A present flag past the two values it can take is not a record.
        bytes[RECORD_HAS_SOURCE] = 2;
        let sum = fnv1a(&bytes[..RECORD_CHECKSUM]);
        bytes[RECORD_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_ledger_record(&bytes), None);
    }

    #[test]
    fn a_replacement_intent_round_trips_and_reads_without_guessing() {
        let id = BookId::from_bytes([3u8; BOOK_ID_BYTES]).unwrap();
        let entries = [
            ReplaceJournal::Cleared,
            ReplaceJournal::Standing(ReplaceIntent {
                id,
                root: BookRoot::Library,
                locator: "Fiction/Dune.epub",
                predecessor: Predecessor::None,
                predecessor_locator: None,
                evict: BookId::from_bytes([9u8; BOOK_ID_BYTES]),
                new: digest(1),
            }),
            ReplaceJournal::Standing(ReplaceIntent {
                id,
                root: BookRoot::CardRoot,
                locator: "dune.epub",
                predecessor: Predecessor::Unknown,
                predecessor_locator: Some("Dune.epub"),
                evict: None,
                new: digest(2),
            }),
            ReplaceJournal::Standing(ReplaceIntent {
                id,
                root: BookRoot::Library,
                locator: "Dune.epub",
                predecessor: Predecessor::Known(digest(3)),
                predecessor_locator: Some("Dune.epub"),
                evict: None,
                new: digest(4),
            }),
        ];
        for entry in entries {
            let mut bytes = [0u8; REPLACE_JOURNAL_BYTES];
            encode_replace_journal(&entry, 77, &mut bytes).unwrap();
            assert_eq!(
                classify_replace_journal(&bytes),
                ReplaceJournalReading::Entry {
                    entry,
                    sequence: 77
                },
                "{entry:?}"
            );
            for at in [
                0,
                REPLACE_STATE,
                REPLACE_ROOT,
                REPLACE_PREDECESSOR,
                REPLACE_ID,
                REPLACE_HAS_EVICT,
                REPLACE_EVICT + 3,
                REPLACE_LOCATOR_LEN,
                REPLACE_LOCATOR + 2,
                REPLACE_PRED_LOCATOR_LEN,
                REPLACE_PRED_LOCATOR + 1,
                REPLACE_OLD_SHA,
                REPLACE_NEW_LEN,
                REPLACE_SEQUENCE,
                REPLACE_CHECKSUM + 3,
            ] {
                let mut torn = bytes;
                torn[at] ^= 0x04;
                assert_eq!(
                    classify_replace_journal(&torn),
                    ReplaceJournalReading::Damaged,
                    "{entry:?}, flipped byte {at}"
                );
            }
            let mut other = bytes;
            other[REPLACE_VERSION] = REPLACE_JOURNAL_VERSION + 1;
            assert_eq!(
                classify_replace_journal(&other),
                ReplaceJournalReading::UnknownVersion(REPLACE_JOURNAL_VERSION + 1)
            );
            for landed in 1..REPLACE_JOURNAL_BYTES {
                let mut torn = [0u8; REPLACE_JOURNAL_BYTES];
                torn[..landed].copy_from_slice(&bytes[..landed]);
                assert_eq!(
                    classify_replace_journal(&torn),
                    ReplaceJournalReading::Damaged,
                    "{entry:?}, {landed} bytes landed"
                );
            }
        }
        assert_eq!(
            classify_replace_journal(&[0u8; REPLACE_JOURNAL_BYTES]),
            ReplaceJournalReading::Blank
        );
        let long = core::str::from_utf8(&[b'a'; MAX_PATH_BYTES + 1]).unwrap();
        let mut bytes = [0u8; REPLACE_JOURNAL_BYTES];
        assert_eq!(
            encode_replace_journal(
                &ReplaceJournal::Standing(ReplaceIntent {
                    id,
                    root: BookRoot::Library,
                    locator: long,
                    predecessor: Predecessor::None,
                    predecessor_locator: None,
                    evict: None,
                    new: digest(1),
                }),
                1,
                &mut bytes
            ),
            None
        );
        // A predecessor's spelling goes with a predecessor, and only with one.
        for (predecessor, predecessor_locator) in [
            (Predecessor::None, Some("Dune.epub")),
            (Predecessor::Unknown, None),
            (Predecessor::Known(digest(3)), None),
        ] {
            assert_eq!(
                encode_replace_journal(
                    &ReplaceJournal::Standing(ReplaceIntent {
                        id,
                        root: BookRoot::Library,
                        locator: "Dune.epub",
                        predecessor,
                        predecessor_locator,
                        evict: None,
                        new: digest(1),
                    }),
                    1,
                    &mut bytes
                ),
                None,
                "{predecessor:?} with {predecessor_locator:?}"
            );
        }
    }

    /// The identity design's resolution table, one row at a time.
    #[test]
    fn a_landing_is_decided_by_what_the_destination_holds() {
        let new = digest(1);
        let old = digest(2);
        let stranger = SourceDigest::from_parts(1_234_567, [9u8; SHA256_BYTES]);
        let new_bytes = SourceDigest::from_parts(1_234_567, [1u8; SHA256_BYTES]);
        let old_bytes = SourceDigest::from_parts(1_234_567, [2u8; SHA256_BYTES]);
        for predecessor in [
            Predecessor::None,
            Predecessor::Unknown,
            Predecessor::Known(old),
        ] {
            assert_eq!(
                landing(&predecessor, &new, Some(&new_bytes)),
                Some(Landing::New),
                "{predecessor:?}: the new bytes are decisive"
            );
        }
        assert_eq!(
            landing(&Predecessor::Known(old), &new, Some(&old_bytes)),
            Some(Landing::Old)
        );
        assert_eq!(
            landing(&Predecessor::Known(old), &new, Some(&stranger)),
            None,
            "a known predecessor is recognised by its digest and nothing else"
        );
        assert_eq!(
            landing(&Predecessor::Known(old), &new, None),
            None,
            "a predecessor that is gone is no landing"
        );
        assert_eq!(
            landing(&Predecessor::Unknown, &new, Some(&stranger)),
            Some(Landing::Old),
            "under the sole-writer contract, not the new bytes means the old"
        );
        assert_eq!(landing(&Predecessor::Unknown, &new, None), None);
        assert_eq!(landing(&Predecessor::None, &new, None), Some(Landing::Old));
        assert_eq!(
            landing(&Predecessor::None, &new, Some(&stranger)),
            None,
            "where nothing stood, a stranger is not a landing"
        );
    }

    #[test]
    fn a_missing_record_ages_until_the_bound_and_yields_to_a_full_generation() {
        assert_eq!(carry_missing(0, 1), Some(1));
        assert_eq!(
            carry_missing(MISSING_SCANS_RETAINED - 1, 1),
            Some(MISSING_SCANS_RETAINED)
        );
        assert_eq!(carry_missing(MISSING_SCANS_RETAINED, 1), None);
        assert_eq!(carry_missing(u8::MAX, usize::MAX), None);
        assert_eq!(carry_missing(0, 0), None, "live and new copies come first");
    }

    #[test]
    fn a_torn_or_foreign_record_decodes_as_nothing() {
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        encode_ledger_record(&sample(), &mut bytes).unwrap();
        for at in [
            0,
            RECORD_ROOT,
            RECORD_MISSES,
            RECORD_LOCATOR_LEN,
            RECORD_LOCATOR + 3,
            RECORD_SIZE,
            RECORD_HAS_SOURCE,
            RECORD_SOURCE_SHA + 5,
            RECORD_CHECKSUM,
        ] {
            let mut torn = bytes;
            torn[at] ^= 0x40;
            assert_eq!(decode_ledger_record(&torn), None, "flipped byte {at}");
        }
        assert_eq!(decode_ledger_record(&[0u8; LEDGER_RECORD_BYTES]), None);
        // A checksum that happens to agree does not make an unknown root or
        // a zero id readable.
        let mut alien = bytes;
        alien[RECORD_ROOT] = 9;
        let sum = fnv1a(&alien[..RECORD_CHECKSUM]);
        alien[RECORD_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_ledger_record(&alien), None);
        let mut anonymous = bytes;
        anonymous[RECORD_ID..RECORD_ROOT].fill(0);
        let sum = fnv1a(&anonymous[..RECORD_CHECKSUM]);
        anonymous[RECORD_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(decode_ledger_record(&anonymous), None);
    }

    #[test]
    fn a_locator_past_the_field_is_refused_rather_than_cut() {
        let long = core::str::from_utf8(&[b'a'; MAX_PATH_BYTES + 1]).unwrap();
        let record = LedgerRecord {
            locator: long,
            ..sample()
        };
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        assert_eq!(encode_ledger_record(&record, &mut bytes), None);
        let full = core::str::from_utf8(&[b'a'; MAX_PATH_BYTES]).unwrap();
        let record = LedgerRecord {
            locator: full,
            ..sample()
        };
        encode_ledger_record(&record, &mut bytes).unwrap();
        assert_eq!(decode_ledger_record(&bytes).unwrap().locator, full);
    }

    #[test]
    fn a_header_reads_as_committed_placeholder_unknown_or_damaged() {
        let header = LedgerHeader {
            generation: 0xFFFF_FFFE,
            count: 1129,
        };
        let mut bytes = [0u8; LEDGER_HEADER_BYTES];
        encode_ledger_header(header, &mut bytes);
        assert_eq!(
            classify_ledger_header(&bytes),
            LedgerHeaderReading::Committed(header)
        );
        let mut placeholder = [0xAAu8; LEDGER_HEADER_BYTES];
        encode_ledger_placeholder_header(&mut placeholder);
        assert_eq!(
            classify_ledger_header(&placeholder),
            LedgerHeaderReading::Placeholder
        );
        // Only the exact placeholder means "never committed". Every other
        // byte flip is damage to a header that had landed, except the
        // version, which is another build's ledger.
        for at in 0..LEDGER_HEADER_BYTES {
            let mut torn = bytes;
            torn[at] ^= 0x02;
            let expected = if at == HEADER_VERSION {
                LedgerHeaderReading::UnknownVersion(LEDGER_VERSION ^ 0x02)
            } else {
                LedgerHeaderReading::Damaged
            };
            assert_eq!(classify_ledger_header(&torn), expected, "flipped byte {at}");
        }
        let mut nearly_blank = placeholder;
        nearly_blank[LEDGER_HEADER_BYTES - 1] = 1;
        assert_eq!(
            classify_ledger_header(&nearly_blank),
            LedgerHeaderReading::Damaged
        );
        // A header write torn just past its magic, over the placeholder,
        // leaves version zero: damage, not another build.
        for landed in 1..LEDGER_HEADER_BYTES {
            let mut torn = [0u8; LEDGER_HEADER_BYTES];
            torn[..landed].copy_from_slice(&bytes[..landed]);
            assert_eq!(
                classify_ledger_header(&torn),
                LedgerHeaderReading::Damaged,
                "{landed} bytes landed"
            );
        }
        // Another build's version is reported whatever its checksum says,
        // since that build may frame its header differently.
        let mut other = bytes;
        other[HEADER_VERSION] = LEDGER_VERSION + 6;
        let sum = fnv1a(&other[..HEADER_CHECKSUM]);
        other[HEADER_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
        assert_eq!(
            classify_ledger_header(&other),
            LedgerHeaderReading::UnknownVersion(LEDGER_VERSION + 6)
        );
        assert_eq!(
            ledger_file_len(header.count),
            LEDGER_HEADER_BYTES + 1129 * LEDGER_RECORD_BYTES
        );
    }

    #[test]
    fn a_journal_entry_round_trips_and_reads_without_guessing() {
        let header = LedgerHeader {
            generation: 9,
            count: 1129,
        };
        let entries = [
            LedgerJournal::Committed { side: 1, header },
            LedgerJournal::Rewriting {
                target: 0,
                standing: Some(header),
            },
            LedgerJournal::Rewriting {
                target: 0,
                standing: None,
            },
        ];
        for entry in entries {
            let mut bytes = [0u8; LEDGER_JOURNAL_BYTES];
            encode_ledger_journal(entry, 0xFFFF_FFF0, &mut bytes);
            assert_eq!(
                classify_ledger_journal(&bytes),
                LedgerJournalReading::Entry {
                    entry,
                    sequence: 0xFFFF_FFF0
                },
                "{entry:?}"
            );
            for at in 0..LEDGER_JOURNAL_BYTES {
                let mut torn = bytes;
                torn[at] ^= 0x02;
                let expected = if at == JOURNAL_VERSION {
                    LedgerJournalReading::UnknownVersion(LEDGER_JOURNAL_VERSION ^ 0x02)
                } else {
                    LedgerJournalReading::Damaged
                };
                assert_eq!(
                    classify_ledger_journal(&torn),
                    expected,
                    "{entry:?}, flipped byte {at}"
                );
            }
            // Every prefix a torn write over a blank slot can leave is
            // damage, not another build, and not blank once one byte is in.
            for landed in 1..LEDGER_JOURNAL_BYTES {
                let mut torn = [0u8; LEDGER_JOURNAL_BYTES];
                torn[..landed].copy_from_slice(&bytes[..landed]);
                assert_eq!(
                    classify_ledger_journal(&torn),
                    LedgerJournalReading::Damaged,
                    "{entry:?}, {landed} bytes landed"
                );
            }
        }
        assert_eq!(
            classify_ledger_journal(&[0u8; LEDGER_JOURNAL_BYTES]),
            LedgerJournalReading::Blank
        );
        // A side past the two there are, or a state this build does not
        // write, is not an entry even under a checksum that agrees.
        let mut bytes = [0u8; LEDGER_JOURNAL_BYTES];
        encode_ledger_journal(entries[0], 1, &mut bytes);
        for (at, byte) in [
            (JOURNAL_SIDE, 2u8),
            (JOURNAL_STATE, 3),
            (JOURNAL_HAS_STANDING, 1),
        ] {
            let mut odd = bytes;
            odd[at] = byte;
            let sum = fnv1a(&odd[..JOURNAL_CHECKSUM]);
            odd[JOURNAL_CHECKSUM..].copy_from_slice(&sum.to_le_bytes());
            assert_eq!(
                classify_ledger_journal(&odd),
                LedgerJournalReading::Damaged,
                "byte {at} = {byte}"
            );
        }
    }

    #[test]
    fn row_keys_sort_and_answer_by_hash_with_duplicates_in_row_order() {
        let mut scratch = [0u8; 6 * ROW_KEY_BYTES];
        let staged = [(0x30u32, 5u16), (0x10, 9), (0x30, 2), (0x20, 0), (0x30, 7)];
        for (index, (hash, row)) in staged.iter().enumerate() {
            assert!(stage_row_key(&mut scratch, index, *hash, *row));
        }
        assert!(!stage_row_key(&mut scratch, 6, 1, 1), "past capacity");
        sort_row_keys(&mut scratch, staged.len());
        let rows: heapless::Vec<u16, 8> = rows_with_hash(&scratch, staged.len(), 0x30).collect();
        assert_eq!(rows.as_slice(), &[2, 5, 7]);
        let rows: heapless::Vec<u16, 8> = rows_with_hash(&scratch, staged.len(), 0x10).collect();
        assert_eq!(rows.as_slice(), &[9]);
        assert_eq!(rows_with_hash(&scratch, staged.len(), 0x25).count(), 0);
        assert_eq!(rows_with_hash(&scratch, staged.len(), 0x40).count(), 0);
        assert_eq!(rows_with_hash(&scratch, staged.len(), 0).count(), 0);
        // A count past what the scratch holds is clamped, not read past.
        assert_eq!(rows_with_hash(&scratch, 99, 0x20).count(), 1);
    }

    #[test]
    fn the_extreme_hash_values_sort_and_search() {
        let mut scratch = [0u8; 3 * ROW_KEY_BYTES];
        stage_row_key(&mut scratch, 0, u32::MAX, 1);
        stage_row_key(&mut scratch, 1, 0, 2);
        stage_row_key(&mut scratch, 2, u32::MAX, 0);
        sort_row_keys(&mut scratch, 3);
        let rows: heapless::Vec<u16, 4> = rows_with_hash(&scratch, 3, u32::MAX).collect();
        assert_eq!(rows.as_slice(), &[0, 1]);
        let rows: heapless::Vec<u16, 4> = rows_with_hash(&scratch, 3, 0).collect();
        assert_eq!(rows.as_slice(), &[2]);
    }
}
