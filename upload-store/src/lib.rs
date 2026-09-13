//! Book storage on the SD card, and the leftovers of an older way of doing it.
//!
//! The upload path itself lives in [`install`]: a book streams into scratch
//! space outside `/BOOKS` and is published by moving its directory entry, with
//! one journalled record covering the swap. Nothing here participates in that.
//!
//! What remains is the small shared vocabulary — removing a file without
//! leaking its cluster chain, comparing long names the way FAT does — and the
//! read side of the label sidecars.
//!
//! Those sidecars are history. Before uploads carried a VFAT long name, a book
//! on the card was an 8.3 alias and its real name lived in `READER/LABELS`.
//! Books written that way are still on people's cards, so the catalog scan
//! still falls back to reading their labels, and deleting a book still clears
//! them. Nothing writes new ones.
//!
//! They are filed under the alias alone, which was an identity when every book
//! the scheme wrote sat directly in one of two directories. It is not one now
//! that the shelf nests: FAT hands out 8.3 names per directory, so the reader
//! takes a book's position as well as its alias and answers only where a label
//! could have been written.

#![no_std]
#![forbid(unsafe_code)]

pub mod install;
pub mod ledger;
pub mod library;
pub mod reclaim;
pub mod replace;

use embedded_sdmmc::{Directory, Mode, TimeSource};
use heapless::String;
use proto::cache::CACHE_ROOT_DIR;
use proto::library_path::BookRoot;

/// Subdir under the cache root holding one `<8.3-stem>.TXT` per book uploaded before
/// uploads carried a long name of their own, each with that book's real
/// filename. Read-only now: the catalog scan falls back to it for those older
/// books, and deleting a book clears whatever it left.
const LABELS_DIR: &str = "LABELS";

fn label_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".TXT");
}

/// The identity sidecar an older upload path wrote beside the label. Only
/// ever removed now.
fn identity_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".ID");
}

/// Where a computed digest is remembered, beside the label for the same book.
fn source_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".SRC");
}

/// A directory handle keeps the volume manager's lifetime, not the borrow of
/// the parent it was opened through, so a file opened inside it can outlive
/// the handles walked to reach it.
pub(crate) fn open_or_make_dir<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
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

/// Read the stashed label for the book at `locator` under `at`, whose 8.3
/// alias is `open_name`, into `out`. Returns false (leaving `out` untouched)
/// when the book has no sidecar -- i.e. it wasn't uploaded, so the caller falls
/// back to the file-stem label.
///
/// The position is part of the question, not context the caller is trusted to
/// have checked. A label is filed under the alias alone, and an alias is only
/// unique within one directory, so the file name it names is only an answer
/// where the scheme that wrote these could have put a book: directly under the
/// shelf, or at the card root. A book in a folder a reader made on a computer
/// is outside that reach ([`shelf_placement`]), and a label matching its alias
/// belongs to some other file -- one still sitting in the shelf, or one deleted
/// from a computer, which cannot clear what it left in `LABELS`.
pub fn read_upload_label<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    at: BookRoot,
    locator: &str,
    open_name: &str,
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if shelf_placement(at, locator).is_none() {
        return false;
    }
    let Ok(cache_root) = root.open_dir(CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(labels) = cache_root.open_dir(LABELS_DIR) else {
        return false;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let Ok(file) = labels.open_file_in_dir(file_name.as_str(), Mode::ReadOnly) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let Ok(read) = file.read(&mut buf) else {
        return false;
    };
    let Ok(text) = core::str::from_utf8(&buf[..read]) else {
        return false;
    };
    if text.is_empty() {
        return false;
    }
    out.clear();
    let _ = out.push_str(text);
    true
}

/// The outcome of `remove_file_reclaiming_clusters`.
///
/// `Absent` means the file was already gone — the desired end state for
/// callers that only care that the name is free, but *not* proof that this
/// call deleted anything, which is why sidecar cleanup keys on `Removed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveStatus {
    Removed,
    Absent,
    Failed,
}

/// Remove a file without leaking its FAT cluster chain.
///
/// The pinned embedded-sdmmc delete only marks the directory entry deleted; it
/// does not release the file's clusters. `Mode::ReadWriteTruncate` calls
/// `truncate_cluster_chain`, which walks and frees the cluster chain and writes
/// the zeroed directory entry before returning (embedded-sdmmc d26892f,
/// `VolumeManager::open_file_in_dir`).
///
/// # This is not safe to interrupt
///
/// It once said here that a fault in that write-back leaves the entry at its
/// original length, so the file stays readable. That is wrong, and a
/// durability campaign proved it on hardware: the chain is freed *before* the
/// entry is rewritten, so a reset in between leaves an entry advertising its
/// old size over clusters that are already free. The file is listed and
/// unreadable, and no amount of retrying finds the data again.
///
/// So this belongs only where an interruption is invisible to a reader and
/// costs at most leaked space — clearing a journal, discarding scratch,
/// sweeping leftovers, evicting a cache. Anything a reader can see the name
/// of goes through [`crate::reclaim`], which records the chain before
/// freeing any of it and takes the name away first.
///
/// Files only: opening a directory as a file fails, which would report the
/// delete as failed without attempting it. Directory entries hold no cluster
/// chain of their own to leak, so they stay on `delete_entry_in_dir`.
pub fn remove_file_reclaiming_clusters<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> RemoveStatus
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    {
        match directory.open_file_in_dir(name, Mode::ReadWriteTruncate) {
            Ok(file) => {
                if file.close().is_err() {
                    return RemoveStatus::Failed;
                }
            }
            Err(embedded_sdmmc::Error::NotFound) => return RemoveStatus::Absent,
            Err(_) => return RemoveStatus::Failed,
        }
    }
    match directory.delete_entry_in_dir(name) {
        Ok(()) => RemoveStatus::Removed,
        Err(embedded_sdmmc::Error::NotFound) => RemoveStatus::Absent,
        Err(_) => RemoveStatus::Failed,
    }
}

/// Remove `name` from the cache root, and report whether it is provably gone.
///
/// True means the card said so: removed, already absent, or no cache root to
/// hold it. False means the card would not answer, and the caller must treat
/// the file as still there.
///
/// The distinction matters for the catalog snapshot. A caller about to change
/// `/BOOKS` invalidates it first, because a transaction that finishes leaves
/// nothing behind saying it happened: an install clears its journal, and a
/// delete — which does write one now — clears its reclaim record the same
/// way. So once the change is made, nothing on the card would tell the next
/// mount that the snapshot describes a shelf that has moved.
pub fn clear_cache_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return true,
        Err(_) => return false,
    };
    remove_file_reclaiming_clusters(&cache_root, name) != RemoveStatus::Failed
}

pub fn delete_upload_sidecars<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(cache_root) = root.open_dir(CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(labels) = cache_root.open_dir(LABELS_DIR) else {
        return;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let _ = remove_file_reclaiming_clusters(&labels, file_name.as_str());

    file_name.clear();
    identity_file_name(open_name, &mut file_name);
    let _ = remove_file_reclaiming_clusters(&labels, file_name.as_str());

    // The digest goes with the book. A freed alias is handed to whatever is
    // written next, and a record left behind would then describe a file it
    // has nothing to do with.
    file_name.clear();
    source_file_name(open_name, &mut file_name);
    let _ = remove_file_reclaiming_clusters(&labels, file_name.as_str());
}

/// Remember what a book hashed to, beside the book's label.
///
/// Best-effort: derived state, and losing it costs a read rather than a book.
/// The caller supplies a digest it already holds, so nothing here reads the
/// EPUB.
pub fn record_source_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
    digest: &proto::source::SourceDigest,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(cache_root) = open_or_make_dir(root, CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(labels) = open_or_make_dir(&cache_root, LABELS_DIR) else {
        return;
    };
    let mut file_name = String::<12>::new();
    source_file_name(open_name, &mut file_name);
    // Truncated rather than appended: one record per book, and a shorter
    // record over a longer one would otherwise leave the tail of the old.
    let Ok(file) = labels.open_file_in_dir(file_name.as_str(), Mode::ReadWriteCreateOrTruncate)
    else {
        return;
    };
    let _ = file.write(&proto::source::encode_record(digest));
    let _ = file.close();
}

/// What the book at `open_name` hashed to when it was last looked at.
///
/// Evidence, in a type that says so. It describes whatever held this alias
/// when it was written, and a computer can delete that book or replace it,
/// leaving the record intact. Removing it alongside a managed delete is
/// hygiene; reading the file is what makes alias reuse safe.
///
/// `None` covers absent, unreadable, and untrusted alike, because each means
/// hash the file again.
pub fn cached_source_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) -> Option<proto::source::CachedSourceDigest>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let labels = cache_root.open_dir(LABELS_DIR).ok()?;
    let mut file_name = String::<12>::new();
    source_file_name(open_name, &mut file_name);
    let file = labels
        .open_file_in_dir(file_name.as_str(), Mode::ReadOnly)
        .ok()?;
    let mut buf = [0u8; proto::source::SOURCE_RECORD_BYTES];
    let read = file.read(&mut buf).ok()?;
    let _ = file.close();
    proto::source::parse_record(&buf[..read])
}

/// The shelf, opened through the caller's own root so it is that card's.
pub const SHELF_DIR: &str = "BOOKS";

/// Where a book sits, as far as the browser shelf can say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShelfPlacement {
    /// At the card root.
    Root,
    /// Directly under the shelf.
    Shelf,
}

/// Which of the two places the shelf can name a book in, or `None` for one it
/// cannot.
///
/// The shelf addresses a book by a placement flag and an 8.3 alias, which
/// between them reach the library root and the card root and nothing below
/// either. A book in a folder a reader made on a computer is outside that
/// reach, and saying `Root` or `Shelf` for it would name a different file or
/// none.
pub fn shelf_placement(at: BookRoot, path: &str) -> Option<ShelfPlacement> {
    // One component: a book sitting directly in the root it is relative to.
    if path.is_empty() || path.contains(proto::library_path::SEPARATOR) {
        return None;
    }
    Some(match at {
        BookRoot::Library => ShelfPlacement::Shelf,
        BookRoot::CardRoot => ShelfPlacement::Root,
    })
}

/// What this file holds, read from the card now.
///
/// The key names the file and its directory, and the shelf is opened through
/// the given root, so the bytes hashed here answer for that key on this card.
/// `Ok(None)` is a file that is not there.
///
/// Reads, and writes nothing. Refreshing the record beside the book would
/// allocate, and the shelf stays readable while a reclaim is unsettled, when
/// an allocation can be handed a cluster the journal already recorded. A
/// caller that knows the card is safe to mutate calls
/// [`record_source_identity`] itself, and losing the refresh costs a read
/// rather than an answer.
///
/// Costs the whole file, tens of seconds for a large book, so ask only before
/// claiming two files hold the same bytes.
pub fn source_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &proto::source::FileKey,
) -> Result<Option<proto::source::SourceDigest>, install::InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let found = if key.in_books() {
        match library::open_library_root(root)? {
            Some(books) => digest_of_file(&books, key.alias())?,
            // No shelf holds no sidecar, which reads as no recorded identity.
            None => None,
        }
    } else {
        digest_of_file(root, key.alias())?
    };
    Ok(found)
}

/// Whether two physical files hold the same bytes, and may therefore share
/// state derived from their contents.
///
/// The first question that widens reuse past one file. Both files are read in
/// this call, since anything cheaper would share on a coincidence: equal
/// sizes, a freed alias handed on, or a record describing a book that is gone.
/// `Ok(None)` is one of them not being there, which is an absence rather than
/// an answer.
///
/// Writes nothing, as [`source_identity`] explains, and remembers nothing.
/// [`remove_file_reclaiming_clusters`] mutates through whatever directory it
/// is handed, so a remembered answer could outlive the file it described with
/// nothing here able to see it. Amortizing several questions wants a session
/// owning the handles and mediating reads and writes, worth building when a
/// consumer asks several at once.
pub fn may_share_derived_state<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    one: &proto::source::FileKey,
    other: &proto::source::FileKey,
) -> Result<Option<bool>, install::InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(first) = source_identity(root, one)? else {
        return Ok(None);
    };
    if one == other {
        // The same file, read once. Its presence is the whole question, and
        // reading it twice would answer the same thing at twice the price.
        return Ok(Some(true));
    }
    let Some(second) = source_identity(root, other)? else {
        return Ok(None);
    };
    Ok(Some(first == second))
}

/// Bytes read per pass while hashing a book already on the card. One sector,
/// on the caller's stack.
const DIGEST_READ_BYTES: usize = 512;

/// The identity of a book already on the card, read out of it.
///
/// `Ok(None)` is a name that is not there. Anything else is `Err`, including a
/// read that ends short of the length the entry claims: a digest describing a
/// partial read would be a wrong answer shaped like a right one.
pub fn digest_of_file<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Directory<'_, D, T, MD, MF, MV>,
    name: &str,
) -> Result<Option<proto::source::SourceDigest>, install::InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(install::InstallError::Card),
    };
    let length = file.length();
    let mut hasher = proto::source::SourceHasher::new();
    let mut buf = [0u8; DIGEST_READ_BYTES];
    let mut total = 0u32;
    while !file.is_eof() {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => {
                let _ = file.close();
                return Err(install::InstallError::Card);
            }
        };
        hasher.update(&buf[..read]);
        total = total.saturating_add(read as u32);
    }
    if file.close().is_err() {
        return Err(install::InstallError::Card);
    }
    // A short read that reported no error still describes a different book,
    // and the entry is the only claim about how long this one is.
    if total != length {
        return Err(install::InstallError::Card);
    }
    Ok(Some(hasher.finish()))
}

pub fn read_upload_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) -> Result<Option<u64>, install::InstallError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(install::InstallError::Card),
    };
    let labels = match cache_root.open_dir(LABELS_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(install::InstallError::Card),
    };
    let mut file_name = String::<12>::new();
    identity_file_name(open_name, &mut file_name);
    let file = match labels.open_file_in_dir(file_name.as_str(), Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(install::InstallError::Card),
    };
    let mut buf = [0u8; 8];
    let parsed = proto::upload::parse_identity_read(file.length(), file.read(&mut buf), &buf);
    if file.close().is_err() {
        return Err(install::InstallError::Card);
    }
    parsed.map_err(|()| install::InstallError::Card)
}

/// Whether two VFAT long names denote the same file.
///
/// FAT ignores case over Unicode, so an ASCII-only comparison would let
/// `Märchen.epub` and `MÄRCHEN.epub` become two entries a computer sees as
/// one name twice. This is `core`'s simple case mapping, which matches how
/// the driver compares names when it refuses a duplicate. Simple mapping,
/// not full folding: the rare pairs that differ (ß/SS and friends) read as
/// distinct names, which is the safe direction — a missed match adds an
/// entry rather than deleting the wrong book.
fn same_long_name(left: &str, right: &str) -> bool {
    let mut left = left.chars().flat_map(char::to_lowercase);
    let mut right = right.chars().flat_map(char::to_lowercase);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (a, b) if a != b => return false,
            _ => {}
        }
    }
}

/// Long-name scan buffer for finding the book that holds a name.
///
/// Sized against the upload name budget, not the FAT maximum: a name this
/// crate can produce is at most [`proto::upload::UPLOAD_FILENAME_BYTES`] (64) of UTF-8, so
/// 256 is generous, where a full 255-character FAT name would need roughly
/// three times it.
///
/// Undersizing is safe here because of how the pinned driver behaves: a long
/// name too large for the buffer comes back empty, so an oversized entry — a
/// hand-copied file with a very long name — is not recognised as the holder.
/// It cannot be a false negative, since a name that could equal ours is at
/// most 64 bytes and always decodes.
///
/// That proof rests on comparing only names this crate wrote. The library
/// resolver's job is different: it matches computer-written components up
/// to `proto::library_path::MAX_COMPONENT_BYTES`, so it sizes its own
/// buffer to that bound rather than borrowing this one. Names longer than
/// that are outside the locator model there, and outside the upload
/// namespace here.
const LFN_SCAN_BYTES: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shelf_names_the_two_places_uploads_land() {
        assert_eq!(
            shelf_placement(BookRoot::CardRoot, "Dune.epub"),
            Some(ShelfPlacement::Root)
        );
        assert_eq!(
            shelf_placement(BookRoot::Library, "Dune.epub"),
            Some(ShelfPlacement::Shelf)
        );
    }

    #[test]
    fn a_book_the_shelf_cannot_address_gets_no_placement() {
        // Deeper than the flag can say, so the shelf leaves it out rather
        // than naming a file it did not mean.
        assert_eq!(
            shelf_placement(BookRoot::Library, "Fiction/Dune.epub"),
            None
        );
        assert_eq!(
            shelf_placement(BookRoot::CardRoot, "Fiction/Dune.epub"),
            None
        );
        // No locator names no book.
        assert_eq!(shelf_placement(BookRoot::Library, ""), None);
    }
}
