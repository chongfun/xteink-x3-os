//! The library ledger: the durable record of which [`BookId`] each physical
//! copy on the card carries.
//!
//! `CATALOG.BIN` is rebuilt from the card whenever it stops matching it, and
//! that is fine for everything in it except an id, which the card cannot
//! give back. So ids live here, in `/READER/LEDGERA.BIN` and `LEDGERB.BIN`,
//! and the catalog only caches them.
//!
//! One generation is one whole file: a header, then one fixed-size record
//! per adopted copy, each carrying its own checksum. A rewrite goes to the
//! side that is not live. Records go down first under a placeholder header
//! and the file is closed, so its directory entry carries the final length;
//! then the file is opened again and the real header written over the
//! placeholder. That header is the last block of the generation to land, so
//! a generation with a header is a generation with all of its records.
//!
//! Which side is live is not something the two files can say on their own.
//! A side holding the placeholder, or nothing, looks the same whether a
//! rewrite of it was interrupted, which costs nothing, or it was the live
//! generation and lost its header, which is the loss of every id it added.
//! So a third file, `/READER/LEDGER.JNL`, keeps the fact that tells them
//! apart. While a rewrite lays the target's records down, the journal still
//! names the side that stands, so whatever the target held before is not
//! consulted, damaged or not. Once the records are down under the
//! placeholder it records which side is being written and what stood on the
//! other; after the new header has landed and read back it records which
//! side is live and what its header says.
//!
//! The journal has to survive its own interrupted write, so it is two
//! sector-sized slots written alternately, each entry with a sequence
//! number, the way `RECLAIM.JNL` is kept. A write torn by a power cut
//! damages the slot being written, and the entry before it still reads.
//! Falling back one entry is safe by construction: a generation's ids reach
//! a committed catalog only after the journal has named that generation
//! live, so the entry before names either the same live side or a rewrite
//! whose outcome the target's own header decides. A torn write of that
//! header, or of the placeholder before it, is read the same way: under a
//! journal that says a side is being written, a target that does not read
//! back as the committed, whole generation expected is a target whose commit
//! did not land, and the side that stood is live.
//!
//! A reader believes a side only when the journal accounts for it: the side
//! the journal names as live, with the header it recorded and every record
//! intact; or, during a rewrite, the target if it committed whole and
//! otherwise the side that stood, exactly as recorded. Anything else, a live
//! side that is not as the journal says, a journal with no entry that reads,
//! a header or journal of a version this build does not read, or ledger
//! files with no journal beside them, refuses. Nothing here repairs a ledger
//! by guessing which side to trust; the intact records stay where they are
//! for something explicit to salvage.
//!
//! Rewriting the whole file rather than appending is deliberate. An append
//! that straddles a cluster boundary can leave the FAT chain and the
//! directory entry disagreeing about where the file ends, and the recovery
//! model in this crate rests on not having to interpret that state. A
//! rewrite happens when a scan changes what the ledger says: copies to
//! adopt, copies gone missing, or copies come back. On a card that has not
//! changed, the scan does not run at all.
//!
//! Adoption is by place and size, and costs no reading: a fresh catalog row
//! whose root, locator and size a live record names is that record's copy.
//! That is the whole of it for a card that has not been reorganised, which
//! is the ordinary case, and it is why a scan of a thousand books opens no
//! book at all.
//!
//! What the join cannot match it does not give up on. A record that named
//! no row and a row no record named can be one copy in a new place, so the
//! search between those two sets reads the files whose length a missing
//! copy has and compares them against what that copy's bytes were: a
//! unique match on both sides moves the record to the row's place, keeping
//! its id, and anything less than unique leaves both alone and adopts the
//! row in its own right. A copy nothing recorded the bytes of cannot be
//! matched at all, since a name and a length are not a book. Records that
//! match nothing age for a bounded number of scans and are then let go.
//!
//! The other place a copy's bytes or spelling change under its id is a
//! managed replacement, which [`crate::replace`] carries across the install
//! and publishes here through [`publish_record`] and [`relocate_record`].

use core::cell::Cell;

use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::{
    cache_key_from, decode_cache_claimant, read_cache_claim, source_hash_at, CacheClaimReading,
    CACHE_CLAIM_FILE, CACHE_CLAIM_MAX_BYTES, CACHE_ROOT_DIR, CACHE_V2_DIR,
};
use proto::catalog::{
    catalog_record_at, catalog_record_book_id, catalog_record_identity, CATALOG_HEADER_BYTES,
    CATALOG_RECORD_BYTES, CATALOG_RECORD_ID_OFFSET,
};
use proto::durable::generation_is_newer;
use proto::identity::{
    carry_missing, classify_ledger_header, classify_ledger_journal, decode_ledger_record,
    encode_ledger_header, encode_ledger_journal, encode_ledger_placeholder_header,
    encode_ledger_record, ledger_file_len, rows_with_hash, sort_row_keys, stage_row_key, BookId,
    LedgerHeader, LedgerHeaderReading, LedgerJournal, LedgerJournalReading, LedgerRecord,
    BOOK_ID_BYTES, LEDGER_HEADER_BYTES, LEDGER_JOURNAL_BYTES, LEDGER_JOURNAL_SLOTS,
    LEDGER_JOURNAL_SLOT_BYTES, LEDGER_RECORD_BYTES, ROW_KEY_BYTES,
};
use proto::library_path::{BookRoot, MAX_PATH_BYTES};
use proto::source::{encode_cached_record, parse_record, CachedSourceDigest, SOURCE_RECORD_BYTES};

/// The two generations, under the cache root.
pub const LEDGER_FILES: [&str; 2] = ["LEDGERA.BIN", "LEDGERB.BIN"];
/// The journal that says which of them is live, beside them.
pub const LEDGER_JOURNAL: &str = "LEDGER.JNL";

/// The most records one generation can hold, because the header counts them
/// in two bytes. The same ceiling as the catalog, and for the same reason.
pub const LEDGER_MAX_RECORDS: usize = u16::MAX as usize;

/// Why the ledger could not do what was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerFault {
    /// The card refused a read, a write, or an open for a reason other than
    /// the file being absent. Not evidence about the ledger: the caller
    /// leaves things as they are and the next mount tries again.
    Device,
    /// The ledger is not as its journal says, or something in it does not
    /// read: the live side is missing, empty, or holds a header other than
    /// the one recorded; the live generation's records or length do not
    /// match its header; a header or a journal is bytes this build did not
    /// write; or there are ledger files with no journal to account for
    /// them. That is durable identity state damaged after it landed, and
    /// taking whatever else is on the card in its place would re-mint every
    /// id it held. The ledger is left exactly as it is and the operation is
    /// refused; a catalog already committed keeps serving, and only rebuilds
    /// stop, until the intact records are salvaged by something explicit.
    Damaged,
    /// A committed header, or a journal entry, of a format version this
    /// build does not read, written by another build. Refused for the same
    /// reason as damage: the ids it holds cannot come back from the card.
    Unreadable,
    /// A managed replacement is in flight. Until it settles nothing may
    /// begin another, and nothing may adopt at its locator by guessing.
    Busy,
    /// More copies than a generation can hold once the live and newly
    /// adopted ones are counted. Missing records yield first, so this is a
    /// library past the catalog's own ceiling.
    Full,
    /// A row that cannot be adopted: a root byte this build does not know,
    /// or a locator wider than a record. Neither is reachable from a
    /// catalog this build wrote.
    Record,
    /// The caller's scratch cannot hold the join's working set.
    Scratch,
}

/// One validated generation: which side it is on, and what its header said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ledger {
    side: usize,
    pub generation: u32,
    pub count: u16,
}

impl Ledger {
    /// Which file this generation is in, for a caller that reports it.
    pub fn file_name(&self) -> &'static str {
        LEDGER_FILES[self.side]
    }
}

/// What one side's file holds, as far as its first sixteen bytes say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideState {
    /// No file.
    Absent,
    /// An empty file, or one under the placeholder: a generation whose
    /// header has not landed, if the journal says one was being written
    /// here, and a lost one otherwise.
    Uncommitted,
    Committed(LedgerHeader),
    /// Bytes that are neither the placeholder nor a header: a torn header
    /// write, if the journal says one was under way here, and damage
    /// otherwise.
    Damaged,
    /// A header of a version this build does not read.
    Unreadable,
}

/// The live generation, or `None` for a card with no ledger yet.
///
/// The journal decides. When it names a live side, that side must hold the
/// header it recorded and read back whole. When it names a side being
/// rewritten, that side is live if its header landed with the generation
/// after the one that stood and it reads back whole; anything else there,
/// the placeholder, a torn header, a header of another generation, is a
/// commit that did not land, and the side that stood is live if it still
/// holds exactly the header recorded for it. A card with no journal has no
/// ledger, and ledger files beside no journal are [`LedgerFault::Damaged`].
/// Sides the journal does not point at are not read at all, so damage to a
/// generation that is no longer live costs nothing until the next rewrite
/// goes over it.
pub fn open<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<Ledger>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    let (side, header) = match journal_slots(&cache_root)? {
        None => {
            if side_state(&cache_root, 0)? == SideState::Absent
                && side_state(&cache_root, 1)? == SideState::Absent
            {
                return Ok(None);
            }
            return Err(LedgerFault::Damaged);
        }
        Some((LedgerJournal::Committed { side, header }, _, _)) => {
            let side = usize::from(side);
            match side_state(&cache_root, side)? {
                SideState::Committed(found) if found == header => {
                    if !reads_back_whole(&cache_root, side, header)? {
                        return Err(LedgerFault::Damaged);
                    }
                    (side, header)
                }
                SideState::Unreadable => return Err(LedgerFault::Unreadable),
                _ => return Err(LedgerFault::Damaged),
            }
        }
        Some((LedgerJournal::Rewriting { target, standing }, _, _)) => {
            let target = usize::from(target);
            let other = 1 - target;
            let expected = standing.map_or(1, |stood| stood.generation.wrapping_add(1));
            // The rewrite got as far as its commit and the power went before
            // the journal could say so. The generation number is what says
            // this is that commit, and the whole check is what says it
            // landed rather than tore.
            if let SideState::Committed(found) = side_state(&cache_root, target)? {
                if found.generation == expected && reads_back_whole(&cache_root, target, found)? {
                    return Ok(Some(Ledger {
                        side: target,
                        generation: found.generation,
                        count: found.count,
                    }));
                }
            }
            // The commit did not land. What stood must still stand, exactly
            // as recorded, or nothing does.
            match (standing, side_state(&cache_root, other)?) {
                (None, SideState::Absent) => return Ok(None),
                (Some(stood), SideState::Committed(found)) if found == stood => {
                    if !reads_back_whole(&cache_root, other, stood)? {
                        return Err(LedgerFault::Damaged);
                    }
                    (other, stood)
                }
                (Some(_), SideState::Unreadable) => return Err(LedgerFault::Unreadable),
                _ => return Err(LedgerFault::Damaged),
            }
        }
    };
    Ok(Some(Ledger {
        side,
        generation: header.generation,
        count: header.count,
    }))
}

/// The journal's newest entry that reads, or `None` for a card with no
/// journal. For a caller that reports on the ledger; [`open`] is what
/// decides from it.
pub fn read_journal<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<LedgerJournal>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    Ok(journal_slots(&cache_root)?.map(|(entry, _, _)| entry))
}

/// Every record of `ledger`, in order, to `visit`. A record that no longer
/// decodes is the card misbehaving between the check in [`open`] and now,
/// and is reported as such rather than skipped.
pub fn for_each_record<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    ledger: &Ledger,
    visit: &mut impl FnMut(u16, &LedgerRecord<'_>) -> Result<(), LedgerFault>,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root
        .open_dir(CACHE_ROOT_DIR)
        .map_err(|_| LedgerFault::Device)?;
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[ledger.side], Mode::ReadOnly)
        .map_err(|_| LedgerFault::Device)?;
    file.seek_from_start(LEDGER_HEADER_BYTES as u32)
        .map_err(|_| LedgerFault::Device)?;
    let mut bytes = [0u8; LEDGER_RECORD_BYTES];
    for index in 0..ledger.count {
        if !read_exact(&file, &mut bytes)? {
            return Err(LedgerFault::Device);
        }
        let record = decode_ledger_record(&bytes).ok_or(LedgerFault::Device)?;
        visit(index, &record)?;
    }
    Ok(())
}

/// The live record naming a place, if one does: its id and what it says
/// about the copy's bytes. The first in ledger order, as the scan's join
/// takes it.
pub fn find_record<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    ledger: &Ledger,
    at: BookRoot,
    locator: &str,
    byte_size: u32,
) -> Result<Option<(BookId, Option<CachedSourceDigest>)>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut found: Option<(BookId, Option<CachedSourceDigest>)> = None;
    for_each_record(root, ledger, &mut |_, record| {
        if found.is_none()
            && record.root == at
            && record.locator == locator
            && record.byte_size == byte_size
        {
            found = Some((record.id, record.source));
        }
        Ok(())
    })?;
    Ok(found)
}

/// The copy a [`BookId`] names, as the ledger has it.
///
/// Owned rather than borrowed like [`LedgerRecord`]: a caller resolving an
/// id is between reads, and a borrowed record points into the buffer the
/// next read fills.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryCopy {
    /// Where the copy sits: the root its locator is relative to, and the
    /// locator spelled exactly as the card spells it.
    ///
    /// `None` is a record with no place to give. Not the same thing as a
    /// place that is empty, which a missing copy has and which the copy can
    /// come back to: it is a record whose place another id holds, so what
    /// it says about the card describes another copy's file. Handing that
    /// back is how state belonging to one copy would be resolved against
    /// another, which is worse than losing the copy. The record stays in
    /// the ledger to be matched by its bytes or aged out; what it says
    /// about the card is what stops being evidence.
    pub place: Option<(BookRoot, heapless::String<MAX_PATH_BYTES>)>,
    pub byte_size: u32,
    /// Consecutive scans that have not found it. Zero is a copy the last
    /// scan saw; anything else is a copy whose card may simply have been
    /// out, which is why a record with misses still answers here.
    pub misses: u8,
    pub source: Option<CachedSourceDigest>,
}

impl LibraryCopy {
    /// Where the copy sits, spelled exactly as the card spells it, when the
    /// ledger has a place to give for it.
    pub fn locator(&self) -> Option<&str> {
        self.place.as_ref().map(|(_, locator)| locator.as_str())
    }
}

/// Where the copy `id` names is now, or `None` when no record carries that
/// id.
///
/// The reverse of [`find_record`], and the direction per-copy user state is
/// addressed by: a reading position hangs from an id, and resuming it means
/// asking which file that id is, wherever it has been moved or renamed to
/// since. A locator answers the other question, which file this is, and the
/// two meet in the catalog row that caches both.
///
/// A place is handed back only if it is this record's to give. A place
/// belongs to the record the last scan matched to it, which is the record
/// with no misses: the scan matches a row by root, locator and size, gives
/// the row to one record, and ages every record it did not match. So a
/// record with misses whose place another record holds with none is a
/// record describing a file that answers to another id, and it is told
/// nothing rather than told that.
///
/// It is a place with different bytes at it that gets there, which an
/// ordinary card edit reaches: a book replaced on a computer by one of
/// another size is a row the old record no longer matches, so the row is
/// minted an id of its own and the old record is carried as missing at the
/// name the new copy now holds. Two records naming one place with one size,
/// which this crate's writers do not produce, resolves the same way, the
/// scan having matched exactly one of them.
///
/// A record with no misses is the owner of its place, so the check runs
/// only for the others, and resolving a live copy still reads the ledger
/// once.
pub fn find_by_id<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    ledger: &Ledger,
    id: BookId,
) -> Result<Option<LibraryCopy>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut found: Option<(BookRoot, heapless::String<MAX_PATH_BYTES>, u32, u8)> = None;
    let mut source = None;
    for_each_record(root, ledger, &mut |_, record| {
        if found.is_some() || record.id != id {
            return Ok(());
        }
        let mut locator = heapless::String::new();
        locator
            .push_str(record.locator)
            .map_err(|_| LedgerFault::Record)?;
        source = record.source;
        found = Some((record.root, locator, record.byte_size, record.misses));
        Ok(())
    })?;
    let Some((at, locator, byte_size, misses)) = found else {
        return Ok(None);
    };
    let mut held_by_another = false;
    if misses > 0 {
        for_each_record(root, ledger, &mut |_, record| {
            held_by_another |=
                record.misses == 0 && record.root == at && record.locator == locator.as_str();
            Ok(())
        })?;
    }
    Ok(Some(LibraryCopy {
        place: (!held_by_another).then_some((at, locator)),
        byte_size,
        misses,
        source,
    }))
}

/// What a carried record is written with when it keeps its id, root and
/// locator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kept {
    pub byte_size: u32,
    pub misses: u8,
    pub source: Option<CachedSourceDigest>,
}

impl Kept {
    /// Carry a record exactly as it is.
    pub fn of(record: &LedgerRecord<'_>) -> Self {
        Self {
            byte_size: record.byte_size,
            misses: record.misses,
            source: record.source,
        }
    }
}

/// What becomes of one record of the previous generation in the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carry<'x> {
    /// The same copy at the same place, as `Kept` says.
    Keep(Kept),
    /// This record in full, in the previous one's position. For a copy
    /// whose place or bytes changed under its id.
    Replace(LedgerRecord<'x>),
    /// Left behind.
    Drop,
}

/// Appends records to the generation being written. See [`write_generation`].
pub struct LedgerWriter<'f, 'd, D, T, const MD: usize, const MF: usize, const MV: usize>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'f File<'d, D, T, MD, MF, MV>,
    count: u16,
}

impl<D, T, const MD: usize, const MF: usize, const MV: usize> LedgerWriter<'_, '_, D, T, MD, MF, MV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    pub fn append(&mut self, record: &LedgerRecord<'_>) -> Result<(), LedgerFault> {
        let count = self.count.checked_add(1).ok_or(LedgerFault::Full)?;
        let mut bytes = [0u8; LEDGER_RECORD_BYTES];
        encode_ledger_record(record, &mut bytes).ok_or(LedgerFault::Record)?;
        self.file.write(&bytes).map_err(|_| LedgerFault::Device)?;
        self.count = count;
        Ok(())
    }

    /// Records in the generation so far, carried and appended.
    pub fn count(&self) -> u16 {
        self.count
    }
}

/// Write the next generation: each record of `previous` as `carry` says,
/// then whatever `fill` appends, and then the header that commits it, with
/// the journal saying throughout what is going on.
///
/// The target is the side `previous` is not on, so `previous` stands
/// untouched until the new header has landed and read back. `fill` is
/// handed the writer once the carried records are down, and may read and
/// write other files meanwhile; the one it must not touch is the previous
/// side, which is already closed by then.
pub fn write_generation<'x, D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    previous: Option<&Ledger>,
    carry: &mut impl FnMut(u16, &LedgerRecord<'_>) -> Carry<'x>,
    fill: impl FnOnce(&mut LedgerWriter<'_, '_, D, T, MD, MF, MV>) -> Result<(), LedgerFault>,
) -> Result<Ledger, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_cache_root(root)?;
    let (target, generation) = match previous {
        Some(live) => (1 - live.side, live.generation.wrapping_add(1)),
        None => (0, 1),
    };
    let mut header = [0u8; LEDGER_HEADER_BYTES];
    let standing = previous.map(|live| LedgerHeader {
        generation: live.generation,
        count: live.count,
    });
    let rewriting = LedgerJournal::Rewriting {
        target: target as u8,
        standing,
    };

    // With nothing standing there is no journal yet, and a ledger file with
    // no journal beside it reads as damage, so the first generation is
    // announced before its file exists.
    if standing.is_none() {
        write_journal(&cache_root, rewriting)?;
    }

    // Records first, under a header that decodes as nothing, closed so the
    // directory entry holds the final length before anything says the
    // generation exists. The journal still names the side that stands, so a
    // cut anywhere in here leaves the target unread: whatever it held
    // before, damaged or not, is neither consulted nor a reason to refuse.
    let count = {
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[target], Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| LedgerFault::Device)?;
        encode_ledger_placeholder_header(&mut header);
        file.write(&header).map_err(|_| LedgerFault::Device)?;
        let mut carried: u16 = 0;
        if let Some(live) = previous {
            let source = cache_root
                .open_file_in_dir(LEDGER_FILES[live.side], Mode::ReadOnly)
                .map_err(|_| LedgerFault::Device)?;
            source
                .seek_from_start(LEDGER_HEADER_BYTES as u32)
                .map_err(|_| LedgerFault::Device)?;
            let mut bytes = [0u8; LEDGER_RECORD_BYTES];
            for index in 0..live.count {
                // Carried forward only as read back intact. The side was
                // checked when it was opened, so anything else here is the
                // card, and a copy of it would be a copy of the damage.
                if !read_exact(&source, &mut bytes)? {
                    return Err(LedgerFault::Device);
                }
                let record = decode_ledger_record(&bytes).ok_or(LedgerFault::Device)?;
                let mut out = [0u8; LEDGER_RECORD_BYTES];
                match carry(index, &record) {
                    Carry::Keep(kept) => {
                        let kept = LedgerRecord {
                            byte_size: kept.byte_size,
                            misses: kept.misses,
                            source: kept.source,
                            ..record
                        };
                        encode_ledger_record(&kept, &mut out).ok_or(LedgerFault::Record)?;
                    }
                    Carry::Replace(replacement) => {
                        encode_ledger_record(&replacement, &mut out).ok_or(LedgerFault::Record)?;
                    }
                    Carry::Drop => continue,
                }
                file.write(&out).map_err(|_| LedgerFault::Device)?;
                carried = carried.checked_add(1).ok_or(LedgerFault::Full)?;
            }
        }
        let mut writer = LedgerWriter {
            file: &file,
            count: carried,
        };
        fill(&mut writer)?;
        let count = writer.count;
        file.close().map_err(|_| LedgerFault::Device)?;
        count
    };

    // Now the target is known to hold the placeholder over this
    // generation's records and nothing else. Say it is being written: from
    // here until the header lands the side that stood is the one to
    // believe, and once the header has landed whole the target is.
    if standing.is_some() {
        write_journal(&cache_root, rewriting)?;
    }

    // Then the commit: one header, over the placeholder, in a file whose
    // length is already final.
    let committed = LedgerHeader { generation, count };
    {
        let file = cache_root
            .open_file_in_dir(LEDGER_FILES[target], Mode::ReadWriteAppend)
            .map_err(|_| LedgerFault::Device)?;
        file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
        encode_ledger_header(committed, &mut header);
        file.write(&header).map_err(|_| LedgerFault::Device)?;
        file.close().map_err(|_| LedgerFault::Device)?;
    }
    match side_state(&cache_root, target)? {
        SideState::Committed(found) if found == committed => {}
        _ => return Err(LedgerFault::Device),
    }
    if !reads_back_whole(&cache_root, target, committed)? {
        return Err(LedgerFault::Device);
    }

    // And say so. Until this lands the journal still explains the target as
    // being written, and a reader finds it committed whole with the
    // generation it expects, which is the same answer.
    write_journal(
        &cache_root,
        LedgerJournal::Committed {
            side: target as u8,
            header: committed,
        },
    )?;
    Ok(Ledger {
        side: target,
        generation,
        count,
    })
}

/// Publish one record: the record with its id, if the ledger holds one, is
/// rewritten to say what `record` says, and otherwise `record` is appended.
/// The rest of the generation is carried as it is.
///
/// This is how a managed replacement lands in the ledger: the copy keeps
/// its id and takes the place, size and digest of the bytes that replaced
/// it. It is a whole generation rewrite, which is what every change to the
/// ledger is, and is committed before this returns.
///
/// The place published belongs to this record afterwards: any other record
/// naming it is dropped, since one file sits at a place and the caller has
/// just proved which copy that is.
///
/// A generation with no room for one more record can make it only by letting
/// `evict` go, a record the caller chose and verified against the card;
/// nothing here decides which copy is disposable. A full ledger with no
/// record to let go of, or one whose `evict` is not there, is
/// [`LedgerFault::Full`], and a caller that can refuse before anything is
/// journalled does.
pub fn publish_record<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    ledger: Option<Ledger>,
    record: &LedgerRecord<'_>,
    evict: Option<BookId>,
) -> Result<Ledger, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut exists = false;
    let mut evict_exists = false;
    if let Some(live) = &ledger {
        for_each_record(root, live, &mut |_, entry| {
            exists |= entry.id == record.id;
            evict_exists |= evict == Some(entry.id);
            Ok(())
        })?;
    }
    let evict = match &ledger {
        Some(live) if !exists && live.count as usize >= LEDGER_MAX_RECORDS => {
            if !evict_exists {
                return Err(LedgerFault::Full);
            }
            evict
        }
        _ => None,
    };
    let replaced = Cell::new(false);
    let published = LedgerRecord {
        misses: 0,
        ..*record
    };
    write_generation(
        root,
        ledger.as_ref(),
        &mut |_, entry| {
            if evict == Some(entry.id) {
                Carry::Drop
            } else if entry.id == record.id {
                replaced.set(true);
                Carry::Replace(published)
            } else if entry.root == record.root && entry.locator == record.locator {
                // Another id claiming the place this copy has just been
                // proved to hold. One file sits at a place, so the claim is
                // contradicted by the proof: a book deleted on a computer
                // and uploaded again lands under a fresh id at the name its
                // predecessor's record still names, and carrying both would
                // leave two ids answering for one file for ever, with the
                // scan's join picking between them by ledger order rather
                // than by evidence.
                Carry::Drop
            } else {
                Carry::Keep(Kept::of(entry))
            }
        },
        |writer| {
            if !replaced.get() {
                writer.append(&published)?;
            }
            Ok(())
        },
    )
}

/// Move the record with `id` to another place, keeping its size, digest and
/// id. For a copy that a managed transaction respelled or moved. A ledger
/// with no such record is left as it is, and any other record naming the
/// place moved to is dropped, as in [`publish_record`].
pub fn relocate_record<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    ledger: Ledger,
    id: BookId,
    at: BookRoot,
    locator: &str,
) -> Result<Ledger, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut exists = false;
    for_each_record(root, &ledger, &mut |_, entry| {
        exists |= entry.id == id;
        Ok(())
    })?;
    if !exists {
        return Ok(ledger);
    }
    write_generation(
        root,
        Some(&ledger),
        &mut |_, entry| {
            if entry.id == id {
                Carry::Replace(LedgerRecord {
                    root: at,
                    locator,
                    misses: 0,
                    ..*entry
                })
            } else if entry.root == at && entry.locator == locator {
                // The place this copy is moving to, claimed by another id.
                // One file sits at a place, and the caller has just seen
                // which copy that is; see [`publish_record`].
                Carry::Drop
            } else {
                Carry::Keep(Kept::of(entry))
            }
        },
        |_| Ok(()),
    )
}

/// What [`assign_book_ids`] did, for the caller that reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Assignment {
    /// Rows a live record named, which kept that record's id.
    pub matched: u16,
    /// Rows no record named, which were adopted under a fresh id.
    pub minted: u16,
    /// Live records that named a row another record had already claimed.
    /// The first in ledger order wins, deterministically. This writer
    /// appends one record per unnamed row, so a duplicate is a ledger
    /// written by something else.
    pub duplicates: u16,
    /// Records that named no row and were carried into the new generation
    /// as missing copies, one scan older.
    pub missing: u16,
    /// Records that named no row and were left behind: missing for longer
    /// than the ledger retains, or not fitting beside the live library.
    pub retired: u16,
    /// Copies found again somewhere else: a record that named no row and a
    /// row no record named, proved the same bytes, so the record moved to
    /// the row's place with its id rather than the row being adopted afresh.
    pub repaired: u16,
    /// Files read whole to confirm a move. What the search costs, and the
    /// number a fingerprint would be there to bring down.
    pub hashed: u16,
    /// Moves that could have been more than one thing: two missing copies of
    /// the same bytes, or one missing copy and two new files holding them.
    /// Left alone, both sides, since a wrong join of one reader's place to
    /// another book costs more than a copy adopted afresh.
    pub ambiguous: u16,
    /// Copies left alone because the card would not give up a file they
    /// could have been. One unread file makes every copy of its length
    /// undecidable, since it could hold any of their bytes, and a scan
    /// decides a length only when it has read every file of that length.
    /// Those copies stay missing on the ordinary retention and the files
    /// are adopted in their own right, as for any other reason the search
    /// cannot tell two things apart.
    pub unreadable: u16,
}

// ---------------------------------------------------------------------------
// Moves: a copy that went missing and a file that appeared may be one book
// ---------------------------------------------------------------------------

/// One missing copy the move search carries while it reads the rows: which
/// record it is, what it was, the id it keeps if it is found again, the
/// digest that would prove it, and what the search has turned up so far.
const MOVE_INDEX: usize = 0;
const MOVE_SIZE: usize = 2;
const MOVE_ID: usize = 6;
const MOVE_DIGEST: usize = MOVE_ID + BOOK_ID_BYTES;
const MOVE_ROW: usize = MOVE_DIGEST + SOURCE_RECORD_BYTES;
const MOVE_MATCHES: usize = MOVE_ROW + 2;
const MOVE_STATE: usize = MOVE_MATCHES + 1;
const MOVE_ROOT: usize = MOVE_STATE + 1;
const MOVE_LOCATOR_LEN: usize = MOVE_ROOT + 1;
const MOVE_LOCATOR: usize = MOVE_LOCATOR_LEN + 2;
const MOVE_ENTRY_BYTES: usize = MOVE_LOCATOR + MAX_PATH_BYTES;

/// Missing copies one scan carries into the search. A reorganisation larger
/// than this repairs what fits and adopts the rest afresh, which is the
/// same answer the search gives anything it cannot prove.
const MOVES_CONSIDERED: usize = 64;
/// This copy is not one the search may repair: another missing copy holds
/// the same bytes, or two files do, or a file of its length went unread and
/// could have held them. One outcome for all three, because they are one
/// answer: the bytes do not say which copy this file is. The copies stay
/// missing on the ordinary retention and the files are adopted in their own
/// right.
///
/// A scan decides a length or leaves it alone. There is no reading budget
/// to run out of: every unclaimed file whose length a missing copy has is
/// read, so one match means one match. Bounding that reading instead would
/// mean deciding on part of the evidence, or carrying the
/// question to the next scan, and a question carried across scans wants a
/// journal of its own rather than a state spread through the catalog, the
/// ledger and the cache.
const MOVE_AMBIGUOUS: u8 = 1;

/// A copy the scan found again, told to a caller that keeps something
/// filed under where it used to be.
///
/// Reported before the ledger is written, so a caller that acts on it and a
/// power cut that follows leave a card whose next scan reports the same
/// move again: the record is still missing, the row is still unadopted, and
/// doing it twice costs what doing it once did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoundAgain<'a> {
    /// The id the copy keeps, which is the point of finding it again.
    pub id: BookId,
    /// Where it was, as the ledger had it.
    pub was: (BookRoot, &'a str, u32),
    /// Where it is, as the row that holds it has it.
    pub now: (BookRoot, &'a str, u32),
}

/// What the directory a copy keeps its reading place in says its bytes
/// were, when it says anything.
///
/// A book a scan adopted has no digest in the ledger: nothing read it, and
/// reading every book to adopt it would cost a card's worth of hashing for
/// a move that may never happen. Opening one records what it held beside
/// the place it was read from, and that directory is named for the place
/// this record still names. So a copy that has been read can be proved
/// somewhere else, and one that has not cannot, which is the same rule the
/// position it would carry lives by.
///
/// Anything unreadable, foreign, or silent is no evidence rather than an
/// error: the copy simply is not one this scan can match.
///
/// What this makes a copy is the bytes seen at its own place while that
/// place looked unchanged, which is a deliberate rule and not quite the
/// same as the bytes it was adopted with. A computer can put a different
/// book of the same length at that name between two scans, which the
/// join's cheap filter cannot see and no later reading can undo, since
/// nothing on the card ever said what the first book's bytes were. The
/// copy then takes what was read there, and moves with it. The alternative
/// is reading every book as the scan adopts it, hours on a full card for a
/// move that may never happen, and the caches already resume the old
/// book's place at a same-sized replacement, so the rule makes that
/// durable rather than inventing it.
fn claim_digest<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    at: BookRoot,
    locator: &str,
    byte_size: u32,
) -> Option<CachedSourceDigest>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let key = cache_key_from(source_hash_at(at, locator, byte_size));
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = cache_root.open_dir(CACHE_V2_DIR).ok()?;
    let book = cache.open_dir(key.as_str()).ok()?;
    let file = book
        .open_file_in_dir(CACHE_CLAIM_FILE, Mode::ReadOnly)
        .ok()?;
    let mut stored = [0u8; CACHE_CLAIM_MAX_BYTES];
    let read = file.read(&mut stored).ok()?;
    match read_cache_claim(&stored[..read], at, locator) {
        // Its own directory, released or not: a sweep releases a claim
        // rather than unsaying it, and what the book's bytes were when it
        // was read is still what they were.
        CacheClaimReading::MineActive | CacheClaimReading::MineReleased => {
            decode_cache_claimant(&stored[..read])?.evidence.digest
        }
        _ => None,
    }
}

fn move_root(byte: u8) -> BookRoot {
    if byte == 1 {
        BookRoot::Library
    } else {
        BookRoot::CardRoot
    }
}

fn move_root_byte(root: BookRoot) -> u8 {
    u8::from(matches!(root, BookRoot::Library))
}

fn move_entry(table: &[u8], slot: usize) -> &[u8] {
    &table[slot * MOVE_ENTRY_BYTES..][..MOVE_ENTRY_BYTES]
}

fn move_u16(entry: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([entry[at], entry[at + 1]])
}

/// Whether files of `byte_size` still say anything about this copy: one
/// that size the bytes have not already told apart from something else.
fn move_awaits(table: &[u8], slot: usize, byte_size: u32) -> bool {
    let entry = move_entry(table, slot);
    entry[MOVE_STATE] != MOVE_AMBIGUOUS
        && u32::from_le_bytes(entry[MOVE_SIZE..MOVE_ID].try_into().expect("four bytes"))
            == byte_size
}

/// Leave every copy of `byte_size` alone: a file that length went unread,
/// so what the files of that length hold is not known well enough to say
/// which copy any of them is.
fn move_undecidable(table: &mut [u8], slots: usize, byte_size: u32, unreadable: &mut u16) {
    for slot in 0..slots {
        if !move_awaits(table, slot, byte_size) {
            continue;
        }
        table[slot * MOVE_ENTRY_BYTES + MOVE_STATE] = MOVE_AMBIGUOUS;
        *unreadable = unreadable.saturating_add(1);
    }
}

/// The slot a record index belongs to, when the search means to move it.
fn move_slot_of_record(table: &[u8], slots: usize, index: u16) -> Option<usize> {
    (0..slots).find(|slot| {
        let entry = move_entry(table, *slot);
        move_u16(entry, MOVE_INDEX) == index && move_settled(entry)
    })
}

/// The slot carrying a record, whether or not the search settled it.
fn move_slot_carrying(table: &[u8], slots: usize, index: u16) -> Option<usize> {
    (0..slots).find(|slot| move_u16(move_entry(table, *slot), MOVE_INDEX) == index)
}

/// Whether a slot names the one file it can be, which is the one thing that
/// leaves this search: the ledger moves an id on it, and so does everything
/// downstream.
fn move_settled(entry: &[u8]) -> bool {
    entry[MOVE_MATCHES] == 1 && entry[MOVE_STATE] == 0
}

/// The slot a row belongs to, when the search means to give it that copy.
fn move_slot_of_row(table: &[u8], slots: usize, row: u16) -> Option<usize> {
    (0..slots).find(|slot| {
        let entry = move_entry(table, *slot);
        move_settled(entry) && move_u16(entry, MOVE_ROW) == row
    })
}

/// Give every row of a freshly written catalog its [`BookId`].
///
/// `catalog` is the open `CATALOG.BIN` with `count` records written and its
/// header still the placeholder; rows carry no id yet. `ledger` is what
/// [`open`] returned for this card, opened by the caller so that a ledger
/// that refuses can refuse before the catalog is touched. Each row a live
/// record names by root, locator and size takes that record's id in place.
/// The rest are minted ids and appended in one new generation, which is
/// committed before this returns, so the ids the catalog carries are
/// durable by the time its own header is. The same generation carries
/// forward the records no row named, aged by one scan and dropped once
/// they pass the retention bound or would not fit beside the live library.
/// That includes a catalog with no rows at all, which ages every record.
/// A scan that changes nothing writes nothing.
///
/// A join rather than a lookup per row, because a per-row lookup in a
/// thousand-book library is a thousand file opens. Row keys are staged in
/// `scratch` a slice at a time, the ledger is read once per slice, and only
/// rows whose 32-bit place hash agrees are compared in full. The head of
/// `scratch` holds one bit per ledger record, set when the record names a
/// row, which is what tells a live record from a missing one when the
/// generation is rewritten. The scratch bounds nothing but the number of
/// slices.
///
/// On a fault nothing is retracted. Ids written into rows are ids that the
/// ledger either committed, in which case they are right, or did not, in
/// which case the caller must not commit the catalog either, and the next
/// scan starts over from the generation that stands.
pub fn assign_book_ids<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    catalog: &File<'_, D, T, MD, MF, MV>,
    count: u16,
    scratch: &mut [u8],
    random: &mut impl FnMut() -> u32,
    ledger: Option<Ledger>,
    found_again: &mut dyn FnMut(&FoundAgain<'_>),
) -> Result<Assignment, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut assigned = Assignment::default();
    let live = ledger.filter(|live| live.count > 0);
    if count == 0 && live.is_none() {
        return Ok(assigned);
    }
    let bitmap_bytes = live.map_or(0, |live| (live.count as usize).div_ceil(8));
    if scratch.len() < bitmap_bytes {
        return Err(LedgerFault::Scratch);
    }
    let (named, keys) = scratch.split_at_mut(bitmap_bytes);
    named.fill(0);
    let per_slice = keys.len() / ROW_KEY_BYTES;
    if count > 0 && per_slice == 0 {
        return Err(LedgerFault::Scratch);
    }
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    // Live records that had been missing, whose count has to go back to
    // zero: a change to the ledger even when nothing else moved.
    let mut returned = 0usize;
    if let Some(live) = live {
        let mut slice_start = 0usize;
        while slice_start < count as usize {
            let slice_end = (slice_start + per_slice).min(count as usize);
            seek_row(catalog, slice_start)?;
            for row in slice_start..slice_end {
                if !read_exact(catalog, &mut record)? {
                    return Err(LedgerFault::Device);
                }
                let (hash, _) = catalog_record_identity(&record);
                stage_row_key(keys, row - slice_start, hash, row as u16);
            }
            let staged = slice_end - slice_start;
            sort_row_keys(keys, staged);
            for_each_record(root, &live, &mut |index, entry| {
                let hash = source_hash_at(entry.root, entry.locator, entry.byte_size);
                let mut names_a_row = false;
                for row in rows_with_hash(keys, staged, hash) {
                    seek_row(catalog, row as usize)?;
                    if !read_exact(catalog, &mut record)? {
                        return Err(LedgerFault::Device);
                    }
                    if catalog_record_at(&record)
                        != Some((entry.root, entry.locator, entry.byte_size))
                    {
                        continue;
                    }
                    if catalog_record_book_id(&record).is_some() {
                        // Another record has already taken this row: two ids
                        // for one place, which this writer cannot produce
                        // and one file cannot answer to. The first in ledger
                        // order keeps the row, deterministically, and this
                        // one is not counted as naming anything, so it ages
                        // out like any other record whose place is not
                        // there and the ledger comes back to one id per
                        // copy on its own.
                        assigned.duplicates = assigned.duplicates.saturating_add(1);
                        continue;
                    }
                    names_a_row = true;
                    write_row_id(catalog, row as usize, entry.id)?;
                    assigned.matched += 1;
                }
                if names_a_row && !bit(named, index) {
                    set_bit(named, index);
                    if entry.misses > 0 {
                        returned += 1;
                    }
                }
                Ok(())
            })?;
            slice_start = slice_end;
        }
    }

    let live_records = named
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum::<usize>();
    let missing_records = live.map_or(0, |live| live.count as usize) - live_records;
    let new_rows = count as usize - assigned.matched as usize;
    if new_rows == 0 && missing_records == 0 && returned == 0 {
        return Ok(assigned);
    }
    // The move search: a record that named no row may be a copy that was
    // moved or renamed on a computer, and a row no record named may be
    // where it went. Only ever between those two sets, so a card whose
    // shelf did not change pays nothing, and a stable file is not read
    // again to prove what the join already matched by place.
    //
    // Size narrows the candidates and the digest decides, which is the one
    // thing that can tell a copy from another book of the same length. A
    // copy with no recorded digest cannot be matched at all: nothing on the
    // card says what its bytes were, and a name and a length are not a
    // book.
    let mut slots = 0usize;
    let capacity = (keys.len() / MOVE_ENTRY_BYTES).min(MOVES_CONSIDERED);
    let table = &mut keys[..capacity * MOVE_ENTRY_BYTES];
    if let Some(live) = live {
        if missing_records > 0 && new_rows > 0 && capacity > 0 {
            for_each_record(root, &live, &mut |index, entry| {
                if bit(named, index) {
                    return Ok(());
                }
                let Some(digest) = entry
                    .source
                    .or_else(|| claim_digest(root, entry.root, entry.locator, entry.byte_size))
                else {
                    return Ok(());
                };
                let recorded = encode_cached_record(&digest);
                // Another copy of these bytes, whether or not there was
                // room to carry this one: two copies no file can be told
                // apart by, so the one being carried is not repaired. Every
                // eligible record is compared for this reason, since a
                // table that holds sixty-four of them would otherwise call
                // the sixty-fifth's twin unique.
                if let Some(held) = (0..slots)
                    .find(|slot| move_entry(table, *slot)[MOVE_DIGEST..MOVE_ROW] == recorded)
                {
                    let state = &mut table[held * MOVE_ENTRY_BYTES + MOVE_STATE];
                    if *state != MOVE_AMBIGUOUS {
                        *state = MOVE_AMBIGUOUS;
                        assigned.ambiguous = assigned.ambiguous.saturating_add(1);
                    }
                    // Both copies are set aside, the one being carried and
                    // the one that turned up holding its bytes, so both are
                    // counted.
                    assigned.ambiguous = assigned.ambiguous.saturating_add(1);
                    return Ok(());
                }
                if slots >= capacity {
                    return Ok(());
                }
                let slot = &mut table[slots * MOVE_ENTRY_BYTES..][..MOVE_ENTRY_BYTES];
                slot[MOVE_INDEX..MOVE_SIZE].copy_from_slice(&index.to_le_bytes());
                slot[MOVE_SIZE..MOVE_ID].copy_from_slice(&entry.byte_size.to_le_bytes());
                slot[MOVE_ID..MOVE_DIGEST].copy_from_slice(&entry.id.to_bytes());
                slot[MOVE_DIGEST..MOVE_ROW].copy_from_slice(&recorded);
                slot[MOVE_ROW..MOVE_MATCHES].copy_from_slice(&u16::MAX.to_le_bytes());
                slot[MOVE_MATCHES] = 0;
                slot[MOVE_STATE] = 0;
                slot[MOVE_ROOT] = move_root_byte(entry.root);
                let locator = entry.locator.as_bytes();
                let len = locator.len().min(MAX_PATH_BYTES);
                slot[MOVE_LOCATOR_LEN..MOVE_LOCATOR].copy_from_slice(&(len as u16).to_le_bytes());
                slot[MOVE_LOCATOR..MOVE_LOCATOR + len].copy_from_slice(&locator[..len]);
                slots += 1;
                Ok(())
            })?;
        }
    }
    if slots > 0 {
        seek_row(catalog, 0)?;
        for row in 0..count as usize {
            if !read_exact(catalog, &mut record)? {
                return Err(LedgerFault::Device);
            }
            if catalog_record_book_id(&record).is_some() {
                continue;
            }
            let (at, locator, byte_size) = catalog_record_at(&record).ok_or(LedgerFault::Record)?;
            let wanted = (0..slots).any(|slot| move_awaits(table, slot, byte_size));
            if !wanted {
                continue;
            }
            // A file of that length the card would not give up could hold
            // any of those copies' bytes, so the length stops being
            // decidable and every copy of it is left alone. The file is
            // adopted in its own right, as it would have been.
            let Ok(Some(found)) = crate::replace::digest_at(root, at, locator) else {
                move_undecidable(table, slots, byte_size, &mut assigned.unreadable);
                continue;
            };
            assigned.hashed = assigned.hashed.saturating_add(1);
            for slot in 0..slots {
                let entry = move_entry(table, slot);
                if !move_awaits(table, slot, byte_size) {
                    continue;
                }
                let Some(recorded) = parse_record(&entry[MOVE_DIGEST..MOVE_ROW]) else {
                    continue;
                };
                if !recorded.agrees_with(&found) {
                    continue;
                }
                let entry = &mut table[slot * MOVE_ENTRY_BYTES..][..MOVE_ENTRY_BYTES];
                // A second file holding one copy's bytes is the same
                // ambiguity from the other side.
                if entry[MOVE_MATCHES] == 1 {
                    entry[MOVE_MATCHES] = 2;
                    entry[MOVE_STATE] = MOVE_AMBIGUOUS;
                    assigned.ambiguous = assigned.ambiguous.saturating_add(1);
                } else if entry[MOVE_MATCHES] == 0 {
                    entry[MOVE_MATCHES] = 1;
                    entry[MOVE_ROW..MOVE_MATCHES].copy_from_slice(&(row as u16).to_le_bytes());
                }
            }
        }
    }
    let table = &table[..slots * MOVE_ENTRY_BYTES];
    // Told only now: a match is settled once every row has been read, since
    // a second file holding the same bytes makes one ambiguous, and
    // a caller acting on a move that turns out to be two would file a
    // reader's place under another book.
    for slot in 0..slots {
        let entry = move_entry(table, slot);
        // The same test the ledger is written by. A copy with one match and
        // a file nobody read is not a copy that has been found: telling a
        // caller otherwise would have it move a reading place onto a file
        // the next scan may well refuse to give the copy's id to.
        if !move_settled(entry) {
            continue;
        }
        let row = move_u16(entry, MOVE_ROW) as usize;
        seek_row(catalog, row)?;
        if !read_exact(catalog, &mut record)? {
            return Err(LedgerFault::Device);
        }
        let Some((at, locator, byte_size)) = catalog_record_at(&record) else {
            continue;
        };
        let len = move_u16(entry, MOVE_LOCATOR_LEN) as usize;
        let Ok(was) = core::str::from_utf8(&entry[MOVE_LOCATOR..MOVE_LOCATOR + len]) else {
            continue;
        };
        let Some(id) = BookId::from_bytes(
            entry[MOVE_ID..MOVE_DIGEST]
                .try_into()
                .expect("sixteen bytes"),
        ) else {
            continue;
        };
        found_again(&FoundAgain {
            id,
            was: (
                move_root(entry[MOVE_ROOT]),
                was,
                u32::from_le_bytes(entry[MOVE_SIZE..MOVE_ID].try_into().expect("four bytes")),
            ),
            now: (at, locator, byte_size),
        });
    }

    // Live and new copies are the library; a missing record takes a slot
    // only if one is left once they are all in.
    let room = LEDGER_MAX_RECORDS.saturating_sub(live_records + new_rows);
    let mut carried_missing = 0usize;
    let Assignment {
        minted,
        missing,
        retired,
        repaired,
        ..
    } = &mut assigned;
    write_generation(
        root,
        ledger.as_ref(),
        &mut |index, entry| {
            if bit(named, index) {
                return Carry::Keep(Kept {
                    misses: 0,
                    ..Kept::of(entry)
                });
            }
            // A copy the search found again is written where it was found,
            // by the row that holds it, rather than carried as missing from
            // a place it has left.
            if move_slot_of_record(table, slots, index).is_some() {
                return Carry::Drop;
            }
            match carry_missing(entry.misses, room.saturating_sub(carried_missing)) {
                Some(misses) => {
                    carried_missing += 1;
                    *missing += 1;
                    // What this copy's bytes were, taken into its own
                    // record on the scan that first misses it.
                    //
                    // For a book nobody uploaded, the claim beside its
                    // reading place is where that was learned, and a
                    // departed book's directory is exactly what the cache
                    // sweep tidies away. So the ledger takes it first, on
                    // every copy that goes missing, whether or not a file
                    // turned up to compare it with and whether or not it
                    // was one of the copies this scan carried into the
                    // search. Once the library has learned what a copy is,
                    // an ordinary tidy-up cannot make it forget: that is
                    // what stops one of two identical copies losing its
                    // evidence and leaving the other looking unique.
                    let source = entry.source.or_else(|| {
                        move_slot_carrying(table, slots, index)
                            .and_then(|slot| {
                                parse_record(&move_entry(table, slot)[MOVE_DIGEST..MOVE_ROW])
                            })
                            .or_else(|| {
                                // The first miss is the one scan where the
                                // claim is still there to read.
                                (entry.misses == 0)
                                    .then(|| {
                                        claim_digest(
                                            root,
                                            entry.root,
                                            entry.locator,
                                            entry.byte_size,
                                        )
                                    })
                                    .flatten()
                            })
                    });
                    Carry::Keep(Kept {
                        misses,
                        source,
                        ..Kept::of(entry)
                    })
                }
                None => {
                    *retired += 1;
                    Carry::Drop
                }
            }
        },
        |writer| {
            seek_row(catalog, 0)?;
            for row in 0..count as usize {
                if !read_exact(catalog, &mut record)? {
                    return Err(LedgerFault::Device);
                }
                if catalog_record_book_id(&record).is_some() {
                    continue;
                }
                let (at, locator, byte_size) =
                    catalog_record_at(&record).ok_or(LedgerFault::Record)?;
                let found_again = move_slot_of_row(table, slots, row as u16).map(|slot| {
                    let entry = move_entry(table, slot);
                    (
                        BookId::from_bytes(
                            entry[MOVE_ID..MOVE_DIGEST]
                                .try_into()
                                .expect("sixteen bytes"),
                        ),
                        parse_record(&entry[MOVE_DIGEST..MOVE_ROW]),
                    )
                });
                let (id, source) = match found_again {
                    // The copy keeps its id and what its bytes were, which
                    // is what was just read off the file at this row.
                    Some((Some(id), source)) => {
                        *repaired += 1;
                        (id, source)
                    }
                    _ => {
                        *minted += 1;
                        (BookId::mint(random), None)
                    }
                };
                writer.append(&LedgerRecord {
                    id,
                    root: at,
                    locator,
                    byte_size,
                    misses: 0,
                    source,
                })?;
                // Leaves the cursor at the next row, where the read above
                // expects it.
                write_row_id(catalog, row, id)?;
            }
            Ok(())
        },
    )?;
    Ok(assigned)
}

fn bit(bits: &[u8], index: u16) -> bool {
    bits[index as usize / 8] & (1 << (index % 8)) != 0
}

fn set_bit(bits: &mut [u8], index: u16) {
    bits[index as usize / 8] |= 1 << (index % 8);
}

// ---------------------------------------------------------------------------
// Two-slot journals
// ---------------------------------------------------------------------------

/// What a caller's classifier says about one slot of a two-slot journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotVerdict {
    /// All zero: nothing was ever written here.
    Blank,
    /// An entry, ordered against the other slot's by this sequence number.
    Entry(u32),
    /// An entry of a version this build does not read.
    UnknownVersion,
    /// A write torn by a power cut, or damage, or not a journal.
    Damaged,
}

/// The slot files in this crate are all kept the same way: two slots, each
/// a whole number of sectors, an entry at the start of each, written
/// alternately with a sequence number one past the entry being superseded.
/// An entry's checksum covers the whole entry, so a write torn anywhere in
/// the slot reads as damage and the other slot answers.
pub(crate) const SLOT_COUNT: usize = LEDGER_JOURNAL_SLOTS;

/// The newest slot of `name` that reads: its first `N` bytes, its index and
/// its sequence. Slots are `S` bytes each. `None` for a journal that has no
/// entry yet: no file, an empty one, which is what a cut during its first
/// creation leaves, or two blank slots.
///
/// Of two entries the newer by sequence wins. Beside an entry, a slot that
/// is blank or does not read is the slot a later write was torn in, and the
/// entry stands. A journal with no entry that reads, or of a length this
/// crate did not write, is [`LedgerFault::Damaged`]; an entry of a version
/// this build does not read, in either slot, is [`LedgerFault::Unreadable`].
pub(crate) fn newest_slot<
    D,
    T,
    const MD: usize,
    const MF: usize,
    const MV: usize,
    const N: usize,
    const S: usize,
>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    classify: impl Fn(&[u8; N]) -> SlotVerdict,
) -> Result<Option<([u8; N], usize, u32)>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match cache_root.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    if file.length() == 0 {
        return Ok(None);
    }
    if file.length() as usize != S * SLOT_COUNT {
        return Err(LedgerFault::Damaged);
    }
    let mut slots = [[0u8; N]; SLOT_COUNT];
    let mut verdicts = [SlotVerdict::Blank; SLOT_COUNT];
    for (index, slot) in slots.iter_mut().enumerate() {
        file.seek_from_start((index * S) as u32)
            .map_err(|_| LedgerFault::Device)?;
        if !read_exact(&file, slot)? {
            return Err(LedgerFault::Damaged);
        }
        verdicts[index] = classify(slot);
    }
    if verdicts.contains(&SlotVerdict::UnknownVersion) {
        return Err(LedgerFault::Unreadable);
    }
    let mut newest: Option<(usize, u32)> = None;
    for (index, verdict) in verdicts.iter().enumerate() {
        let SlotVerdict::Entry(sequence) = *verdict else {
            continue;
        };
        newest = match newest {
            None => Some((index, sequence)),
            Some((_, held)) if generation_is_newer(sequence, held) => Some((index, sequence)),
            // Two entries one sequence apart are the two most recent writes;
            // equal sequence numbers are nothing this writer produces.
            Some((_, held)) if held == sequence => return Err(LedgerFault::Damaged),
            Some(kept) => Some(kept),
        };
    }
    match newest {
        Some((index, sequence)) => Ok(Some((slots[index], index, sequence))),
        None if verdicts
            .iter()
            .all(|verdict| *verdict == SlotVerdict::Blank) =>
        {
            Ok(None)
        }
        None => Err(LedgerFault::Damaged),
    }
}

/// Publish an entry into the slot the newest entry is not in, one sequence
/// past it. `current` is what [`newest_slot`] found. The file is created
/// whole the first time and never truncated after, so a torn write damages
/// one slot and the entry before it stands.
pub(crate) fn publish_slot<
    D,
    T,
    const MD: usize,
    const MF: usize,
    const MV: usize,
    const N: usize,
    const S: usize,
>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
    current: Option<(usize, u32)>,
    encode: impl FnOnce(u32, &mut [u8; N]) -> Result<(), LedgerFault>,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let (slot, sequence) = match current {
        Some((held, sequence)) => ((held + 1) % SLOT_COUNT, sequence.wrapping_add(1)),
        None => (0, 1),
    };
    let mut block = [0u8; S];
    let mut entry = [0u8; N];
    encode(sequence, &mut entry)?;
    block[..N].copy_from_slice(&entry);
    let file = match cache_root.open_file_in_dir(name, Mode::ReadWriteAppend) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => cache_root
            .open_file_in_dir(name, Mode::ReadWriteCreate)
            .map_err(|_| LedgerFault::Device)?,
        Err(_) => return Err(LedgerFault::Device),
    };
    if file.length() == 0 {
        // A journal that does not exist yet, or one whose creation was cut
        // before it closed: both slots, the entry in the first.
        file.seek_from_start(0).map_err(|_| LedgerFault::Device)?;
        file.write(&block).map_err(|_| LedgerFault::Device)?;
        let blank = [0u8; S];
        file.write(&blank).map_err(|_| LedgerFault::Device)?;
    } else {
        file.seek_from_start((slot * S) as u32)
            .map_err(|_| LedgerFault::Device)?;
        file.write(&block).map_err(|_| LedgerFault::Device)?;
    }
    file.close().map_err(|_| LedgerFault::Device)
}

fn journal_verdict(bytes: &[u8; LEDGER_JOURNAL_BYTES]) -> SlotVerdict {
    match classify_ledger_journal(bytes) {
        LedgerJournalReading::Blank => SlotVerdict::Blank,
        LedgerJournalReading::Entry { sequence, .. } => SlotVerdict::Entry(sequence),
        LedgerJournalReading::UnknownVersion(_) => SlotVerdict::UnknownVersion,
        LedgerJournalReading::Damaged => SlotVerdict::Damaged,
    }
}

/// The ledger journal's newest entry that reads, with its slot and
/// sequence, or `None` for a card that has none.
fn journal_slots<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<(LedgerJournal, usize, u32)>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some((bytes, slot, sequence)) =
        newest_slot::<_, _, MD, MF, MV, LEDGER_JOURNAL_BYTES, LEDGER_JOURNAL_SLOT_BYTES>(
            cache_root,
            LEDGER_JOURNAL,
            journal_verdict,
        )?
    else {
        return Ok(None);
    };
    match classify_ledger_journal(&bytes) {
        LedgerJournalReading::Entry { entry, .. } => Ok(Some((entry, slot, sequence))),
        // `newest_slot` hands back only a slot it classified as an entry.
        _ => Err(LedgerFault::Damaged),
    }
}

fn write_journal<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    entry: LedgerJournal,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let current = journal_slots(cache_root)?.map(|(_, slot, sequence)| (slot, sequence));
    publish_slot::<_, _, MD, MF, MV, LEDGER_JOURNAL_BYTES, LEDGER_JOURNAL_SLOT_BYTES>(
        cache_root,
        LEDGER_JOURNAL,
        current,
        |sequence, out| {
            encode_ledger_journal(entry, sequence, out);
            Ok(())
        },
    )
}

/// What the first sixteen bytes of `side` say. Only a refused open or read
/// is an error here; what a damaged or unreadable header means depends on
/// what the journal says about the side, and that is [`open`]'s call.
fn side_state<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    side: usize,
) -> Result<SideState, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match cache_root.open_file_in_dir(LEDGER_FILES[side], Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(SideState::Absent),
        Err(_) => return Err(LedgerFault::Device),
    };
    if file.length() == 0 {
        return Ok(SideState::Uncommitted);
    }
    let mut bytes = [0u8; LEDGER_HEADER_BYTES];
    if !read_exact(&file, &mut bytes)? {
        return Ok(SideState::Damaged);
    }
    Ok(match classify_ledger_header(&bytes) {
        LedgerHeaderReading::Placeholder => SideState::Uncommitted,
        LedgerHeaderReading::Committed(header) => SideState::Committed(header),
        LedgerHeaderReading::UnknownVersion(_) => SideState::Unreadable,
        LedgerHeaderReading::Damaged => SideState::Damaged,
    })
}

/// Whether a committed generation is exactly as long as its header says and
/// every record on it decodes. `Ok(false)` is a damaged side; `Err` is the
/// card.
fn reads_back_whole<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    side: usize,
    header: LedgerHeader,
) -> Result<bool, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = cache_root
        .open_file_in_dir(LEDGER_FILES[side], Mode::ReadOnly)
        .map_err(|_| LedgerFault::Device)?;
    if file.length() as usize != ledger_file_len(header.count) {
        return Ok(false);
    }
    file.seek_from_start(LEDGER_HEADER_BYTES as u32)
        .map_err(|_| LedgerFault::Device)?;
    let mut bytes = [0u8; LEDGER_RECORD_BYTES];
    for _ in 0..header.count {
        if !read_exact(&file, &mut bytes)? || decode_ledger_record(&bytes).is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn open_or_make_cache_root<'a, D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &'a Directory<'_, D, T, MD, MF, MV>,
) -> Result<Directory<'a, D, T, MD, MF, MV>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => Ok(dir),
        Err(embedded_sdmmc::Error::NotFound) => {
            root.make_dir_in_dir(CACHE_ROOT_DIR)
                .map_err(|_| LedgerFault::Device)?;
            root.open_dir(CACHE_ROOT_DIR)
                .map_err(|_| LedgerFault::Device)
        }
        Err(_) => Err(LedgerFault::Device),
    }
}

fn seek_row<D, T, const MD: usize, const MF: usize, const MV: usize>(
    catalog: &File<'_, D, T, MD, MF, MV>,
    row: usize,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    catalog
        .seek_from_start((CATALOG_HEADER_BYTES + row * CATALOG_RECORD_BYTES) as u32)
        .map_err(|_| LedgerFault::Device)
}

fn write_row_id<D, T, const MD: usize, const MF: usize, const MV: usize>(
    catalog: &File<'_, D, T, MD, MF, MV>,
    row: usize,
    id: BookId,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let at = CATALOG_HEADER_BYTES + row * CATALOG_RECORD_BYTES + CATALOG_RECORD_ID_OFFSET;
    catalog
        .seek_from_start(at as u32)
        .map_err(|_| LedgerFault::Device)?;
    catalog
        .write(&id.to_bytes())
        .map_err(|_| LedgerFault::Device)
}

/// Fill `out` from `file`. `Ok(false)` is a file that ended first; a refused
/// read is the card and is an error, since the two mean different things to
/// every caller here.
pub(crate) fn read_exact<D, T, const MD: usize, const MF: usize, const MV: usize>(
    file: &File<'_, D, T, MD, MF, MV>,
    mut out: &mut [u8],
) -> Result<bool, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !out.is_empty() {
        let read = file.read(out).map_err(|_| LedgerFault::Device)?;
        if read == 0 {
            return Ok(false);
        }
        let rest = out;
        out = &mut rest[read..];
    }
    Ok(true)
}
