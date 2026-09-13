//! Installing a finished file under its real name, crash-safely.
//!
//! Uploads stream into a scratch file under `/READER/UPLOAD`, opaque and with
//! no long name, so an interrupted one leaves a stray file and nothing a
//! catalog scan will trip over.
//!
//! What needs a protocol is what follows: the finished file has to become
//! `/BOOKS/Some Book.epub` and whatever held that name has to go. FAT cannot
//! do it in one write, so it is two moves and a delete under one durable
//! record:
//!
//! ```text
//! /BOOKS/<old>          --move-->  /READER/ROLLBACK/<txn>
//! /READER/UPLOAD/<txn>  --move-->  /BOOKS/, under the long name
//! /READER/ROLLBACK/<txn> --delete, reclaiming its clusters
//! ```
//!
//! Each move rewrites directory entries and leaves the cluster chain alone,
//! so it costs two directory writes rather than a copy of the book.
//!
//! # Intent, not progress
//!
//! The record says which scratch file, which long name, which predecessor. It
//! is written before anything is touched and cleared when everything is done,
//! never updated in between; [`IntentState`] covers what a record that cannot
//! be read means.
//!
//! Progress is not stored because the card holds it. The four places a file
//! can be at rest — old name, rollback, scratch, shelf — say how far the
//! sequence got, and [`plan`] maps every combination to one next action. No
//! phase field can disagree with the card.
//!
//! # The one rule that matters
//!
//! A move puts two names on one chain for a moment, and both work. Cleanup
//! there must **unlink** the name it does not want, never reclaim it:
//! reclaiming frees clusters the survivor is still reading from. [`Step`]
//! keeps the two apart so a caller cannot pick the wrong one.
//!
//! # What this is safe against
//!
//! Power loss, at any point, while this device is the only thing writing to
//! the card. That is the whole promise.
//!
//! Not a card edited elsewhere while a record stands. FAT counts no references
//! and identifies a file by nothing but its directory entry, so a chain freed
//! by another writer looks exactly like one still held, and its number goes to
//! whatever is written next. No journal survives concurrent outside writers;
//! this one is not special.
//!
//! Where it can, it refuses rather than destroys: someone else's file under
//! the destination name is left alone ([`Presence::foreign`]), a book deleted
//! from a computer mid-transaction reads as a lost install so the predecessor
//! goes back, and a record this build cannot read stops the card. That makes
//! the likely accidents survivable; it does not make outside edits supported.
//!
//! It is also why [`Step::InstallStage`] is a move, not a link: the move takes
//! the scratch name as it publishes, so that name still standing means the
//! upload has not landed. A link would keep it — proving the shelf entry is
//! ours while both names live, but losing that signal, so that deleting a book
//! from `/BOOKS` would re-link a dangling entry and free the predecessor.
use heapless::String;

/// Scratch files being streamed, and finished ones awaiting install.
pub const UPLOAD_DIR: &str = "UPLOAD";
/// Predecessors held until the install that replaced them is complete.
pub const ROLLBACK_DIR: &str = "ROLLBACK";
/// The intent record, directly under the cache root.
pub const JOURNAL_FILE: &str = "INSTALL.JNL";

/// Bytes on disk for one intent record. One block, so the write the card
/// performs is the write the record needs.
pub const RECORD_BYTES: usize = 128;

const MAGIC: &[u8; 4] = b"CIJ1";
/// Bumped when the clusters joined the record. Versions 1 and 2 name their
/// files by alias alone, and a development 3 named only the predecessor by
/// chain; all of them are weaker claims than this build acts on, so they are
/// refused rather than replayed under assumptions the build that wrote them
/// could not make.
const VERSION: u16 = 4;
const LFN_BYTES: usize = 64;

const OFF_VERSION: usize = 4;
const OFF_FLAGS: usize = 6;
const OFF_STAGE: usize = 8;
const OFF_OLD: usize = 20;
const OFF_ROLLBACK: usize = 32;
const OFF_OLD_CLUSTER: usize = 44;
const OFF_STAGE_CLUSTER: usize = 48;
const OFF_LFN_LEN: usize = 56;
const OFF_LFN: usize = 58;
/// Last four bytes, so the checksum covers every other byte in the record
/// and no part of it can change unnoticed.
const OFF_CRC: usize = RECORD_BYTES - 4;

/// The long name has to end before the checksum. Widening either it or the
/// layout past this point is a build failure rather than an encode that
/// writes over the checksum it is about to compute.
const _: () = assert!(OFF_LFN + LFN_BYTES <= OFF_CRC);
/// And the fields before it may not run into each other.
const _: () = assert!(OFF_OLD_CLUSTER + 4 <= OFF_STAGE_CLUSTER);
const _: () = assert!(OFF_STAGE_CLUSTER + 4 <= OFF_LFN_LEN);

const FLAG_HAS_OLD: u8 = 1 << 0;

const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

pub(crate) fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// An 8.3 name. Short enough that the record is fixed-size, which is what
/// lets it be written in one block.
pub type ShortName = String<12>;

/// A file the record has to find again after a reset: the name it stood under,
/// and the chain that name pointed at.
///
/// Both, because neither works alone. Names are what this transaction changes,
/// and a freed 8.3 alias goes to whatever the driver writes next. The chain is
/// what a move never touches.
///
/// Two limits. Every zero-length file starts at cluster 0, so there the chain
/// says nothing the name did not — such a transaction is refused outright
/// ([`InstallError::Empty`]). And a chain identifies a file only while it stays
/// allocated: free it and the number goes back to the pool. See
/// [`Step::ReclaimRollback`] for where that still bites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The 8.3 name the entry stood under when the record was written.
    pub alias: ShortName,
    /// The first cluster of its data.
    pub chain: u32,
}

/// What an install is trying to achieve, in full, before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallIntent {
    /// The finished file under `/READER/UPLOAD`. Its chain is what identifies
    /// the book once installed — the scratch name is gone by then.
    pub stage: Located,
    /// The long name the book is installed under. A directory holds at most one
    /// entry under a long name, so this says which entry to ask about;
    /// [`Located::chain`] says whether the answer is ours. Bounded by the
    /// record's field so `encode` cannot overrun it.
    pub long_name: String<LFN_BYTES>,
    /// The book being replaced, if any.
    pub old: Option<Located>,
    /// Where the predecessor is parked while the swap happens.
    pub rollback: ShortName,
}

impl InstallIntent {
    /// Serialise into one block. The trailing bytes are zero, so a record
    /// reads the same whether it was written to a fresh file or over an old
    /// one.
    pub fn encode(&self) -> [u8; RECORD_BYTES] {
        let mut out = [0u8; RECORD_BYTES];
        out[..4].copy_from_slice(MAGIC);
        out[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&VERSION.to_le_bytes());
        out[OFF_FLAGS] = if self.old.is_some() { FLAG_HAS_OLD } else { 0 };

        write_name(
            &mut out[OFF_STAGE..OFF_STAGE + 12],
            self.stage.alias.as_str(),
        );
        out[OFF_STAGE_CLUSTER..OFF_STAGE_CLUSTER + 4]
            .copy_from_slice(&self.stage.chain.to_le_bytes());
        write_name(
            &mut out[OFF_OLD..OFF_OLD + 12],
            self.old.as_ref().map_or("", |old| old.alias.as_str()),
        );
        out[OFF_OLD_CLUSTER..OFF_OLD_CLUSTER + 4]
            .copy_from_slice(&self.old.as_ref().map_or(0, |old| old.chain).to_le_bytes());
        write_name(
            &mut out[OFF_ROLLBACK..OFF_ROLLBACK + 12],
            self.rollback.as_str(),
        );

        let long = self.long_name.as_bytes();
        out[OFF_LFN_LEN] = long.len() as u8;
        out[OFF_LFN..OFF_LFN + long.len()].copy_from_slice(long);

        let crc = fnv1a(&out[..OFF_CRC]);
        out[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Read a record back, or `None` if these bytes are not one.
    ///
    /// A rejection here is not "no transaction": these are whole-record
    /// bytes, so the transaction had started. What it means to a caller is
    /// decided in [`IntentState`], which keeps such a record rather than
    /// discarding it.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RECORD_BYTES || &bytes[..4] != MAGIC {
            return None;
        }
        let version = u16::from_le_bytes([bytes[OFF_VERSION], bytes[OFF_VERSION + 1]]);
        if version != VERSION {
            return None;
        }
        let stored = u32::from_le_bytes([
            bytes[OFF_CRC],
            bytes[OFF_CRC + 1],
            bytes[OFF_CRC + 2],
            bytes[OFF_CRC + 3],
        ]);
        if stored != fnv1a(&bytes[..OFF_CRC]) {
            return None;
        }

        let long_len = usize::from(bytes[OFF_LFN_LEN]);
        if long_len == 0 || long_len > LFN_BYTES {
            return None;
        }
        let long_name = core::str::from_utf8(&bytes[OFF_LFN..OFF_LFN + long_len]).ok()?;
        let mut name = String::<LFN_BYTES>::new();
        name.push_str(long_name).ok()?;

        let stage = Located {
            alias: read_name(&bytes[OFF_STAGE..OFF_STAGE + 12])?,
            chain: read_chain(bytes, OFF_STAGE_CLUSTER),
        };
        // The upload is the one file the record must be able to name: the
        // whole sequence turns on recognising the installed book. `install`
        // refuses an empty body, so nothing this build writes lands here —
        // what does is a record laid out by some other build.
        if stage.chain == 0 {
            return None;
        }
        let rollback = read_name(&bytes[OFF_ROLLBACK..OFF_ROLLBACK + 12])?;
        let old = if bytes[OFF_FLAGS] & FLAG_HAS_OLD != 0 {
            let alias = read_name(&bytes[OFF_OLD..OFF_OLD + 12])?;
            let chain = read_chain(bytes, OFF_OLD_CLUSTER);
            // Same rule, same reason: an empty file cannot be told from any
            // other, and both steps that act on the predecessor take a book
            // off the shelf.
            if alias.is_empty() || chain == 0 {
                return None;
            }
            Some(Located { alias, chain })
        } else {
            None
        };

        if stage.alias.is_empty() || rollback.is_empty() {
            return None;
        }

        Some(Self {
            stage,
            long_name: name,
            old,
            rollback,
        })
    }
}

fn write_name(out: &mut [u8], name: &str) {
    let bytes = name.as_bytes();
    let len = bytes.len().min(out.len());
    out[..len].copy_from_slice(&bytes[..len]);
}

fn read_chain(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_name(bytes: &[u8]) -> Option<ShortName> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text = core::str::from_utf8(&bytes[..end]).ok()?;
    let mut name = ShortName::new();
    name.push_str(text).ok()?;
    Some(name)
}

/// Where the file can be found right now — the whole of what recovery needs
/// to observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    /// The predecessor is still under its own name in `/BOOKS`.
    pub old: bool,
    /// A copy is parked in `/READER/ROLLBACK`.
    pub rollback: bool,
    /// The finished upload is still in `/READER/UPLOAD`, on the chain the
    /// record named — not merely under that name, which a stranger could hold.
    pub stage: bool,
    /// The long name in `/BOOKS` is held by a file on the upload's recorded
    /// chain — the installed book. Holding the name is not enough on its own:
    /// the entry can have been put there by something other than this
    /// transaction, which is what [`Self::foreign`] is for.
    pub dest: bool,
    /// The long name is held by a file on none of this transaction's three
    /// chains — not the upload, not the parked copy, not the predecessor. The
    /// card is browsable from a computer, so a name can be taken while an
    /// upload is in flight. All three are recorded by chain, so this stays
    /// answerable for the whole transaction.
    pub foreign: bool,
}

/// The single next action. Recovery applies one, re-observes, and asks again,
/// so a step that fails is simply a step that gets retried on the next mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Give the predecessor a second name under `/READER/ROLLBACK`.
    RetireOldHolder,
    /// The predecessor now has two names on one chain. Take away the one in
    /// `/BOOKS` — unlink only, since the rollback copy is that same data.
    UnlinkOldHolder,
    /// Give the finished upload its real name in `/BOOKS`.
    ///
    /// A move, which takes the scratch name as it goes. That is load bearing:
    /// while that name stands, the upload has not been installed, and recovery
    /// reads it that way. A link would keep it and lose the distinction — see
    /// the note on the module.
    InstallStage,
    /// The book has two names on one chain, because the move was cut between
    /// its two writes. Take away the scratch one — unlink only, since the
    /// shelf copy is that same data.
    UnlinkStage,
    /// The install is complete and the predecessor is obsolete. The only step
    /// that frees clusters.
    ///
    /// Under the module's contract — this device the only writer — the shelf
    /// entry here is this transaction's book, since nothing else could be on
    /// the chain the record names.
    ///
    /// Outside it, this is the one step that can lose a book: if the scratch
    /// chain was freed elsewhere and its number went to a book copied on under
    /// the same name, the predecessor is freed here for a stranger. No
    /// arrangement of FAT operations closes that — only identity the record
    /// carries itself, such as a digest, which costs reading the candidate
    /// book at every recovery.
    ReclaimRollback,
    /// The upload was lost before it was installed, but the predecessor is
    /// safe in rollback. Put it back where it came from.
    RestoreOldHolder,
    /// The predecessor is back under its own name and the parked copy is the
    /// same chain. Take the parked name away — unlink only.
    UnlinkRollbackCopy,
    /// Nothing left to do; clear the journal.
    Done,
}

/// The next step towards the state `intent` describes, given what is on the
/// card.
///
/// Total by construction: every combination of the observations has an
/// answer, because a recovery that could encounter a state it has no rule for
/// is a recovery that stalls on exactly the crash it exists to handle.
pub fn plan(intent: &InstallIntent, at: Presence) -> Step {
    match (at.stage, at.dest) {
        // The upload has not been installed yet. Clear its destination's name
        // first, which means getting the predecessor out of `/BOOKS`.
        (true, false) => match (at.foreign, at.old, at.rollback) {
            // Somebody else's file holds the name, so the install cannot
            // happen until that is resolved. Nothing is parked, so nothing of
            // ours is off the shelf: give up and let the sweep take the
            // upload. Retrying would refuse every later upload for as long as
            // that file stayed, and retiring the predecessor first would park
            // it for nothing.
            (true, _, false) => Step::Done,
            // Once the predecessor is parked there is no such exit: it cannot
            // go back under a name somebody else holds. The install is still
            // owed, so keep asking — the card takes it the moment the
            // collision clears, and until then the record keeps the sweep off
            // the parked book.
            (true, _, true) => Step::InstallStage,
            (false, true, false) => Step::RetireOldHolder,
            (false, true, true) => Step::UnlinkOldHolder,
            (false, false, _) => Step::InstallStage,
        },
        // Mid-install: the move was cut between its two writes, so both names
        // stand on the one chain.
        (true, true) => Step::UnlinkStage,
        // Installed. Anything parked is obsolete now, and obsolete rather
        // than shared: the installed book came from the scratch file, not
        // from the predecessor.
        (false, true) => match (at.old, at.rollback) {
            // Both stand on the predecessor's chain, so this unlinks rather
            // than reclaims. Only reachable if something outside put a file
            // at the destination alias — and assuming otherwise costs a freed
            // chain that a live name still points at.
            (true, true) => Step::UnlinkRollbackCopy,
            (false, true) => Step::ReclaimRollback,
            (_, false) => Step::Done,
        },
        // Neither the upload nor the installed book is there. Whatever
        // happened, the predecessor is the only book left to protect.
        (false, false) => match (at.old, at.rollback) {
            // Mid-retirement, with the upload gone: unwind the half-done move
            // instead of finishing a transaction that has nothing to install.
            (true, true) => Step::UnlinkRollbackCopy,
            // The predecessor is parked and its own name is free. Put it
            // back; this is the only path that walks the sequence backwards.
            (false, true) if intent.old.is_some() => Step::RestoreOldHolder,
            // A parked copy with no predecessor recorded is not this
            // transaction's to interpret, and unlinking it would discard the
            // only name some other file has left.
            (false, true) => Step::Done,
            // The predecessor stands and nothing else happened, or there was
            // never anything to install. Either way the shelf is consistent.
            (_, false) => Step::Done,
        },
    }
}

// ---------------------------------------------------------------------------
// Applying a plan to a card
// ---------------------------------------------------------------------------

use core::ops::ControlFlow;

use crate::{open_or_make_dir, remove_file_reclaiming_clusters, RemoveStatus};
use embedded_sdmmc::{Directory, Mode, TimeSource};
use proto::cache::{CACHE_ROOT_DIR, CATALOG_FILE};
use proto::identity::Landing;
use proto::library_path::BookRoot;
use proto::source::{SourceDigest, SourceHasher};

/// Why a step could not be carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// The card would not answer a question or would not accept a write.
    /// Always retryable: the transaction stands and the next mount tries
    /// again.
    Card,
    /// The step needed something the record does not describe — a
    /// predecessor step on a record with no predecessor. Retrying cannot
    /// help.
    Malformed,
    /// A transaction is already in flight. Until it is finished, its record
    /// is the only thing that knows where the files it named have got to,
    /// and a second transaction would both destroy that record and mutate
    /// names the first one still owns.
    Busy,
    /// A file this transaction would name has no bytes, so no cluster, so the
    /// record cannot tell it from any other empty file. Refused before
    /// anything is written down, for the upload and the book it replaces
    /// alike. No EPUB is zero bytes, and an empty one on the shelf can be
    /// deleted and the upload retried.
    Empty,
    /// A name resolved to more than one plausible entry, or to one this
    /// transaction cannot work with: case variants of the shelf with no
    /// exact spelling, an exact non-directory squatting on the name beside
    /// a case-variant directory, or, where an upload is to land, two entries
    /// answering alike or a folder carrying the name. A computer can legally
    /// leave any of these behind. Nothing here can pick a reading without
    /// risking the wrong library, and a landing beside the other would be
    /// refused by the shelf halfway through, so the card is refused before
    /// anything is written until it is fixed on a computer. Unlike
    /// [`Self::Card`] a retry without changing the card cannot help.
    Ambiguous,
    /// The library ledger refused: it is damaged, of a version this build
    /// does not read, or full with no record it may evict, so the copy being
    /// replaced cannot have its identity looked up or recorded. The install is
    /// not started, or, after the swap, the book is on the shelf with its
    /// intent standing for the next mount. Nothing changes the shelf until the
    /// ledger is looked at.
    Ledger,
}

/// What a refusal from the library ledger means to an install.
fn ledger_error(fault: crate::ledger::LedgerFault) -> InstallError {
    match fault {
        crate::ledger::LedgerFault::Device => InstallError::Card,
        crate::ledger::LedgerFault::Busy => InstallError::Busy,
        _ => InstallError::Ledger,
    }
}

/// What a recovery pass did, for the caller that has to decide whether its
/// cached view of the shelf is still good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallRecovery {
    /// A step changed what is in `/BOOKS`, so any cached catalog is stale.
    pub touched_shelf: bool,
    /// No leftover scratch or rollback file is still there — whether one
    /// would not delete, or there were more than a pass takes. Housekeeping
    /// only: these files are invisible to the catalog and the reader, so a
    /// false here is worth logging and nothing more. Deliberately not part of
    /// `complete`, or one stale file could turn a committed upload into a
    /// failed one and block a library scan.
    pub swept: bool,
    /// A record was in flight when this pass started — including one this
    /// pass then finished, and one it could not read.
    ///
    /// A cached catalog cannot be trusted when this is true, and
    /// `touched_shelf` is not enough to tell: an install whose shelf-changing
    /// steps happened *before* the reset leaves this pass with only the
    /// rollback copy to reclaim, which changes nothing in `/BOOKS` while the
    /// shelf has already moved on from what the catalog describes.
    pub had_intent: bool,
    /// Every step the plan asked for was carried out and the journal is
    /// clear. When false, the transaction stands and the next mount retries
    /// it — nothing has been abandoned.
    pub complete: bool,
}

pub(crate) type Dir<'a, D, T, const MD: usize, const MF: usize, const MV: usize> =
    Directory<'a, D, T, MD, MF, MV>;

/// The chain this name points at, or `Ok(None)` if the name is not here.
/// `None` when the card would not say, which is never the same as "no".
fn entry_cluster<D, T, const MD: usize, const MF: usize, const MV: usize>(
    directory: &Dir<'_, D, T, MD, MF, MV>,
    name: &str,
) -> Option<Option<embedded_sdmmc::ClusterId>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match directory.find_directory_entry(name) {
        Ok(entry) => Some(Some(entry.cluster)),
        Err(embedded_sdmmc::Error::NotFound) => Some(None),
        Err(_) => None,
    }
}

/// Record an intent durably, before touching anything it describes.
pub fn write_intent<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    intent: &InstallIntent,
) -> Result<(), InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Truncating a record that still describes unfinished work would leave
    // the files it named with nothing to explain them — a predecessor parked
    // under a name no one is looking for.
    match read_intent(root)? {
        IntentState::Absent | IntentState::Truncated => {}
        IntentState::Valid(_) | IntentState::Unrecognized => return Err(InstallError::Busy),
    }
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).map_err(|()| InstallError::Card)?;
    let file = cache_root
        .open_file_in_dir(JOURNAL_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| InstallError::Card)?;
    let bytes = intent.encode();
    let written = file.write(&bytes).is_ok();
    let closed = file.close().is_ok();
    if written && closed {
        Ok(())
    } else {
        Err(InstallError::Card)
    }
}

/// What the journal says, which is three answers rather than two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentState {
    /// No record. Nothing has happened that this transaction started.
    Absent,
    /// A record shorter than one. Written whole before the first mutation and
    /// truncated only after the last, so a short one had either not started
    /// or already finished: nothing to replay. Which of the two cannot be
    /// told from here, and one of them moved the shelf, so a cached catalog
    /// still cannot be trusted.
    Truncated,
    /// A record this pass can act on.
    Valid(InstallIntent),
    /// A whole record this build cannot read: unknown version, failed
    /// checksum, nonsense fields.
    ///
    /// [`Self::Truncated`]'s argument does not hold here. It was written, so
    /// its transaction started, and another build's version describes work in
    /// a shape this one cannot see. Erasing it would destroy the only account
    /// of where a book went, so it is kept and mutations are refused.
    Unrecognized,
}

/// Read back the intent in flight.
///
/// [`InstallError::Card`] means the card would not say, which must not be
/// read as "no transaction" — that would leave a half-done install unrepaired
/// while the shelf is served.
pub fn read_intent<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
) -> Result<IntentState, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(IntentState::Absent),
        Err(_) => return Err(InstallError::Card),
    };
    let file = match cache_root.open_file_in_dir(JOURNAL_FILE, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(IntentState::Absent),
        Err(_) => return Err(InstallError::Card),
    };
    let mut bytes = [0u8; RECORD_BYTES];
    let read = file.read(&mut bytes);
    if file.close().is_err() {
        return Err(InstallError::Card);
    }
    match read {
        Ok(count) if count == RECORD_BYTES => match InstallIntent::decode(&bytes) {
            Some(intent) => Ok(IntentState::Valid(intent)),
            None => Ok(IntentState::Unrecognized),
        },
        // Clearing a journal truncates it before unlinking it, so a
        // zero-length one is very likely a transaction that finished and was
        // interrupted on its way out — with the shelf already changed.
        // Calling that "no record" is how a stale catalog survives.
        Ok(_) => Ok(IntentState::Truncated),
        Err(_) => Err(InstallError::Card),
    }
}

/// Reclaim the record. Public so a caller that has decided there is nothing
/// to replay can stop it retiring the catalog on every mount afterwards.
pub fn clear_intent<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
) -> Result<(), InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(()),
        Err(_) => return Err(InstallError::Card),
    };
    match remove_file_reclaiming_clusters(&cache_root, JOURNAL_FILE) {
        RemoveStatus::Removed | RemoveStatus::Absent => Ok(()),
        RemoveStatus::Failed => Err(InstallError::Card),
    }
}

/// Where each of the four files stands right now.
pub fn observe<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    books: &Dir<'_, D, T, MD, MF, MV>,
    intent: &InstallIntent,
) -> Result<Presence, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).map_err(|()| InstallError::Card)?;
    let upload = open_or_make_dir(&cache_root, UPLOAD_DIR).map_err(|()| InstallError::Card)?;
    let rollback = open_or_make_dir(&cache_root, ROLLBACK_DIR).map_err(|()| InstallError::Card)?;

    // The alias cannot tell these entries apart: retiring the predecessor
    // frees its alias, and the driver hands the same one to the replacement.
    // The chain can, being what a move never changes — the installed book
    // carries the scratch file's, the predecessor the parked copy's.
    let staged = entry_cluster(&upload, intent.stage.alias.as_str()).ok_or(InstallError::Card)?;
    // By the name it was parked under: the move gave it a derived alias.
    let parked =
        holder_of_long_name(&rollback, intent.rollback.as_str())?.map(|(_, cluster)| cluster);
    let holder = holder_of_long_name(books, intent.long_name.as_str())?;

    // The installed book is the one on the upload's chain, recorded before
    // anything moved. Asking instead whether the scratch file is still there
    // would make every holder look installed the moment it went, a
    // stranger's included.
    let dest = match &holder {
        Some((_, cluster)) => cluster.value() == intent.stage.chain,
        None => false,
    };

    // A shelf entry on the parked copy's chain *is* the predecessor, whatever
    // it is called: a restore cut between its two writes puts the book back
    // under a derived alias, and the recorded one is no help there.
    let restored = match (&holder, parked) {
        (Some((_, cluster)), Some(parked)) => *cluster == parked,
        _ => false,
    };

    // Otherwise by its recorded alias, not the long name: a book stored
    // before long names has none, which is why it needs replacing rather than
    // colliding. The alias must still point at the recorded chain, because a
    // freed alias goes to whatever is written next — and a file merely
    // answering to that name is somebody else's, which both steps this
    // authorises would take off the shelf.
    let standing = match &intent.old {
        Some(old) => entry_cluster(books, old.alias.as_str())
            .ok_or(InstallError::Card)?
            .is_some_and(|found| found.value() == old.chain),
        None => false,
    };
    let old = restored || standing;

    // A holder on none of this transaction's three chains is somebody else's
    // file, and moving it aside is the one thing recovery may not do.
    let recorded_old = intent.old.as_ref().map(|old| old.chain);
    let foreign = match &holder {
        Some((_, cluster)) => !dest && !restored && recorded_old != Some(cluster.value()),
        None => false,
    };

    Ok(Presence {
        old,
        rollback: parked.is_some(),
        // On the recorded chain, not merely under the recorded name: a
        // stranger at the scratch name is not this upload.
        stage: staged.is_some_and(|staged| staged.value() == intent.stage.chain),
        dest,
        foreign,
    })
}

/// The alias the driver gave the parked copy, found by the name it was parked
/// under.
fn parked_alias<D, T, const MD: usize, const MF: usize, const MV: usize>(
    rollback: &Dir<'_, D, T, MD, MF, MV>,
    intent: &InstallIntent,
) -> Result<Option<(ShortName, embedded_sdmmc::ClusterId)>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    holder_of_long_name(rollback, intent.rollback.as_str())
}

/// Carry out one step. `Ok(true)` if `/BOOKS` changed.
///
/// Public because an install and a recovery are the same walk: the caller
/// that just staged a file writes its intent and then drives the plan
/// forward exactly as the next mount would.
///
/// Requires a settled reclaim journal, for the reason given on
/// [`recover_installs`]: [`Step::ReclaimRollback`] cannot run while a
/// reclaim record stands.
pub fn apply_step<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    books: &Dir<'_, D, T, MD, MF, MV>,
    intent: &InstallIntent,
    step: Step,
) -> Result<bool, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).map_err(|()| InstallError::Card)?;
    let upload = open_or_make_dir(&cache_root, UPLOAD_DIR).map_err(|()| InstallError::Card)?;
    let rollback_dir =
        open_or_make_dir(&cache_root, ROLLBACK_DIR).map_err(|()| InstallError::Card)?;

    match step {
        Step::RetireOldHolder => {
            let old = intent.old.as_ref().ok_or(InstallError::Malformed)?;
            // The parked copy's long name is its own short name: nothing
            // reads it, and giving it the book's real name would put a second
            // holder of that name in a directory that allows only one.
            books
                .move_file_in_dir_lfn(old.alias.as_str(), &rollback_dir, intent.rollback.as_str())
                .map_err(|_| InstallError::Card)?;
            // A move takes the shelf copy away as it makes the parked one, so
            // the shelf did change.
            Ok(true)
        }
        Step::UnlinkOldHolder => {
            let old = intent.old.as_ref().ok_or(InstallError::Malformed)?;
            books
                .delete_entry_in_dir(old.alias.as_str())
                .map_err(|_| InstallError::Card)?;
            Ok(true)
        }
        Step::InstallStage => {
            upload
                .move_file_in_dir_lfn(
                    intent.stage.alias.as_str(),
                    books,
                    intent.long_name.as_str(),
                )
                .map_err(|_| InstallError::Card)?;
            Ok(true)
        }
        Step::UnlinkStage => {
            upload
                .delete_entry_in_dir(intent.stage.alias.as_str())
                .map_err(|_| InstallError::Card)?;
            Ok(false)
        }
        Step::ReclaimRollback => {
            let Some((alias, _)) = parked_alias(&rollback_dir, intent)? else {
                return Ok(false);
            };
            // Handed to the reclaim journal rather than truncated here.
            //
            // The two journals do not need an atomic update between them,
            // and that is the point of the ordering. This step establishes a
            // reclaim record, unlinks the parked name, frees its chain and
            // clears that record. A reset anywhere inside leaves the reclaim
            // journal describing the rest, and the next mount replays it
            // *before* this journal -- so by the time the install planner
            // looks again, the parked copy is either still there, in which
            // case this step runs afresh, or gone, in which case the planner
            // observes that and advances. Neither state needs this record to
            // have been updated in the same breath.
            //
            // No shelf is passed: a parked copy lives under the cache root,
            // which recovery reaches from `root` alone.
            //
            // The handles this step opened are given up first. The reclaim
            // opens its own, and the directory table is small enough that
            // holding both sets at once exhausts it -- which arrives as a
            // card error and reads like a failing card rather than a
            // bookkeeping mistake.
            drop(rollback_dir);
            drop(upload);
            drop(cache_root);
            match crate::reclaim::reclaim_entry(
                root,
                None,
                crate::reclaim::Place::Rollback,
                alias.as_str(),
            ) {
                Ok(()) => Ok(false),
                // Not this step's to resolve. A reclaim already outstanding,
                // or a journal this build cannot read, is settled before any
                // install step runs -- so meeting one here means the card
                // changed under this pass, and the next mount starts over.
                Err(_) => Err(InstallError::Card),
            }
        }
        Step::RestoreOldHolder => {
            // Restored under the book's long name rather than the alias it
            // used to have: the alias was never the name, and the driver
            // derives a fresh one anyway. But a record with no predecessor
            // has nothing to put back.
            if intent.old.is_none() {
                return Err(InstallError::Malformed);
            }
            let Some((alias, _)) = parked_alias(&rollback_dir, intent)? else {
                return Ok(false);
            };
            rollback_dir
                .move_file_in_dir_lfn(alias.as_str(), books, intent.long_name.as_str())
                .map_err(|_| InstallError::Card)?;
            Ok(true)
        }
        Step::UnlinkRollbackCopy => {
            let Some((alias, _)) = parked_alias(&rollback_dir, intent)? else {
                return Ok(false);
            };
            rollback_dir
                .delete_entry_in_dir(alias.as_str())
                .map_err(|_| InstallError::Card)?;
            Ok(false)
        }
        Step::Done => Ok(false),
    }
}

/// Get rid of a leftover without ever freeing a chain the shelf may share.
///
/// A move that was cut leaves two names on one chain, so a leftover can be
/// the very file a book on the shelf is reading from. Truncating it there
/// frees the shelf copy's clusters, so a shared chain has only its name taken
/// away. Every caller that removes a leftover goes through here: the sweep,
/// and the upload that finds the scratch name already taken.
fn discard_leftover<D, T, const MD: usize, const MF: usize, const MV: usize>(
    directory: &Dir<'_, D, T, MD, MF, MV>,
    books: &Dir<'_, D, T, MD, MF, MV>,
    name: &str,
) -> Result<(), InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(cluster) = entry_cluster(directory, name).ok_or(InstallError::Card)? else {
        return Ok(());
    };
    if shelf_holds_chain(books, cluster).ok_or(InstallError::Card)? {
        directory
            .delete_entry_in_dir(name)
            .map_err(|_| InstallError::Card)?;
    } else if remove_file_reclaiming_clusters(directory, name) == RemoveStatus::Failed {
        return Err(InstallError::Card);
    }
    Ok(())
}

/// Whether any book on the shelf is reading from this chain.
fn shelf_holds_chain<D, T, const MD: usize, const MF: usize, const MV: usize>(
    books: &Dir<'_, D, T, MD, MF, MV>,
    cluster: embedded_sdmmc::ClusterId,
) -> Option<bool>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut shared = false;
    books
        .iterate_dir(|entry| {
            if entry.attributes.is_directory() || entry.attributes.is_volume() {
                return ControlFlow::Continue(());
            }
            if entry.cluster == cluster {
                shared = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })
        .ok()?;
    Some(shared)
}

/// Reclaim files left behind by transactions that are over.
///
/// Only called once the journal is clear, so nothing here is named by a
/// record: an interrupted upload leaves its scratch file, and a transaction
/// whose record was lost can leave a parked copy no later plan will mention.
///
/// Housekeeping, not recovery. These files are invisible to the catalog and
/// the reader, so a failure is worth reporting but must not turn a finished
/// install into a failed one. Bounded per pass: the rest wait for the next
/// mount rather than delaying the shelf, and `false` says they are waiting.
fn sweep_leftovers<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    books: &Dir<'_, D, T, MD, MF, MV>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    const PER_PASS: usize = 8;
    let Ok(cache_root) = open_or_make_dir(root, CACHE_ROOT_DIR) else {
        return false;
    };
    let mut swept = true;
    for directory in [UPLOAD_DIR, ROLLBACK_DIR] {
        let Ok(dir) = open_or_make_dir(&cache_root, directory) else {
            swept = false;
            continue;
        };
        let mut stale: heapless::Vec<ShortName, PER_PASS> = heapless::Vec::new();
        // A file this pass will not get to is still a file left behind, and
        // the caller has no other way to hear about it.
        let mut remaining = false;
        let walked = dir.iterate_dir(|entry| {
            if entry.attributes.is_directory() || entry.attributes.is_volume() {
                return ControlFlow::Continue(());
            }
            let mut name = ShortName::new();
            use core::fmt::Write as _;
            if write!(name, "{}", entry.name).is_err() {
                remaining = true;
                return ControlFlow::Continue(());
            }
            if stale.push(name).is_err() {
                remaining = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        if walked.is_err() {
            swept = false;
            continue;
        }
        // Every name is attempted: one file that will not delete should not
        // strand the others until the next mount.
        for name in &stale {
            if discard_leftover(&dir, books, name.as_str()).is_err() {
                remaining = true;
            }
        }
        swept &= !remaining;
    }
    swept
}

/// Finish whatever install was in flight, then clear the journal.
///
/// Runs before the shelf is served. Each step is applied against a fresh
/// observation, so a step that fails simply leaves the transaction standing
/// for the next mount rather than advancing a phase that did not happen.
///
/// Retires the catalog snapshot first when there is a record to act on — see
/// the note inside on why that is a precondition rather than the caller's
/// housekeeping.
///
/// # The reclaim journal must be settled first
///
/// A precondition, not a preference, and this function does not check it.
/// [`Step::ReclaimRollback`] hands a parked predecessor to
/// [`crate::reclaim::reclaim_entry`], which refuses while a reclaim record
/// stands — so reaching that step with one outstanding turns the whole pass
/// into a card error and leaves the install standing for the next mount. And
/// the steps before it allocate, over a card whose free space an unfinished
/// reclaim is still deciding.
///
/// Every mount-time and session-start caller in the firmware replays
/// [`crate::reclaim::recover`] before calling this, and refuses outright if
/// that will not settle. The one caller that does not is
/// [`StagedUpload::install`], which reaches here after its own record is
/// durable — and it runs inside an upload session, which settled both
/// journals on the way in and re-checks before every command.
pub fn recover_installs<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    books: &Dir<'_, D, T, MD, MF, MV>,
) -> InstallRecovery
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut touched_shelf = false;
    let state = match read_intent(root) {
        Ok(state) => state,
        // A record that cannot be read might be describing a shelf that has
        // already moved, so this is not the same as finding none.
        Err(_) => {
            return InstallRecovery {
                touched_shelf,
                swept: false,
                had_intent: true,
                complete: false,
            }
        }
    };

    // Both of the things this pass may do from here — move books between
    // directories, and clear the record afterwards — end with a card on which
    // nothing says a catalog snapshot describes the shelf as it used to be.
    // The record is that evidence, and recovery's last act is to remove it. So
    // the snapshot goes first, and one that will not go is a reason to leave
    // the whole transaction for the next mount rather than to press on and
    // delete the only thing that would have caught it.
    if matches!(state, IntentState::Valid(_) | IntentState::Truncated)
        && !crate::clear_cache_file(root, CATALOG_FILE)
    {
        return InstallRecovery {
            touched_shelf,
            swept: false,
            had_intent: true,
            complete: false,
        };
    }

    let intent = match state {
        IntentState::Valid(intent) => intent,
        // Kept, deliberately. Sweeping now would clear scratch and rollback
        // files this build cannot know are still spoken for.
        IntentState::Unrecognized => {
            return InstallRecovery {
                touched_shelf,
                swept: false,
                had_intent: true,
                complete: false,
            }
        }
        // Nothing was in flight, so nothing here can move the shelf: the
        // sweep only touches the scratch and rollback directories.
        IntentState::Absent => {
            return InstallRecovery {
                touched_shelf,
                swept: sweep_leftovers(root, books),
                had_intent: false,
                complete: true,
            }
        }
        // Nothing to replay. Reclaim it so it stops retiring the catalog on
        // every mount, but say it was there.
        IntentState::Truncated => {
            let cleared = clear_intent(root).is_ok();
            return InstallRecovery {
                touched_shelf,
                swept: cleared && sweep_leftovers(root, books),
                had_intent: true,
                complete: cleared,
            };
        }
    };

    // One step per observation. The bound is the length of the longest path
    // through `plan`, and exists so a card that keeps reporting a state the
    // step did not change cannot spin here forever.
    for _ in 0..8 {
        let Ok(presence) = observe(root, books, &intent) else {
            return InstallRecovery {
                touched_shelf,
                swept: false,
                had_intent: true,
                complete: false,
            };
        };
        let step = plan(&intent, presence);
        if step == Step::Done {
            let cleared = clear_intent(root).is_ok();
            // Sweeping only once the record is gone: while it stands it names
            // files a reset here would have the next mount pick up again.
            return InstallRecovery {
                touched_shelf,
                swept: cleared && sweep_leftovers(root, books),
                had_intent: true,
                complete: cleared,
            };
        }
        match apply_step(root, books, &intent, step) {
            Ok(changed) => touched_shelf |= changed,
            // Retrying is not a different answer here, it is the same one on
            // every mount for the life of the card — the record cannot
            // describe the step the shelf is asking for, and no amount of
            // waiting changes that. So let it go rather than refuse every
            // upload and delete forever.
            //
            // Nothing is lost by sweeping afterwards. The only observation
            // that plans a predecessor step against a record with no
            // predecessor is a shelf entry sharing the parked copy's chain,
            // and `discard_leftover` unlinks a name whose chain the shelf
            // holds rather than freeing it.
            Err(InstallError::Malformed) => {
                let cleared = clear_intent(root).is_ok();
                return InstallRecovery {
                    touched_shelf,
                    swept: cleared && sweep_leftovers(root, books),
                    had_intent: true,
                    complete: cleared,
                };
            }
            Err(_) => {
                return InstallRecovery {
                    touched_shelf,
                    swept: false,
                    had_intent: true,
                    complete: false,
                }
            }
        }
    }
    InstallRecovery {
        touched_shelf,
        swept: false,
        had_intent: true,
        complete: false,
    }
}

// ---------------------------------------------------------------------------
// The caller's side: stage a file, then ask for it to be installed
// ---------------------------------------------------------------------------

use crate::same_long_name;
use embedded_sdmmc::File;

/// How many rollback names to try before giving up. Each is derived from the
/// book, so a collision means a copy parked by a transaction whose record was
/// lost; sixteen of those for one book is not a card this code can help with.
const ROLLBACK_PROBE_LIMIT: u16 = 16;

fn with_extension(alias: &str, extension: &str) -> ShortName {
    let mut name = ShortName::new();
    let _ = name.push_str(alias.split('.').next().unwrap_or(alias));
    let _ = name.push_str(extension);
    name
}

/// A book as the pre-long-name uploader stored it, so a re-upload replaces it
/// instead of landing beside it.
///
/// Those books are an 8.3 alias with no long name, their real filename in a
/// `.TXT` label beside a `.ID` identity. A new upload's long name therefore
/// matches nothing on the shelf, and without this the user gets two copies.
/// Identity rather than the label, because labels are truncated display
/// strings that two books can share — which is why the sidecar exists.
#[derive(Debug, Clone)]
pub struct LegacyKey {
    /// The alias the old uploader derived from this client filename.
    pub alias: ShortName,
    /// The identity hash it recorded alongside it.
    pub identity: u64,
}

/// How many consecutive aliases the old uploader could have used for one
/// book. It probed a fixed window from the derived name, so a book that
/// collided with others sits somewhere inside it.
const LEGACY_PROBE_WINDOW: u16 = 16;

/// The pre-long-name book matching `key`, if one is still on the shelf.
///
/// `None` is a card that would not answer: proceeding would install a second
/// copy of a book that is already there.
fn legacy_holder<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    books: &Dir<'_, D, T, MD, MF, MV>,
    key: &LegacyKey,
) -> Option<Option<Located>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let (prefix, base_tail) = split_legacy_alias(key.alias.as_str())?;
    for probe in 0..LEGACY_PROBE_WINDOW {
        let candidate = legacy_alias(&prefix, base_tail.wrapping_add(u32::from(probe)));
        let Some(chain) = entry_cluster(books, candidate.as_str())? else {
            continue;
        };
        match crate::read_upload_identity(root, candidate.as_str()) {
            Ok(Some(identity)) if identity == key.identity => {
                return Some(Some(Located {
                    alias: candidate,
                    chain: chain.value(),
                }))
            }
            // A book with no identity, or a different one, is a different
            // book that happens to sit in this window.
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
    Some(None)
}

/// `PPPPTTTT.EPU` split into its prefix and base-36 tail.
fn split_legacy_alias(alias: &str) -> Option<(String<4>, u32)> {
    let bytes = alias.as_bytes();
    if bytes.len() != 12 || &bytes[8..12] != b".EPU" {
        return None;
    }
    if !bytes[..8]
        .iter()
        .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
    {
        return None;
    }
    let mut prefix = String::<4>::new();
    prefix.push_str(&alias[..4]).ok()?;
    let mut tail = 0u32;
    for &byte in &bytes[4..8] {
        tail = tail * 36
            + match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                _ => u32::from(byte - b'A' + 10),
            };
    }
    Some((prefix, tail))
}

fn legacy_alias(prefix: &str, tail: u32) -> ShortName {
    let mut name = ShortName::new();
    let _ = name.push_str(prefix);
    for digit in proto::upload::base36_tail(tail) {
        let _ = name.push(digit as char);
    }
    let _ = name.push_str(".EPU");
    name
}

/// A book being streamed into scratch space, under a name nothing reads.
///
/// Nothing in `/BOOKS` is touched until [`StagedUpload::install`], so an
/// upload that is interrupted — by a lost connection, a full card, a reset —
/// leaves a scratch file and no trace in the library.
/// A book that reached the shelf, and what its bytes are.
///
/// Only a landing produces one, so an identity here always describes bytes a
/// reader can reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landed {
    /// The 8.3 name the book now answers to.
    pub alias: ShortName,
    /// The identity of the bytes that landed.
    pub source: SourceDigest,
}

pub struct StagedUpload<'a, D, T, const MD: usize, const MF: usize, const MV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: File<'a, D, T, MD, MF, MV>,
    stage: ShortName,
    long_name: String<64>,
    legacy: Option<LegacyKey>,
    /// Accumulated over exactly the bytes that reached the file, and it goes
    /// no further than this struct until a landing publishes it. An abandoned
    /// upload drops it with everything else it staged.
    ///
    /// Costs 240 B of `.bss` on both boards, measured by building with this
    /// field stubbed out: X3 247536 against 247296, X4 242736 against 242496.
    /// It lands in static task storage rather than on a stack because
    /// `write_one_book` holds the `StagedUpload` across its awaits. Stack
    /// frames are unaffected, with the largest still 13.8 KB against a 24 KB
    /// budget.
    hasher: SourceHasher,
}

impl<'a, D, T, const MD: usize, const MF: usize, const MV: usize> StagedUpload<'a, D, T, MD, MF, MV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Open scratch space for a book that will be installed as `long_name`.
    ///
    /// `legacy` describes the same book as the old uploader would have stored
    /// it, so a re-upload of a book that predates long names replaces it
    /// rather than landing beside it. `None` skips that lookup.
    pub fn begin(
        root: &Directory<'a, D, T, MD, MF, MV>,
        books: &Directory<'_, D, T, MD, MF, MV>,
        long_name: &str,
        legacy: Option<LegacyKey>,
    ) -> Result<Self, InstallError> {
        let mut name = String::<64>::new();
        name.push_str(long_name)
            .map_err(|_| InstallError::Malformed)?;

        // A record in flight owns the scratch directory, the alias it is
        // installing to, and the predecessor it parked. Staging alongside it
        // would take names it is still counting on, and installing would
        // destroy the only description of where its files went.
        match read_intent(root)? {
            IntentState::Absent | IntentState::Truncated => {}
            IntentState::Valid(_) | IntentState::Unrecognized => return Err(InstallError::Busy),
        }
        // The library's transaction too. A replacement whose landing has not
        // been settled owns its place until it is, and nothing else may
        // change the shelf while it stands.
        if crate::replace::read(root).map_err(ledger_error)?.is_some() {
            return Err(InstallError::Busy);
        }

        let alias = proto::upload::upload_short_alias(long_name, 0);
        let stage = with_extension(alias.as_str(), ".TMP");

        let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).map_err(|()| InstallError::Card)?;
        let upload = open_or_make_dir(&cache_root, UPLOAD_DIR).map_err(|()| InstallError::Card)?;
        // A leftover from an abandoned upload is cleared rather than appended
        // to. Not always by reclaiming it: an install cut half way through
        // leaves the scratch file and the book it published sharing one
        // chain, and if the record was lost too this is the next thing to
        // touch that name.
        discard_leftover(&upload, books, stage.as_str())?;
        let file = upload
            .open_file_in_dir(stage.as_str(), Mode::ReadWriteCreate)
            .map_err(|_| InstallError::Card)?;
        Ok(Self {
            file,
            stage,
            long_name: name,
            legacy,
            hasher: SourceHasher::new(),
        })
    }

    /// Append to the staged body, and to its identity.
    ///
    /// The hash follows the write, so bytes the card refused are absent from
    /// both and the digest cannot describe a file that failed mid-write.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), InstallError> {
        self.file.write(bytes).map_err(|_| InstallError::Card)?;
        self.hasher.update(bytes);
        Ok(())
    }

    /// Give up, leaving the library exactly as it was.
    pub fn abandon(self, root: &Directory<'_, D, T, MD, MF, MV>) {
        let _ = self.file.close();
        if let Ok(cache_root) = root.open_dir(CACHE_ROOT_DIR) {
            if let Ok(upload) = cache_root.open_dir(UPLOAD_DIR) {
                let _ = remove_file_reclaiming_clusters(&upload, self.stage.as_str());
            }
        }
    }

    /// Publish the staged file to `/BOOKS` under its long name, replacing any
    /// book already holding that name.
    ///
    /// `Ok(Some(landed))` carries the 8.3 name the book now answers to and
    /// the identity of the bytes behind it. `Ok(None)` is a transaction that
    /// finished and installed nothing, a rollback or a name that turned out to
    /// belong to somebody else, and carries no identity because those bytes
    /// are on no shelf. `Err` says why it could not be finished here, which
    /// does not always mean it will not happen: once the intent is durable, an
    /// install interrupted from this point is finished by the next mount.
    ///
    /// The library's own intent is published before the filesystem's and
    /// settled after it, so the copy under this name keeps its `BookId`
    /// across the swap; see [`crate::replace`]. `random` mints an id for a
    /// place the ledger has not adopted.
    pub fn install(
        self,
        root: &Directory<'_, D, T, MD, MF, MV>,
        books: &Directory<'_, D, T, MD, MF, MV>,
        random: &mut impl FnMut() -> u32,
    ) -> Result<Option<Landed>, InstallError> {
        let Self {
            file,
            stage,
            long_name,
            legacy,
            hasher,
        } = self;
        let source = hasher.finish();
        // Until the close succeeds the file is not durable, and installing a
        // book the card has not finished writing is the one thing staging
        // exists to prevent.
        if file.close().is_err() {
            discard_scratch(root, stage.as_str());
            return Err(InstallError::Card);
        }

        // Read after the close, never before: a file's entry is only brought
        // up to date when it is flushed, so an open file's chain is whatever
        // it was before the body was written.
        let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).map_err(|()| InstallError::Card)?;
        let upload = open_or_make_dir(&cache_root, UPLOAD_DIR).map_err(|()| InstallError::Card)?;
        let staged = entry_cluster(&upload, stage.as_str())
            .ok_or(InstallError::Card)?
            .ok_or(InstallError::Card)?;
        // A body with no clusters is a body the record cannot name. Recording
        // it anyway would make every other empty file on the shelf answer to
        // the same identity — including the one being replaced, which recovery
        // would then read as this upload already installed.
        if staged.value() == 0 {
            drop(upload);
            drop(cache_root);
            discard_scratch(root, stage.as_str());
            return Err(InstallError::Empty);
        }
        let staged = staged.value();
        // Dropped before the rest: every lookup below opens its own handles,
        // and the directory table is small enough that holding these would
        // starve them.
        drop(upload);
        drop(cache_root);

        // The chain is recorded alongside each alias: it is what still
        // identifies these files after their names have been rewritten, freed
        // and handed to something else.
        // The exact spelling the holder has is kept beside it: the ledger
        // names places exactly where the card matches them by FAT's rules, so
        // an upload spelled another way is a replacement of this copy that
        // will also respell its place.
        let mut predecessor_spelling = String::<{ proto::library_path::MAX_PATH_BYTES }>::new();
        // Two case variants of the name with neither spelled exactly is a
        // card this transaction must not touch: either could be the copy
        // meant, and parking one would take a book off the shelf that no
        // record then explains. Refused here, before anything is journalled.
        let holder = match spelled_holder_of_long_name(books, long_name.as_str()) {
            Ok(holder) => holder,
            Err(error) => {
                discard_scratch(root, stage.as_str());
                return Err(error);
            }
        };
        let old = match holder {
            Some((alias, chain, spelled)) => {
                predecessor_spelling = spelled;
                Some(Located {
                    alias,
                    chain: chain.value(),
                })
            }
            // Nothing on the shelf carries this long name, which for a book
            // stored before long names existed is exactly what it would look
            // like whether or not it is there. Such a book has no long name,
            // so its alias is its spelling.
            None => match &legacy {
                Some(key) => {
                    let found = legacy_holder(root, books, key).ok_or(InstallError::Card)?;
                    if let Some(found) = &found {
                        predecessor_spelling
                            .push_str(found.alias.as_str())
                            .map_err(|_| InstallError::Card)?;
                    }
                    found
                }
                None => None,
            },
        };
        // Same rule for the book being replaced: a transaction that cannot
        // name the file it is about to move off the shelf must not start. An
        // empty EPUB can be deleted and the upload retried, which is a plainer
        // answer than acting on a file the record cannot recognise again.
        if old.as_ref().is_some_and(|old| old.chain == 0) {
            discard_scratch(root, stage.as_str());
            return Err(InstallError::Empty);
        }
        // Probed rather than derived from the book alone. A copy parked by a
        // transaction whose record was lost still sits under that derived
        // name, and linking onto a name that is taken fails every time it is
        // tried -- which would wedge this book's uploads permanently.
        let rollback = free_rollback_name(root, stage.as_str()).ok_or(InstallError::Card)?;
        let intent = InstallIntent {
            rollback,
            stage: Located {
                alias: stage,
                chain: staged,
            },
            long_name,
            old,
        };
        // The library's intent first, durable before the filesystem
        // transaction begins: which copy this is, what stood at its place,
        // and which bytes are meant to land. Nothing below touches the shelf
        // until it has returned. The predecessor's bytes were not read here,
        // so it is "unknown" to the intent whatever the ledger recorded of
        // them; see `crate::replace` for why that is the safe claim.
        let predecessor = match &intent.old {
            Some(located) => Some(crate::replace::PredecessorSeen {
                locator: predecessor_spelling.as_str(),
                byte_size: books
                    .find_directory_entry(located.alias.as_str())
                    .map_err(|_| InstallError::Card)?
                    .size,
                digest: None,
            }),
            None => None,
        };
        crate::replace::begin(
            root,
            BookRoot::Library,
            intent.long_name.as_str(),
            predecessor,
            source,
            random,
        )
        .map_err(ledger_error)?;
        write_intent(root, &intent)?;

        if !recover_installs(root, books).complete {
            return Err(InstallError::Card);
        }
        // The plan is the authority on whether the book landed, not the step
        // count: a transaction that ended in a rollback completed cleanly and
        // still installed nothing.
        //
        // No host test reaches this branch. `old` above is whichever file
        // holds the long name at this moment, so a stranger arriving between
        // `begin` and here is replaced rather than refused, and a completed
        // recovery landing elsewhere needs a writer this transaction excludes
        // or a card that lost the staged file. It stays because recovery after
        // a reset reaches these states for real.
        if !observe(root, books, &intent)?.dest {
            crate::replace::settle(root, Landing::Old).map_err(ledger_error)?;
            return Ok(None);
        }
        // The retired book's label and identity now describe a name that is
        // gone. Best-effort: a sidecar left behind is only read for a book
        // that still exists.
        if legacy.is_some() {
            if let Some(retired) = &intent.old {
                crate::delete_upload_sidecars(root, retired.alias.as_str());
            }
        }
        // The alias the driver derived, an artefact of the format rather than
        // the book's name. Checked against the chain `observe` just proved, so
        // the digest travels with the file that was verified rather than with
        // whatever holds the name by the time this reads it.
        //
        // A failed walk is an error rather than an empty answer: reporting no
        // landing for a book that landed would lose the identity and log a
        // success as a failure.
        let holder = holder_of_long_name(books, intent.long_name.as_str())?;
        match holder {
            Some((alias, chain)) if chain.value() == intent.stage.chain => {
                // The destination is this upload's chain, which is the file
                // the digest was taken over, so the landing is known without
                // reading the book again. A settle that fails leaves the
                // intent standing for the next mount to resolve by reading
                // the destination; the book is on the shelf either way.
                crate::replace::settle(root, Landing::New).map_err(ledger_error)?;
                Ok(Some(Landed { alias, source }))
            }
            // `observe` proved the destination was on this upload's chain a
            // moment ago, so the name being gone or on another chain means
            // something outside this transaction wrote to the card.
            _ => Err(InstallError::Card),
        }
    }
}

/// Reclaim a scratch file for a transaction that will not be started.
///
/// Best-effort: nothing under `/READER/UPLOAD` is visible to the catalog or
/// the reader, and the sweep comes back for whatever is left.
fn discard_scratch<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    stage: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if let Ok(cache_root) = root.open_dir(CACHE_ROOT_DIR) {
        if let Ok(upload) = cache_root.open_dir(UPLOAD_DIR) {
            let _ = remove_file_reclaiming_clusters(&upload, stage);
        }
    }
}

/// A name in the rollback directory that nothing holds.
fn free_rollback_name<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    stage: &str,
) -> Option<ShortName>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).ok()?;
    let rollback = open_or_make_dir(&cache_root, ROLLBACK_DIR).ok()?;
    let stem = stage.split('.').next().unwrap_or(stage);
    for probe in 0..ROLLBACK_PROBE_LIMIT {
        let mut candidate = ShortName::new();
        if probe == 0 {
            candidate.push_str(stem).ok()?;
        } else {
            // Trade a character of the stem for the probe digit rather than
            // overflow the 8.3 stem.
            let keep = stem.len().min(7);
            candidate.push_str(&stem[..keep]).ok()?;
            candidate
                .push(char::from_digit(u32::from(probe) % 36, 36)?.to_ascii_uppercase())
                .ok()?;
        }
        candidate.push_str(".OLD").ok()?;
        // Anything answering to the name has it, a directory as much as a
        // book: the predecessor is parked by moving it in under this name,
        // and FAT counts every entry in one namespace. A blocked name is
        // the next probe's business rather than a refusal, since this one
        // is chosen rather than given.
        if classify_long_name(&rollback, candidate.as_str())
            .ok()?
            .is_free()
        {
            return Some(candidate);
        }
    }
    None
}

/// The book currently holding `long_name`, if one does.
///
/// `Ok(None)` is a name nobody holds. [`InstallError::Card`] is a shelf that
/// would not say, which must stop the install: proceeding would put a
/// second holder of the name on the card. [`InstallError::Ambiguous`] is a
/// name held by something this transaction cannot move aside; see
/// [`classify_long_name`].
fn holder_of_long_name<D, T, const MD: usize, const MF: usize, const MV: usize>(
    books: &Directory<'_, D, T, MD, MF, MV>,
    long_name: &str,
) -> Result<Option<(ShortName, embedded_sdmmc::ClusterId)>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    spelled_holder_of_long_name(books, long_name)
        .map(|holder| holder.map(|(alias, chain, _)| (alias, chain)))
}

/// [`holder_of_long_name`], with the long name exactly as the holder spells
/// it, which is the locator the ledger knows the copy by.
fn spelled_holder_of_long_name<D, T, const MD: usize, const MF: usize, const MV: usize>(
    books: &Directory<'_, D, T, MD, MF, MV>,
    long_name: &str,
) -> Result<Option<SpelledHolder>, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let held = classify_long_name(books, long_name)?;
    if held.blocked {
        return Err(InstallError::Ambiguous);
    }
    Ok(held.book)
}

/// One entry a long-name lookup found, with the name exactly as it spells
/// it.
type SpelledHolder = (
    ShortName,
    embedded_sdmmc::ClusterId,
    String<{ proto::library_path::MAX_PATH_BYTES }>,
);

/// What answers to a name in a directory.
///
/// A struct rather than the three-way enum it reads as, because a holder
/// carries a whole locator and the empty answers would carry that width
/// with them.
struct NameHolder {
    /// The one book holding the name, when a book is all that holds it. An
    /// upload to that name replaces it.
    book: Option<SpelledHolder>,
    /// Something an upload cannot land beside or move aside holds the name.
    blocked: bool,
}

impl NameHolder {
    /// Nothing holds the name, so an upload lands there fresh.
    fn is_free(&self) -> bool {
        self.book.is_none() && !self.blocked
    }
}

/// What holds `long_name` in `books`, by the rules the landing will meet.
///
/// FAT gives a directory one namespace over its entries' long names and
/// their aliases together, with case ignored, and the install lands by
/// moving the staged file in under the name. So everything that answers to
/// the name is counted here, whatever kind of entry it is and however it is
/// spelled, and anything other than a single book blocks the name:
///
/// - Two entries answering alike, which a computer can leave behind as case
///   variants. Whichever one the install replaced, the other would be there
///   to refuse the landing, and the rollback after it, halfway through the
///   transaction.
/// - A directory. It is not a book to replace, it cannot be parked, and it
///   would refuse the landing the same way.
/// - A book whose name will not fit the buffers the steps that move and
///   delete it use, which is a book this transaction cannot name again.
///
/// A blocked name refuses the install before anything is journalled or
/// moved, which is the whole point of asking here. Directory order decides
/// nothing: with one book there is nothing to order, and with anything else
/// the answer is the same whichever came first.
///
/// An entry with no long name answers to its rendered alias, which is the
/// name a listing shows for it and the locator the library adopts it under.
/// Only a name an alias could be is compared against one, which no upload
/// is: a `.epub` extension is four characters where 8.3 allows three.
fn classify_long_name<D, T, const MD: usize, const MF: usize, const MV: usize>(
    books: &Directory<'_, D, T, MD, MF, MV>,
    long_name: &str,
) -> Result<NameHolder, InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut storage = [0u8; crate::LFN_SCAN_BYTES];
    let mut lfn = embedded_sdmmc::LfnBuffer::new(&mut storage);
    let mut holder: Option<SpelledHolder> = None;
    let mut answered = 0usize;
    let mut blocked = false;
    let alias_shaped = long_name.len() <= proto::storage::MAX_ALIAS_UTF8_BYTES;
    let walked = books.iterate_dir_lfn(&mut lfn, |entry, found| {
        if entry.attributes.is_volume() {
            return ControlFlow::Continue(());
        }
        use core::fmt::Write as _;
        let mut rendered = String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
        let held = match found {
            Some(long) => long,
            None => {
                if !alias_shaped || write!(rendered, "{}", entry.name).is_err() {
                    return ControlFlow::Continue(());
                }
                rendered.as_str()
            }
        };
        if !same_long_name(held, long_name) {
            return ControlFlow::Continue(());
        }
        answered += 1;
        let mut name = ShortName::new();
        let mut spelled = String::new();
        if entry.attributes.is_directory()
            || answered > 1
            || write!(name, "{}", entry.name).is_err()
            || spelled.push_str(held).is_err()
        {
            blocked = true;
            // Blocked is already the answer; the rest of the walk cannot
            // change it.
            return ControlFlow::Break(());
        }
        holder = Some((name, entry.cluster, spelled));
        ControlFlow::Continue(())
    });
    walked.map_err(|_| InstallError::Card)?;
    Ok(NameHolder {
        book: if blocked { None } else { holder },
        blocked,
    })
}
