//! Managed replacement: keeping a copy's [`BookId`] when an upload lands over
//! it.
//!
//! An upload that lands under a name the shelf already holds is two
//! transactions. The filesystem one, `INSTALL.JNL`, swaps the bytes and
//! clears itself. The library one is what this module keeps: that the copy
//! at that place is the same copy, now made of other bytes, and that its
//! record in the ledger should say so. Nothing bridged them before, so a
//! power cut after the install had cleared its journal and before the
//! ledger had been told left a record saying one thing and a file holding
//! another, which the next scan could only read as a stranger at a known
//! path, and mint a fresh id for. Reading position, once it hangs from the
//! id, would have gone with the old one.
//!
//! So the installer publishes an intent here before `INSTALL.JNL` is
//! written: which id, at which place, what stood there, and which bytes are
//! meant to land. It stands after `INSTALL.JNL` clears and is cleared only
//! once the ledger record has been rewritten. Recovery runs it after the
//! filesystem journals have settled, and asks the card rather than the
//! record which side won: the destination's bytes are hashed and compared
//! to the intent. The new digest is decisive. A predecessor whose digest
//! was read in the same session is recognised by it. One whose bytes were
//! not read is recognised as any file that is not the new bytes, which
//! holds only because a live intent is a storage transaction for the
//! sole-writer contract: nothing else was permitted to write while it
//! stood. Where nothing stood, nothing standing is the old landing. Anything
//! else keeps the intent and refuses, and while it stands no other
//! replacement may begin and no scan may adopt.
//!
//! What the ledger recorded about the predecessor's bytes is not promoted
//! into the intent. Between transactions a computer may replace a file with
//! another of the same size, and the ledger cannot tell; carrying its digest
//! forward as the predecessor's would make that predecessor unrecognisable
//! at recovery and wedge the intent on a rollback that recovered perfectly.
//! So the predecessor is "known" only to a caller that hashed it in this
//! session, and the installer, which does not, says "unknown".
//!
//! Names are exact in the ledger and equivalent by FAT's rules on the card,
//! so a replacement can respell its place: an upload of `dune.epub` replaces
//! `Dune.epub`, and a rollback puts the predecessor back under the spelling
//! that was typed. The intent carries both spellings, and settling moves the
//! id to whichever the file ends up under.
//!
//! A copy the ledger has not adopted needs a record, and a ledger with no
//! room for one makes it by letting a missing copy go, chosen here and
//! written into the intent: a record the last scan found missing whose place
//! is verified empty now. A counter alone would not do, since the copy may
//! have been put back between transactions; the check against the card is
//! what makes it safe, and the sole-writer contract keeps it true while the
//! intent stands. With no such record the install is refused before anything
//! is journalled.
//!
//! The intent is kept in `/READER/REPLACE.JNL` the way the ledger journal
//! is: two slots written alternately with a sequence number, so a torn
//! publication leaves the entry before it. A torn publication is an install
//! that has not begun, since the installer waits for it; a torn clear is an
//! intent that still stands, which recovery resolves again, and resolving is
//! idempotent.
//!
//! No `BookId` or digest enters `INSTALL.JNL` or `RECLAIM.JNL`. The
//! filesystem transaction decides what the card holds, and this one only
//! records what that means for identity.

use core::fmt::Write as _;

use embedded_sdmmc::{Directory, TimeSource};
use heapless::String;
use proto::cache::CACHE_ROOT_DIR;
use proto::identity::{
    classify_replace_journal, encode_replace_journal, landing, BookId, Landing, LedgerRecord,
    Predecessor, ReplaceIntent, ReplaceJournal, ReplaceJournalReading, REPLACE_JOURNAL_BYTES,
    REPLACE_JOURNAL_SLOTS, REPLACE_JOURNAL_SLOT_BYTES,
};
use proto::library_path::{BookRoot, LibraryPath, MAX_PATH_BYTES};
use proto::source::{CachedSourceDigest, SourceDigest};

use crate::ledger::{self, LedgerFault, SlotVerdict, LEDGER_MAX_RECORDS};

/// The intent, beside the ledger it will be published to.
pub const REPLACE_JOURNAL: &str = "REPLACE.JNL";

const _: () = assert!(REPLACE_JOURNAL_SLOTS == ledger::SLOT_COUNT);

/// What the caller saw at the destination before the install: the exact
/// spelling of the file holding the place, its size, and its digest if the
/// caller read it in this session. Nothing read is nothing known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PredecessorSeen<'a> {
    pub locator: &'a str,
    pub byte_size: u32,
    pub digest: Option<SourceDigest>,
}

/// A replacement in flight, as read back off the card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Standing {
    pub id: BookId,
    pub root: BookRoot,
    /// Where the install lands, spelled as typed.
    pub locator: String<MAX_PATH_BYTES>,
    pub predecessor: Predecessor,
    /// The exact spelling the predecessor held the place under, when there
    /// was one.
    pub predecessor_locator: Option<String<MAX_PATH_BYTES>>,
    /// The missing copy whose record makes room for this one, if the ledger
    /// had no room.
    pub evict: Option<BookId>,
    pub new: CachedSourceDigest,
}

impl Standing {
    fn from_intent(intent: &ReplaceIntent<'_>) -> Result<Self, LedgerFault> {
        let mut locator = String::new();
        locator
            .push_str(intent.locator)
            .map_err(|_| LedgerFault::Record)?;
        let predecessor_locator = match intent.predecessor_locator {
            Some(spelled) => {
                let mut owned = String::new();
                owned.push_str(spelled).map_err(|_| LedgerFault::Record)?;
                Some(owned)
            }
            None => None,
        };
        Ok(Self {
            id: intent.id,
            root: intent.root,
            locator,
            predecessor: intent.predecessor,
            predecessor_locator,
            evict: intent.evict,
            new: intent.new,
        })
    }
}

/// The replacement in flight, or `None` when none is.
pub fn read<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<Standing>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    standing(&cache_root)
}

/// Publish the intent for an install about to begin.
///
/// `locator` is where the install lands, under `at`, spelled as typed;
/// `predecessor` is what holds that place now, or `None` for no file; `new`
/// is the identity of the bytes staged for it. The copy keeps the id of the
/// ledger record naming the predecessor's exact place and size, if there is
/// one; a place no record names is a copy the library has not adopted, and
/// is minted an id here so that the landing can be published under it.
///
/// Refuses with [`LedgerFault::Busy`] while another intent stands, and with
/// [`LedgerFault::Full`] when a fresh id would need a record the ledger has
/// no room for and no missing copy, verified absent from the card now, can
/// make it. Either refusal comes before anything is journalled. The caller
/// must not touch the destination until this returns: the intent is durable
/// then, and not before.
pub fn begin<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    at: BookRoot,
    locator: &str,
    predecessor: Option<PredecessorSeen<'_>>,
    new: SourceDigest,
    random: &mut impl FnMut() -> u32,
) -> Result<Standing, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = ledger::open_or_make_cache_root(root)?;
    if standing(&cache_root)?.is_some() {
        return Err(LedgerFault::Busy);
    }
    let live = ledger::open(root)?;
    let known = match (&live, predecessor) {
        (Some(live), Some(seen)) => {
            ledger::find_record(root, live, at, seen.locator, seen.byte_size)?
        }
        _ => None,
    };
    let (id, evict) = match known {
        Some((id, _)) => (id, None),
        None => {
            let evict = match &live {
                Some(live) if live.count as usize >= LEDGER_MAX_RECORDS => {
                    Some(room_in(root, live)?.ok_or(LedgerFault::Full)?)
                }
                _ => None,
            };
            (BookId::mint(random), evict)
        }
    };
    let (kind, predecessor_locator) = match predecessor {
        None => (Predecessor::None, None),
        Some(PredecessorSeen {
            locator,
            digest: Some(read),
            ..
        }) => (
            Predecessor::Known(CachedSourceDigest::new(read)),
            Some(locator),
        ),
        Some(PredecessorSeen { locator, .. }) => (Predecessor::Unknown, Some(locator)),
    };
    let intent = ReplaceIntent {
        id,
        root: at,
        locator,
        predecessor: kind,
        predecessor_locator,
        evict,
        new: CachedSourceDigest::new(new),
    };
    publish(&cache_root, &ReplaceJournal::Standing(intent))?;
    Standing::from_intent(&intent)
}

/// A record a full ledger can let go of: one the last scan found missing
/// whose place is empty on the card now. The first such in ledger order, or
/// `None` when every missing copy has come back.
fn room_in<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    live: &ledger::Ledger,
) -> Result<Option<BookId>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Reading the ledger holds a directory and a file open, and looking a
    // place up on the card opens a directory per path component, so
    // candidates are gathered in batches and resolved between reads rather
    // than resolved as they are read. A batch is small because each holds a
    // locator, and a second pass is rare: it takes a whole batch of missing
    // copies to have been put back since the last scan.
    const BATCH: usize = 8;
    let mut candidates: heapless::Vec<(BookId, BookRoot, String<MAX_PATH_BYTES>), BATCH> =
        heapless::Vec::new();
    let mut from = 0u32;
    loop {
        candidates.clear();
        let mut next = from;
        ledger::for_each_record(root, live, &mut |index, record| {
            if u32::from(index) < from || record.misses == 0 || candidates.is_full() {
                return Ok(());
            }
            let mut locator = String::new();
            locator
                .push_str(record.locator)
                .map_err(|_| LedgerFault::Record)?;
            candidates
                .push((record.id, record.root, locator))
                .map_err(|_| LedgerFault::Record)?;
            next = u32::from(index) + 1;
            Ok(())
        })?;
        for (id, at, locator) in &candidates {
            if !holds_a_file(root, *at, locator.as_str())? {
                return Ok(Some(*id));
            }
        }
        if !candidates.is_full() {
            return Ok(None);
        }
        from = next;
    }
}

/// Settle the intent in flight, the landing being known.
///
/// A new landing rewrites the copy's ledger record under the same id with
/// the new size and digest, at the spelling the install used, letting the
/// intent's missing copy go if the ledger had no room. An old landing leaves
/// the record's size and digest as they were; if the predecessor has been
/// put back under the spelling the install used rather than its own, which
/// a rollback does, the record moves to that spelling. Either way the intent
/// is then cleared. Nothing to settle is not an error: a clear that was cut
/// leaves an intent recovery resolves again, and a caller may settle what
/// recovery already did.
pub fn settle<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    landed: Landing,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = ledger::open_or_make_cache_root(root)?;
    let Some(intent) = standing(&cache_root)? else {
        return Ok(());
    };
    match landed {
        Landing::New => {
            let live = ledger::open(root)?;
            let byte_size =
                u32::try_from(intent.new.byte_len()).map_err(|_| LedgerFault::Record)?;
            ledger::publish_record(
                root,
                live,
                &LedgerRecord {
                    id: intent.id,
                    root: intent.root,
                    locator: intent.locator.as_str(),
                    byte_size,
                    misses: 0,
                    source: Some(intent.new),
                },
                intent.evict,
            )?;
        }
        Landing::Old => {
            let respelled = match &intent.predecessor_locator {
                Some(spelled) if spelled.as_str() != intent.locator.as_str() => {
                    holds_a_file(root, intent.root, intent.locator.as_str())?
                        && !holds_a_file(root, intent.root, spelled.as_str())?
                }
                _ => false,
            };
            if respelled {
                if let Some(live) = ledger::open(root)? {
                    ledger::relocate_record(
                        root,
                        live,
                        intent.id,
                        intent.root,
                        intent.locator.as_str(),
                    )?;
                }
            }
        }
    }
    publish(&cache_root, &ReplaceJournal::Cleared)
}

/// What recovery found and did about a replacement in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// No intent stands.
    Nothing,
    /// An intent stood, the destination said which side it held, and the
    /// ledger and the intent now say so too.
    Settled(Landing),
    /// An intent stands and the destination holds neither legal landing.
    /// Nothing was changed, and nothing may adopt or replace until it is
    /// looked at.
    Refused,
}

/// Resolve a standing intent against what the destination holds.
///
/// To be run after `RECLAIM.JNL` and `INSTALL.JNL` have settled and not
/// before: the filesystem transaction decides what the card holds, and this
/// one only records what that means. The destination is hashed whole, since
/// which side landed is a question about its bytes and a persisted digest
/// is evidence rather than an answer. It is looked for under the spelling
/// the install used and then under the predecessor's own, which a directory
/// can hold only one of.
pub fn recover<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Recovery, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(Recovery::Nothing),
        Err(_) => return Err(LedgerFault::Device),
    };
    let Some(intent) = standing(&cache_root)? else {
        return Ok(Recovery::Nothing);
    };
    let mut destination = digest_at(root, intent.root, intent.locator.as_str())?;
    if destination.is_none() {
        if let Some(spelled) = &intent.predecessor_locator {
            if spelled.as_str() != intent.locator.as_str() {
                destination = digest_at(root, intent.root, spelled.as_str())?;
            }
        }
    }
    match landing(&intent.predecessor, &intent.new, destination.as_ref()) {
        Some(landed) => {
            settle(root, landed)?;
            Ok(Recovery::Settled(landed))
        }
        None => Ok(Recovery::Refused),
    }
}

/// The digest of the file at a place, read whole now, or `None` for no file.
pub(crate) fn digest_at<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    at: BookRoot,
    locator: &str,
) -> Result<Option<SourceDigest>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let path = LibraryPath::parse(locator).map_err(|_| LedgerFault::Record)?;
    let found = crate::library::with_book_at(root, at, &path, |dir, alias| {
        let mut name = String::<12>::new();
        if write!(name, "{}", alias).is_err() {
            return Err(crate::install::InstallError::Card);
        }
        crate::digest_of_file(dir, name.as_str())
    })
    .map_err(|_| LedgerFault::Device)?;
    match found {
        None => Ok(None),
        Some(Ok(digest)) => Ok(digest),
        Some(Err(_)) => Err(LedgerFault::Device),
    }
}

/// Whether a file sits at a place, spelled exactly so.
fn holds_a_file<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    at: BookRoot,
    locator: &str,
) -> Result<bool, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let path = LibraryPath::parse(locator).map_err(|_| LedgerFault::Record)?;
    crate::library::with_book_at(root, at, &path, |_, _| ())
        .map(|found| found.is_some())
        .map_err(|_| LedgerFault::Device)
}

fn verdict(bytes: &[u8; REPLACE_JOURNAL_BYTES]) -> SlotVerdict {
    match classify_replace_journal(bytes) {
        ReplaceJournalReading::Blank => SlotVerdict::Blank,
        ReplaceJournalReading::Entry { sequence, .. } => SlotVerdict::Entry(sequence),
        ReplaceJournalReading::UnknownVersion(_) => SlotVerdict::UnknownVersion,
        ReplaceJournalReading::Damaged => SlotVerdict::Damaged,
    }
}

/// The intent in flight, read from the newest entry, or `None` when the
/// newest entry is a clear or there is no journal.
fn standing<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<Standing>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some((bytes, _, _)) =
        ledger::newest_slot::<_, _, MD, MF, MV, REPLACE_JOURNAL_BYTES, REPLACE_JOURNAL_SLOT_BYTES>(
            cache_root,
            REPLACE_JOURNAL,
            verdict,
        )?
    else {
        return Ok(None);
    };
    match classify_replace_journal(&bytes) {
        ReplaceJournalReading::Entry {
            entry: ReplaceJournal::Standing(intent),
            ..
        } => Ok(Some(Standing::from_intent(&intent)?)),
        ReplaceJournalReading::Entry {
            entry: ReplaceJournal::Cleared,
            ..
        } => Ok(None),
        // `newest_slot` hands back only a slot it classified as an entry.
        _ => Err(LedgerFault::Damaged),
    }
}

fn publish<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    entry: &ReplaceJournal<'_>,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let current =
        ledger::newest_slot::<_, _, MD, MF, MV, REPLACE_JOURNAL_BYTES, REPLACE_JOURNAL_SLOT_BYTES>(
            cache_root,
            REPLACE_JOURNAL,
            verdict,
        )?
        .map(|(_, slot, sequence)| (slot, sequence));
    ledger::publish_slot::<_, _, MD, MF, MV, REPLACE_JOURNAL_BYTES, REPLACE_JOURNAL_SLOT_BYTES>(
        cache_root,
        REPLACE_JOURNAL,
        current,
        |sequence, out| encode_replace_journal(entry, sequence, out).ok_or(LedgerFault::Record),
    )
}
