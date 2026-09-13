use crate::display_flush::Epd;
use crate::sd_session;
use core::ops::ControlFlow;
use embassy_time::Instant;
use embedded_sdmmc::{Directory, File, LfnBuffer, Mode, TimeSource};
use esp_hal::gpio::Output;
use heapless::String;
use reader_cache::store::{derive_catalog_label, LibraryScanStatus, ReaderStore, LIBRARY_WINDOW};

/// Every file this firmware owns on the card lives here, catalog and
/// diagnostics alike (see `crate::probe_report`). One definition, shared with
/// the upload journal and the reader cache: two spellings of this directory
/// is two halves of the firmware disagreeing about where the card is.
pub(crate) use proto::cache::CACHE_ROOT_DIR as CATALOG_ROOT_DIR;
use proto::cache::CATALOG_FILE;
use proto::catalog::{
    catalog_count, catalog_file_len, catalog_identity_staged, catalog_record_identity,
    decode_catalog_record, encode_catalog_header, encode_catalog_placeholder_header,
    encode_catalog_record, encode_catalog_title, sort_catalog_identities, stage_catalog_identity,
    CatalogRecord, CATALOG_HEADER_BYTES, CATALOG_IDENTITY_BYTES, CATALOG_RECORD_BYTES,
    CATALOG_RECORD_TITLE_OFFSET, CATALOG_TITLE_BYTES,
};
use proto::library_path::BookRoot;

/// Why a catalog read did not hand back a catalog.
///
/// All four are one `Err(())` to any caller that only wants to know whether
/// it worked. They are kept apart for the one that reports: a bench capture
/// has to tell the ordinary cold path from a fault on it, and collapsing them
/// — which `read_catalog_window(..).is_ok()` did — reported a failed record
/// read as a card that simply had no catalog yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogFault {
    /// Nothing to load: no cache-root directory, or no `CATALOG.BIN` in it.
    /// This is the normal state of a card whose catalog has not been built,
    /// and what makes the caller queue a scan.
    Missing,
    /// A catalog written by firmware of another version. Bumping
    /// `CATALOG_VERSION` is how this format migrates — the old snapshot stops
    /// loading and the scan rebuilds it — so this is the designed first boot
    /// after an upgrade, and calling it a fault failed a strict capture for
    /// behaving as intended.
    Stale,
    /// The file is there and does not check out: wrong magic, the version-0
    /// placeholder an interrupted scan leaves behind, a length disagreeing
    /// with its header, or a record that ended early. The card answered; what
    /// it held is unusable.
    Invalid,
    /// The card refused an open, a seek, or a read.
    Device,
    /// Not a fault at all: recovery removed an interrupted upload, so any
    /// catalog written before it may name a file that is now gone. The
    /// rescan that follows is the repair, not a symptom.
    Reclaimed,
    /// Recovery could not finish, so nothing has established what the shelf
    /// now holds. A refused replacement is the sharp one: `INSTALL.JNL` has
    /// already cleared, so no install reports an intent, while the ledger has
    /// yet to be told which bytes stand at the locator. The scan that follows
    /// refuses to rebuild for the same reason, so the library stays empty
    /// until a mount where recovery settles.
    Unreconciled,
}

/// What a lookup of one place in the catalog found.
///
/// Three-valued because the caller acts on the difference. A catalog that
/// answered and holds no such place is a catalog older than the card, and
/// rebuilding it is the repair. A catalog that would not answer says nothing
/// about the card, and rebuilding on it would retire a usable snapshot
/// because one read failed.
pub(crate) enum CatalogRow {
    /// The catalog holds this place, at this row.
    Found(u16),
    /// The catalog was read through and holds no such place. Also a catalog
    /// that is absent, or written by another version, or damaged: each of
    /// those is a snapshot that has to be rebuilt before it can answer.
    Rebuild,
    /// The card refused a read. Not evidence about what the catalog holds.
    Unreadable,
}

/// A failed open is only a missing catalog when the card said so.
fn open_fault<E: core::error::Error>(error: embedded_sdmmc::Error<E>) -> CatalogFault {
    match error {
        embedded_sdmmc::Error::NotFound => CatalogFault::Missing,
        _ => CatalogFault::Device,
    }
}

impl CatalogFault {
    /// The `result=` token a bench capture is judged on.
    const fn bench_result(self) -> &'static str {
        match self {
            Self::Missing => "miss",
            Self::Stale => "stale",
            Self::Invalid => "invalid",
            Self::Device => "error",
            Self::Reclaimed => "reclaimed",
            Self::Unreconciled => "unreconciled",
        }
    }
}

#[inline(never)]
pub(crate) fn scan_books(epd: &mut Epd, sd_cs: &mut Output<'static>, library: &mut ReaderStore) {
    let start = Instant::now();
    esp_println::println!("sd: scan start");
    library.status = LibraryScanStatus::Scanning;

    let status = sd_session::with_root(epd, sd_cs, |root| {
        esp_println::println!("sd: card init begin");
        esp_println::println!("sd: open root");
        library.status = LibraryScanStatus::Scanning;
        let reconciled = reconcile_interrupted_uploads(root);
        // The scanner knows nothing of installs in flight, so a shelf with
        // one pending may list whichever copy the interrupted swap left —
        // possibly the book about to be replaced. That is still a real book,
        // and listing it beats handing a cold-booted reader an empty library,
        // which a record this build cannot read would do for good. So the
        // catalog is published either way and `scan_ok` below carries the
        // unreconciled state; the next mount finds the record still standing
        // and rebuilds rather than trusting the snapshot.
        //
        // The resident catalog is cleared only once a scan is actually going
        // to run. Clearing first would make the fallback below meaningless —
        // it keeps the in-memory catalog when a scan fails, and an emptied
        // one is never non-empty — so a card that would not answer would take
        // the reader's whole shelf rather than postponing the rebuild.
        let scanned = if !reconciled.shelf_readable {
            esp_println::println!("sd: shelf unreadable; keeping the catalog for the next mount");
            Err(())
        } else if !reconciled.may_mutate {
            // A reclaim that did not settle leaves cluster numbers recorded
            // and possibly already free. Writing the catalog would allocate,
            // and could be handed one of them; the replay that eventually
            // runs would then free it back out of `CATALOG.BIN`. The
            // resident catalog is left alone and the next mount tries again.
            esp_println::println!("sd: storage recovery unfinished; not rebuilding the catalog");
            Err(())
        } else {
            // The ledger is asked first, before the resident catalog is
            // cleared or CATALOG.BIN is truncated. A ledger that refuses is
            // durable identity state this build will not guess about, and
            // the one safe answer is to change nothing: the committed catalog
            // keeps serving the shelf as it was, and the next mount asks
            // again.
            match upload_store::ledger::open(root) {
                Err(fault) => {
                    esp_println::println!(
                        "sd: library ledger {:?}; keeping the catalog for the next mount",
                        fault
                    );
                    Err(())
                }
                Ok(ledger) => {
                    // The 16 KB section text arena doubles as the scan's
                    // staging and identity scratch: a scan runs from the
                    // storage dispatcher (boot or an explicit refresh), never
                    // while a page render is reading the arena, and the
                    // section window is invalidated below so a stale page
                    // can't be served from clobbered text afterwards.
                    library.clear_catalog();
                    write_catalog_streaming(root, library.arena_as_scratch(), ledger)
                }
            }
        };
        let status = match scanned {
            Ok(0) => LibraryScanStatus::Empty,
            Ok(count) => {
                esp_println::println!("sd: catalog written, {} epub(s)", count);
                // Re-open and fully validate the finalized catalog (header,
                // version, length, first window) before it is allowed to
                // drive destructive orphan reclamation: a torn or
                // misbehaving write must never convince the sweep that
                // still-present books are gone. This also reloads the
                // header count + first list window from the file just
                // written, so the streaming readers and the store agree.
                if read_catalog_window(root, library, 0).is_err() {
                    LibraryScanStatus::Error
                } else {
                    // Drop the cached data of books no longer on the card:
                    // this is the one moment the full book set is known and
                    // the catalog is proven fresh. Not while an install is
                    // pending: a parked predecessor is off the shelf but its
                    // cache is still wanted, and a rollback would bring the
                    // book back bare.
                    if reconciled.outcome.complete {
                        sweep_orphan_caches(root, library.arena_as_scratch());
                    }
                    LibraryScanStatus::Ready
                }
            }
            Err(()) => LibraryScanStatus::Error,
        };
        // The arena held scan (and sweep) scratch, not section text: drop
        // the resident section (and any Chapters TOC window) so nothing
        // renders from it.
        library.clear_lines();
        library.set_text_holds_toc(false);
        (status, reconciled.outcome.complete)
    })
    .unwrap_or_else(|err| {
        esp_println::println!("sd: session failed: {:?}", err);
        (LibraryScanStatus::Error, false)
    });
    let (status, reconciled) = status;
    // The scan's own verdict, taken before the fallback below can replace it.
    // That fallback keeps the UI on an older in-memory catalog when a scan
    // fails with books already listed — right for the reader, wrong for
    // telemetry, since `library.status` then reads `Ready` for a scan that
    // did not happen. `Empty` is a scan that succeeded and found nothing.
    //
    // A catalog published over an unreconciled shelf reads the same way, and
    // for the same reason: the reader gets a shelf, but it describes a book
    // set an install in flight may still move.
    let scan_ok = status != LibraryScanStatus::Error && reconciled;
    library.status = if status == LibraryScanStatus::Error && !library.catalog_is_empty() {
        LibraryScanStatus::Ready
    } else {
        status
    };
    esp_println::println!("sd: scan complete, {} epub(s)", library.catalog_count());
    bench_log!(
        "bench: storage_catalog action=scan ok={} status={:?} count={} elapsed_ms={} t_ms={}",
        scan_ok,
        library.status,
        library.catalog_count(),
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
}

/// What one reconciliation pass found.
struct Reconciled {
    outcome: upload_store::install::InstallRecovery,
    /// `/BOOKS` opened, or is genuinely not there. False is a card that would
    /// not answer, where a scan would fail too — and clearing the resident
    /// catalog to run one would cost the reader their shelf for nothing.
    shelf_readable: bool,
    /// Safe to allocate on this card.
    ///
    /// Separate from `shelf_readable`, because a stalled reclaim is readable
    /// and must not be written over. Its record names clusters that may
    /// already be free, and those numbers carry no ownership — so anything
    /// that allocates before it settles can be handed one, and the replay
    /// that eventually runs will free it back out from under whatever took
    /// it. A rebuilt catalog is exactly such an allocation.
    may_mutate: bool,
}

/// Finish any install an earlier session left in flight.
///
/// This must run on *every* boot path that goes on to serve the shelf, not
/// only the scan: a session invalidates `CATALOG.BIN` at entry, so a crash
/// normally forces a rescan, but that deletion is best-effort and recovery
/// must not depend on another module's side effect having succeeded.
fn reconcile_interrupted_uploads<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Reconciled
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // The same distinction `walk_epubs` draws: a genuinely absent /BOOKS
    // holds nothing to finish, while one that will not open has not answered.
    // Only the first is a clean shelf.
    let books = match upload_store::library::open_library_root(root) {
        Ok(Some(books)) => books,
        // No shelf means no install can be finished here, but either journal
        // may still be describing one -- and a record that stands must keep a
        // cached catalog from being trusted and keep a fresh one from being
        // published, whether or not there is a /BOOKS to look at.
        Ok(None) => {
            use upload_store::install::IntentState;
            // The reclaim journal is consulted whether or not there is a
            // shelf. A reclaim can name the card root -- the OTA trigger
            // does -- and one of those is replayable here; a reclaim that
            // names the shelf is refused rather than assumed finished, since
            // its clusters are detached and nothing may allocate over them.
            // Either way this must be asked before the scan is let loose to
            // write a catalog.
            match upload_store::reclaim::recover(root, None) {
                #[cfg(feature = "powercut-selftest")]
                Ok(true) => esp_println::println!("powercut: recovery replayed a reclaim"),
                Ok(_) => {}
                Err(error) => {
                    // Nothing else is touched. Clearing a truncated install
                    // record below would mutate this card, and the reclaim
                    // journal not settling is precisely the state in which this
                    // build has decided it does not know enough to. When the
                    // failure is the journal refusing to read, that reader
                    // deliberately infers nothing from the other slot -- so
                    // answering it by writing to a second journal would take
                    // back the fail-stop it just asked for.
                    esp_println::println!(
                        "sd: a reclaim is unfinished and there is no shelf ({:?})",
                        error
                    );
                    return Reconciled {
                        outcome: upload_store::install::InstallRecovery {
                            touched_shelf: false,
                            swept: false,
                            // Conservative: an install may well be
                            // outstanding, and this pass has not looked.
                            had_intent: true,
                            complete: false,
                        },
                        shelf_readable: true,
                        may_mutate: false,
                    };
                }
            }
            let (mut had_intent, complete) = match upload_store::install::read_intent(root) {
                Ok(IntentState::Absent) => (false, true),
                // Nothing to replay, but something was there — and whatever
                // it was may have moved the shelf before it went. Reclaim it
                // here too, or it would retire the catalog on every mount
                // for as long as the shelf stays missing.
                Ok(IntentState::Truncated) => {
                    (true, upload_store::install::clear_intent(root).is_ok())
                }
                Ok(IntentState::Valid(_)) | Ok(IntentState::Unrecognized) => (true, false),
                Err(_) => (true, false),
            };
            // The library transaction does not live on the shelf. It stands
            // in /READER and can name a copy at the card root, which is still
            // there to read a landing from, so this resolves what it can and
            // refuses the rest rather than refusing everything.
            let settled = if complete {
                match upload_store::replace::recover(root) {
                    Ok(upload_store::replace::Recovery::Nothing) => true,
                    Ok(upload_store::replace::Recovery::Settled(landed)) => {
                        esp_println::println!("sd: settled a replacement in flight ({:?})", landed);
                        had_intent = true;
                        true
                    }
                    Ok(upload_store::replace::Recovery::Refused) => {
                        esp_println::println!(
                            "sd: a replacement is unresolved; not rebuilding the catalog"
                        );
                        false
                    }
                    Err(fault) => {
                        esp_println::println!(
                            "sd: library ledger {:?}; not rebuilding the catalog",
                            fault
                        );
                        false
                    }
                }
            } else {
                // Same order as the shelf-present path: read it, do not
                // resolve it, while the filesystem transaction is unsettled.
                match upload_store::replace::read(root) {
                    Ok(None) => true,
                    Ok(Some(_)) => {
                        esp_println::println!(
                            "sd: a replacement stands over an unfinished install; \
                             not rebuilding the catalog"
                        );
                        false
                    }
                    Err(fault) => {
                        esp_println::println!(
                            "sd: library ledger {:?}; not rebuilding the catalog",
                            fault
                        );
                        false
                    }
                }
            };
            return Reconciled {
                outcome: upload_store::install::InstallRecovery {
                    touched_shelf: false,
                    swept: false,
                    had_intent,
                    complete,
                },
                // Nothing to reconcile against, but a scan still has the card
                // root to walk.
                shelf_readable: true,
                // Reclaim settled above, or this branch returned there.
                may_mutate: settled,
            };
        }
        Err(_) => {
            esp_println::println!("sd: shelf unreadable; recovery cannot report it clean");
            // Not knowing whether an install is in flight is not the same as
            // knowing there is none, and a cached catalog must not be
            // trusted on the strength of a shelf that would not open.
            return Reconciled {
                outcome: upload_store::install::InstallRecovery {
                    touched_shelf: false,
                    swept: false,
                    had_intent: true,
                    complete: false,
                },
                shelf_readable: false,
                may_mutate: false,
            };
        }
    };
    // Reclaim before installs, always. A reclaim may be part way through
    // freeing a chain, and its record is the only thing that can find the
    // rest; the install journal's own steps allocate and free, so letting
    // them run first would be reasoning about a shelf whose spare space is
    // still being sorted out. Nothing here can proceed over a reclaim
    // journal this build cannot read, which is what the refusal below says.
    match upload_store::reclaim::recover(root, Some(&books)) {
        #[cfg(feature = "powercut-selftest")]
        Ok(true) => esp_println::println!("powercut: recovery replayed a reclaim"),
        Ok(_) => {}
        Err(error) => {
            esp_println::println!("sd: a reclaim is unfinished ({:?})", error);
            return Reconciled {
                outcome: upload_store::install::InstallRecovery {
                    touched_shelf: false,
                    swept: false,
                    had_intent: true,
                    complete: false,
                },
                shelf_readable: true,
                // Readable, and not to be written on: see `may_mutate`.
                may_mutate: false,
            };
        }
    }
    let mut outcome = upload_store::install::recover_installs(root, &books);
    #[cfg(feature = "powercut-selftest")]
    crate::powercut::report_recovery(&outcome);
    if outcome.touched_shelf {
        esp_println::println!("sd: finished an interrupted install");
    }
    // The library's own transaction, after the filesystem's and not before.
    // A replacement whose intent still stands has a destination the
    // filesystem has now settled, and the ledger has to be told what that
    // means before any scan reads the place as a stranger and mints for it.
    if outcome.complete {
        match upload_store::replace::recover(root) {
            Ok(upload_store::replace::Recovery::Nothing) => {}
            Ok(upload_store::replace::Recovery::Settled(landed)) => {
                esp_println::println!("sd: settled a replacement in flight ({:?})", landed);
                // The shelf changed under whatever snapshot predates it.
                outcome.had_intent = true;
            }
            Ok(upload_store::replace::Recovery::Refused) => {
                esp_println::println!(
                    "sd: a replacement is unresolved; not rebuilding the catalog"
                );
                return Reconciled {
                    outcome,
                    shelf_readable: true,
                    may_mutate: false,
                };
            }
            Err(fault) => {
                esp_println::println!("sd: library ledger {:?}; not rebuilding the catalog", fault);
                return Reconciled {
                    outcome,
                    shelf_readable: true,
                    may_mutate: false,
                };
            }
        }
    } else {
        // Looked for, not resolved: the intent is only resolved after the
        // filesystem transaction settles, which this one has not. An intent
        // standing over an unsettled install names a locator whose identity is
        // undecided, and a scan let through here would mint a fresh id for
        // whatever spelling the shelf holds now.
        match upload_store::replace::read(root) {
            Ok(None) => {}
            Ok(Some(_)) => {
                esp_println::println!(
                    "sd: a replacement stands over an unfinished install; \
                     not rebuilding the catalog"
                );
                return Reconciled {
                    outcome,
                    shelf_readable: true,
                    may_mutate: false,
                };
            }
            Err(fault) => {
                esp_println::println!("sd: library ledger {:?}; not rebuilding the catalog", fault);
                return Reconciled {
                    outcome,
                    shelf_readable: true,
                    may_mutate: false,
                };
            }
        }
    }
    if !outcome.swept {
        // Invisible to the reader either way; the next mount tries again.
        esp_println::println!("sd: could not clear every leftover upload file");
    }
    // Only worth reopening the journal when recovery did not settle: a record
    // this build cannot read is one of the reasons it would not.
    if !outcome.complete
        && matches!(
            upload_store::install::read_intent(root),
            Ok(upload_store::install::IntentState::Unrecognized)
        )
    {
        esp_println::println!(
            "sd: {}/{} is from a build this one cannot read; \
             uploads and deletes are refused until it is resolved",
            CATALOG_ROOT_DIR,
            upload_store::install::JOURNAL_FILE
        );
    }
    if !outcome.complete {
        // Deliberately not fatal. An unfinished install leaves the shelf
        // holding one complete book either way — the old one or the new one
        // — and the next mount picks the transaction up again. Refusing to
        // serve the library over a transient card error would cost the
        // reader their whole shelf to fix something that is not visible to
        // them.
        esp_println::println!("sd: an install is still in flight; retrying next mount");
    }
    Reconciled {
        outcome,
        shelf_readable: true,
        may_mutate: true,
    }
}

#[inline(never)]
pub(crate) fn load_catalog_cache(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
) -> bool {
    let start = Instant::now();
    esp_println::println!("sd: catalog cache load start");
    library.clear_catalog();
    // A valid header (even an empty catalog) counts as loaded; anything else
    // returns false so the caller runs a fresh scan. The *reason* is carried
    // out of the session rather than flattened to a bool inside it, because
    // it decides whether a bench capture is looking at a fault or at the
    // ordinary cold path: a `miss` is what queues `RefreshCatalog`, so a card
    // with no snapshot prints one right before the scan that builds it.
    // `.is_ok()` here reported every outcome — refused read, bad seek, torn
    // file — as that same benign miss.
    let outcome = sd_session::with_root(epd, sd_cs, |root| {
        // Before the shelf is served from a cached catalog: a cache hit
        // skips the scan entirely, so this is the only place an interrupted
        // upload gets reconciled on an ordinary boot.
        //
        // Only on a hit, though. A miss is answered by a scan, which
        // reconciles before it publishes anything, so recovering here too
        // would read the install journal twice on every cold boot — and a
        // cold boot is the normal state after an upload session, which
        // invalidates the snapshot on the way in. Nothing is served
        // unreconciled either way: a miss leaves the catalog cleared until
        // that scan.
        //
        // A catalog written *before* this recovery may name a book the swap
        // has since moved to a different alias, so a record in flight retires
        // the catalog just read rather than trusting it; the rescan that
        // follows rebuilds it against the shelf as it now stands.
        let loaded = read_catalog_window(root, library, 0);
        if loaded.is_ok() {
            let reconciled = reconcile_interrupted_uploads(root);
            // Recovery that could not finish says nothing about what the
            // shelf holds, and a snapshot is worth no more than the recovery
            // that proved it current. The scan refuses to rebuild on these
            // same two conditions, so without them here a cache hit is the
            // one path that serves a shelf recovery declined to vouch for.
            //
            // The refused replacement is the case that needs saying. Its
            // `INSTALL.JNL` has already cleared, so `had_intent` below is
            // false and the install reads as settled, while the ledger has
            // yet to learn which bytes stand at the locator and the catalog
            // row still carries the predecessor's identity.
            if !reconciled.shelf_readable || !reconciled.may_mutate {
                library.clear_catalog();
                return Err(CatalogFault::Unreconciled);
            }
            let recovery = reconciled.outcome;
            // Not just what this pass changed. An install whose shelf-changing
            // steps happened before the reset leaves this pass with only a
            // rollback copy to reclaim -- nothing in /BOOKS changes now, but
            // /BOOKS already stopped matching the catalog when the book was
            // installed under its new alias. The record's existence is the
            // evidence; what this pass had left to do is not.
            if recovery.touched_shelf || recovery.had_intent {
                // The window this catalog describes was read into the library
                // a moment ago. Drop it rather than leave a stale shelf
                // resident for anything that reads before the rescan.
                library.clear_catalog();
                return Err(CatalogFault::Reclaimed);
            }
        }
        loaded
    });
    let result = match outcome {
        Ok(Ok(())) => "hit",
        Ok(Err(fault)) => fault.bench_result(),
        // The session itself never opened, so no catalog read was attempted.
        Err(_) => CatalogFault::Device.bench_result(),
    };
    let loaded = matches!(outcome, Ok(Ok(())));
    library.status = if !loaded {
        LibraryScanStatus::NotScanned
    } else if library.catalog_is_empty() {
        LibraryScanStatus::Empty
    } else {
        LibraryScanStatus::Ready
    };
    if loaded {
        esp_println::println!(
            "sd: catalog cache loaded {} epub(s)",
            library.catalog_count()
        );
    } else {
        esp_println::println!("sd: catalog cache unavailable");
    }
    // `ok` keeps its original meaning — the snapshot loaded — so captures
    // that predate `result` still read the same way.
    bench_log!(
        "bench: storage_catalog action=load ok={} result={} count={} elapsed_ms={} t_ms={}",
        loaded,
        result,
        library.catalog_count(),
        start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    loaded
}

/// Walk both book locations (card root and `/BOOKS`), invoking `visit` with
/// each EPUB's `(display_path, root, locator, alias, byte_size)`.
fn walk_epubs<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    visit: &mut impl FnMut(&str, BookRoot, &str, &str, u32),
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    collect_epubs(root, "/", BookRoot::CardRoot, visit)?;
    // Only a genuinely absent library is an empty result; any other failure
    // (SD/FAT flakiness, or a shelf that resolved and then would not open)
    // must fail the walk, or the scan would commit a catalog missing every
    // shelved book and the orphan sweep would reclaim their caches.
    //
    // The shelf is walked depth first to any legal locator depth; the card
    // root deliberately stays flat, since nothing nests there by contract.
    match upload_store::library::open_library_root(root) {
        Ok(Some(books)) => {
            upload_store::library::for_each_book_depth_first(books, &mut |path, alias, size| {
                visit_located(BookRoot::Library, "/books/", path, alias, size, visit);
            })
            .map_err(|_| ())?;
        }
        Ok(None) => {}
        Err(_) => return Err(()),
    }
    Ok(())
}

/// Seed and per-entry step of an order-sensitive FNV-1a over every field
/// `walk_epubs` reports, with a NUL between fields so adjacent strings can't
/// alias. Two walks fold to the same value only when they visit the same
/// books, with the same sizes, in the same order.
const WALK_FINGERPRINT_SEED: u32 = 0x811c_9dc5;

fn fold_walk_entry(
    hash: &mut u32,
    path: &str,
    at: BookRoot,
    locator: &str,
    alias: &str,
    byte_size: u32,
) {
    for byte in path
        .bytes()
        .chain(core::iter::once(0))
        .chain(core::iter::once(at as u8))
        .chain(locator.bytes())
        .chain(core::iter::once(0))
        .chain(alias.bytes())
        .chain(core::iter::once(0))
        .chain(byte_size.to_le_bytes())
    {
        *hash ^= byte as u32;
        *hash = hash.wrapping_mul(0x0100_0193);
    }
}

/// Write CATALOG.BIN from the card without ever holding the whole library in
/// RAM. embedded-sdmmc locks the volume across a directory walk, so records
/// cannot be written mid-iteration; instead one walk counts the books (for the
/// header), then each later walk stages the next batch of records into the
/// caller's scratch region and appends them once the walk has returned. Each
/// staged record also gets its display title (the EPUB title cached at last
/// open, or the upload label) resolved once here, so Library window reads
/// never probe per-book files again. Returns the book count actually written.
///
/// Every batch is another complete walk, and every walk re-reads the whole
/// tree, so the scan costs one counting walk plus one walk per batch. A
/// record is 435 bytes since it started carrying a locator, a widened
/// alias and a book id, so the idle 16 KB section arena stages 37 of them
/// per pass: ordinary libraries still take two walks, a 1,000-book one
/// takes 29. That is the price of nesting until a
/// derived index earns its place. The walk is depth first over the shelf
/// now, so each pass descends the tree as well as re-reading it, and
/// finding each next subfolder re-iterates its parent; a directory with `s`
/// subfolders is read `s + 1` times per pass. That multiplier is the number
/// to watch before reaching for the derived index.
fn write_catalog_streaming<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut [u8],
    ledger: Option<upload_store::ledger::Ledger>,
) -> Result<u16, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let batch_capacity = scratch.len() / CATALOG_RECORD_BYTES;
    if batch_capacity == 0 {
        return Err(());
    }
    let cache_root = open_or_make_dir(root, CATALOG_ROOT_DIR)?;
    let file = cache_root
        .open_file_in_dir(CATALOG_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| ())?;

    let mut counted = 0usize;
    let mut counted_fingerprint = WALK_FINGERPRINT_SEED;
    {
        let mut count = |path: &str, at: BookRoot, locator: &str, alias: &str, byte_size: u32| {
            counted += 1;
            fold_walk_entry(
                &mut counted_fingerprint,
                path,
                at,
                locator,
                alias,
                byte_size,
            );
        };
        walk_epubs(root, &mut count)?;
    }
    // A card holding more books than the header can count is a failed scan,
    // not a shortened one. Everything downstream reads a committed catalog as
    // the whole book set: the list stops at the count, and the orphan sweep
    // reclaims every cache whose identity is missing from it.
    let Some(count) = catalog_count(counted) else {
        esp_println::println!(
            "sd: {} epub(s) is past the {} this catalog can hold",
            counted,
            proto::catalog::CATALOG_MAX_BOOKS
        );
        return Err(());
    };

    // Keep the header deliberately invalid while records are being written;
    // the real version and count are committed only after every directory
    // pass succeeded, so an interrupted scan leaves an unloadable catalog
    // (and a rescan) instead of a truncated library.
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    encode_catalog_placeholder_header(&mut header);
    file.write(&header).map_err(|_| ())?;

    let total = count as usize;
    let mut cursor = 0usize;
    while cursor < total {
        let mut batch_len = 0usize;
        let mut seen = 0usize;
        let mut fingerprint = WALK_FINGERPRINT_SEED;
        {
            let mut collect =
                |path: &str, at: BookRoot, locator: &str, alias: &str, byte_size: u32| {
                    fold_walk_entry(&mut fingerprint, path, at, locator, alias, byte_size);
                    if seen >= cursor && batch_len < batch_capacity {
                        let offset = batch_len * CATALOG_RECORD_BYTES;
                        let record: &mut [u8; CATALOG_RECORD_BYTES] = (&mut scratch
                            [offset..offset + CATALOG_RECORD_BYTES])
                            .try_into()
                            .expect("record slice is exactly one record");
                        encode_catalog_record(
                            record,
                            path,
                            at,
                            locator,
                            "",
                            alias,
                            byte_size,
                            proto::cache::source_hash_at(at, locator, byte_size),
                        );
                        batch_len += 1;
                    }
                    seen += 1;
                };
            walk_epubs(root, &mut collect)?;
        }
        // Every pass over a card that hasn't changed must reproduce the
        // counting walk exactly -- same books, same order. A differing count
        // or fingerprint means the card was ejected, mutated (books added,
        // removed, or replaced between walks), or the directory chain is
        // failing mid-scan; records staged across passes would then mix
        // snapshots, and the file must not be committed.
        let expected = batch_capacity.min(total - cursor);
        if batch_len != expected || seen != counted || fingerprint != counted_fingerprint {
            return Err(());
        }
        // The walk has returned, so file opens are legal again: resolve each
        // staged record's title (cheap for the common case -- a dir open that
        // fails before any file read) and patch it in before the append.
        for index in 0..batch_len {
            let at = index * CATALOG_RECORD_BYTES;
            let record: &[u8; CATALOG_RECORD_BYTES] = (&scratch[at..at + CATALOG_RECORD_BYTES])
                .try_into()
                .expect("record slice is exactly one record");
            let decoded = decode_catalog_record(record);
            let mut title = String::<64>::new();
            if cached_title_label(root, &decoded, &mut title).is_some() {
                let mut field = [0u8; CATALOG_TITLE_BYTES];
                encode_catalog_title(title.as_str(), &mut field);
                scratch[at + CATALOG_RECORD_TITLE_OFFSET
                    ..at + CATALOG_RECORD_TITLE_OFFSET + CATALOG_TITLE_BYTES]
                    .copy_from_slice(&field);
            }
        }
        file.write(&scratch[..batch_len * CATALOG_RECORD_BYTES])
            .map_err(|_| ())?;
        cursor += batch_len;
    }
    if cursor != total {
        return Err(());
    }
    // Every row is on the card; now say which copy each one is. Rows a
    // ledger record names by place keep that record's id, the rest are
    // minted one, and the ledger's new generation is committed before the
    // catalog's header is, so no committed row carries an id the ledger
    // could lose. A refusal leaves the placeholder header in place, which is
    // a rescan next mount, the same as any other interrupted scan.
    let identity_start = Instant::now();
    let rng = esp_hal::rng::Rng::new();
    // A copy found again in a new place keeps its id, and the place the
    // reader left it at is filed under where it used to be, so the two are
    // brought together here. Reported before the ledger is written, so a
    // reset between the two leaves a card whose next scan reports the same
    // move and carries the same place again.
    let mut carry = |found: &upload_store::ledger::FoundAgain<'_>| {
        let was_key = proto::cache::cache_key_from(proto::cache::source_hash_at(
            found.was.0,
            found.was.1,
            found.was.2,
        ));
        let now_key = proto::cache::cache_key_from(proto::cache::source_hash_at(
            found.now.0,
            found.now.1,
            found.now.2,
        ));
        let was = proto::cache::CacheOwner {
            key: was_key.as_str(),
            root: found.was.0,
            locator: found.was.1,
        };
        let now = proto::cache::CacheOwner {
            key: now_key.as_str(),
            root: found.now.0,
            locator: found.now.1,
        };
        match reader_cache::files::carry_position_for_move(root, &was, &now) {
            Ok(true) => esp_println::println!("sd: carried a reading place to '{}'", found.now.1),
            Ok(false) => {}
            // A place that could not be carried is a place lost, not a scan
            // that failed: the copy has its id back either way, and the
            // book opens at its beginning rather than not at all. The one
            // case this bridge does not cover, and deliberately: failing
            // the scan would let a card that cannot write a cache stop the
            // library being rebuilt. It goes when positions hang from the
            // id and a repaired locator keeps the place with nothing to
            // copy.
            Err(_) => {
                esp_println::println!("sd: could not carry a reading place to '{}'", found.now.1)
            }
        }
    };
    let assigned = upload_store::ledger::assign_book_ids(
        root,
        &file,
        count,
        scratch,
        &mut || rng.random(),
        ledger,
        &mut carry,
    )
    .map_err(|fault| {
        esp_println::println!("sd: library ledger refused: {:?}", fault);
    })?;
    if assigned.minted > 0 {
        esp_println::println!("sd: adopted {} new book(s)", assigned.minted);
    }
    if assigned.retired > 0 {
        esp_println::println!(
            "sd: let go of {} book(s) long missing from the card",
            assigned.retired
        );
    }
    if assigned.duplicates > 0 {
        esp_println::println!(
            "sd: the ledger names {} row(s) more than once",
            assigned.duplicates
        );
    }
    if assigned.repaired > 0 {
        esp_println::println!(
            "sd: found {} book(s) again in a new place",
            assigned.repaired
        );
    }
    if assigned.ambiguous > 0 {
        esp_println::println!(
            "sd: {} book(s) could be more than one copy, so their places were left alone",
            assigned.ambiguous
        );
    }
    if assigned.unreadable > 0 {
        esp_println::println!(
            "sd: {} book(s) were left alone because a file of their length could not be read",
            assigned.unreadable
        );
    }
    bench_log!(
        "bench: storage_ledger action=assign matched={} minted={} missing={} retired={} duplicates={} repaired={} hashed={} ambiguous={} unreadable={} elapsed_ms={} t_ms={}",
        assigned.matched,
        assigned.minted,
        assigned.missing,
        assigned.retired,
        assigned.duplicates,
        assigned.repaired,
        assigned.hashed,
        assigned.ambiguous,
        assigned.unreadable,
        identity_start.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    encode_catalog_header(count, &mut header);
    file.seek_from_start(0).map_err(|_| ())?;
    file.write(&header).map_err(|_| ())?;
    Ok(count)
}

/// Open CATALOG.BIN read-only, validate its header, and hand the file plus its
/// book count to `f`. Keeps the directory and file handles alive across the
/// call so the borrowed `File` stays valid.
fn with_catalog_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
    R,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    f: impl FnOnce(&File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, u16) -> Result<R, CatalogFault>,
) -> Result<R, CatalogFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // `NotFound` is a card with no catalog yet; anything else from the same
    // call is the card refusing, and the two must not report alike.
    let cache_root = root.open_dir(CATALOG_ROOT_DIR).map_err(open_fault)?;
    let file = cache_root
        .open_file_in_dir(CATALOG_FILE, Mode::ReadOnly)
        .map_err(open_fault)?;
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    read_exact_file(&file, &mut header)?;
    // An older format is the migration doing its job; only a header that is
    // not a catalog, or the placeholder a torn scan leaves, is a fault.
    let count = proto::catalog::classify_catalog_header(&header).map_err(|fault| match fault {
        proto::catalog::CatalogHeaderFault::Stale => CatalogFault::Stale,
        proto::catalog::CatalogHeaderFault::Invalid => CatalogFault::Invalid,
    })?;
    // A committed header whose count disagrees with the file length means
    // the file was truncated or appended outside the writer's control;
    // nothing downstream may trust its record offsets.
    if file.length() as usize != catalog_file_len(count) {
        return Err(CatalogFault::Invalid);
    }
    f(&file, count)
}

/// Load the list window `[start, start+LIBRARY_WINDOW)` from the card into the
/// store, and set the total book count from the header. O(1) seek to the start
/// record -- no scan.
fn read_catalog_window<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &mut ReaderStore,
    start: usize,
) -> Result<(), CatalogFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_catalog_file(root, |file, count| {
        library.set_catalog_total(count);
        library.begin_window(start);
        if start >= count as usize {
            return Ok(());
        }
        seek_to_record(file, start)?;
        let take = LIBRARY_WINDOW.min(count as usize - start);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for _ in 0..take {
            read_exact_file(file, &mut record)?;
            let decoded = decode_catalog_record(&record);
            // Prefer the title persisted in the record (resolved at scan,
            // refreshed at book open) over the file-stem label, so uploaded
            // books (8.3 names) read as their real titles. An empty title
            // falls back to the stem. No per-row file probes: window
            // crossings cost exactly the record reads.
            let label = (!decoded.title.is_empty()).then_some(decoded.title.as_str());
            library.push_window_entry(
                decoded.display_name.as_str(),
                decoded.byte_size,
                decoded.source_hash,
                label,
            );
        }
        Ok(())
    })
}

/// Read a single catalog record by absolute index.
pub(crate) fn read_catalog_record_at<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
) -> Option<CatalogRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_catalog_file(root, |file, count| {
        if index >= count as usize {
            return Err(CatalogFault::Invalid);
        }
        seek_to_record(file, index)?;
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        read_exact_file(file, &mut record)?;
        Ok(decode_catalog_record(&record))
    })
    .ok()
}

/// Find the catalog index of the book with the given (path-hash, byte-size).
///
/// One streamed pass with the one-match rule: the identity is a 32-bit
/// hash and two legal books can share it, so a hinted or first match could
/// be the other one. There is no hint fast path for the same reason, since
/// ruling out a second match requires reading every record anyway.
fn find_in_catalog<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_hash: u32,
    byte_size: u32,
) -> Option<u16>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if source_hash == 0 && byte_size == 0 {
        return None;
    }
    with_catalog_file(root, |file, count| {
        seek_to_record(file, 0)?;
        let mut scan = proto::catalog::IdentityScan::new(source_hash, byte_size);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for index in 0..count as usize {
            read_exact_file(file, &mut record)?;
            scan.offer(index as u16, &record);
        }
        Ok(scan.finish())
    })
    .ok()
    .flatten()
}

/// Find the catalog index of the book at an exact place on the card.
///
/// One streamed pass, like the identity lookups, because ruling out a second
/// match means reading every record either way. What differs is what is
/// being matched: a place, which is unique on a filesystem, rather than 32
/// bits derived from one.
fn find_in_catalog_at<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    at: BookRoot,
    locator: &str,
    byte_size: u32,
) -> CatalogRow
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match with_catalog_file(root, |file, count| {
        seek_to_record(file, 0)?;
        let mut scan = proto::catalog::LocatorScan::new(at, locator, byte_size);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for index in 0..count {
            read_exact_file(file, &mut record)?;
            scan.offer(index, &record);
        }
        Ok(scan.finish())
    }) {
        Ok(Some(index)) => CatalogRow::Found(index),
        Ok(None) => CatalogRow::Rebuild,
        // A snapshot that is absent, outdated, damaged, or invalidated by
        // recovery cannot answer until it is rebuilt, which is the same
        // repair a stale one needs. Only a refused read is unknown.
        Err(CatalogFault::Device) => CatalogRow::Unreadable,
        Err(_) => CatalogRow::Rebuild,
    }
}

/// Find the catalog index of the book a pre-v8 `(path-hash, byte-size)`
/// names, by reconstructing each record's frozen legacy identity from its
/// stored display name. Exactly one record may answer; several is a guess
/// between real books, and the scan refuses it.
///
/// Only saved-state restoration reads identities this way. The orphan sweep
/// must not: a re-keyed old cache directory carries the old hash in its
/// header, and a sweep that matched it against legacy identities would keep
/// every old directory alive forever instead of reclaiming it.
fn find_in_catalog_by_legacy_identity<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_hash: u32,
    byte_size: u32,
) -> Option<u16>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if source_hash == 0 && byte_size == 0 {
        return None;
    }
    with_catalog_file(root, |file, count| {
        seek_to_record(file, 0)?;
        let mut scan = proto::catalog::LegacyIdentityScan::new(source_hash, byte_size);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for index in 0..count as usize {
            read_exact_file(file, &mut record)?;
            scan.offer(index as u16, &record);
        }
        Ok(scan.finish())
    })
    .ok()
    .flatten()
}

fn record_identity<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
) -> Result<(u32, u32), CatalogFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    seek_to_record(file, index)?;
    let mut record = [0u8; CATALOG_RECORD_BYTES];
    read_exact_file(file, &mut record)?;
    Ok(catalog_record_identity(&record))
}

fn seek_to_record<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
) -> Result<(), CatalogFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let offset = (CATALOG_HEADER_BYTES + index * CATALOG_RECORD_BYTES) as u32;
    file.seek_from_start(offset)
        .map_err(|_| CatalogFault::Device)
}

/// The list label override for a catalog record, read into `title` in place,
/// in order of authority: the EPUB title saved in the book's cache when it was
/// last opened, then the readable filename stashed at upload (for uploads not
/// yet opened, whose 8.3 name is unreadable), which only answers for the flat
/// positions that scheme could write and so passes the record's own position
/// along. Returns `None` (file-stem
/// fallback) when neither exists. Resolved once per book at scan time -- the
/// result persists in the record's title field, so window reads never call
/// this. Cheap for the common case -- each lookup is a dir open that fails
/// before any file read.
fn cached_title_label<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    decoded: &CatalogRecord,
    title: &'a mut String<64>,
) -> Option<&'a str>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let key = proto::cache::cache_key_from(decoded.source_hash);
    let mut raw_name = String::<64>::new();
    // A record whose root byte this build does not know has no provable
    // cache directory; its label falls back like any other miss.
    let cached_title = decoded.root.is_some_and(|at| {
        let owner = proto::cache::CacheOwner {
            key: key.as_str(),
            root: at,
            locator: decoded.path.as_str(),
        };
        reader_cache::files::read_cached_book_title(
            root,
            &owner,
            (decoded.source_hash, decoded.byte_size),
            title,
        )
    });
    if cached_title {
        return Some(title.as_str());
    }
    // Same unknown root, same answer: a record this build cannot place has no
    // position to hand the label reader, and the label is filed under an alias
    // that only means anything together with one.
    let labelled = decoded.root.is_some_and(|at| {
        upload_store::read_upload_label(
            root,
            at,
            decoded.path.as_str(),
            decoded.upload_alias.as_str(),
            &mut raw_name,
        )
    });
    if labelled {
        reader_cache::store::derive_catalog_label(raw_name.as_str(), title);
        Some(title.as_str())
    } else {
        None
    }
}

pub(crate) use reader_cache::browse::Listing;

/// What the row a reader pressed turned out to be, once the catalog has had
/// its say about a book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowChoice {
    /// A folder, now listed.
    Entered(Listing),
    /// A book, at this catalog row.
    Book(u16),
    /// The row named a book the card holds and the catalog does not, which
    /// is a catalog written before somebody edited the card on a computer.
    /// The caller rescans and asks again.
    Stale,
    /// Gone since the listing, unnameable from here, or a card that would
    /// not answer. Nothing moved.
    Failed,
}

/// Refill the resident folder page so it covers the visible rows around
/// `selection`, reading from the card only when the page does not already
/// cover them. Windowed the way the catalog snapshot is, and called before
/// each Library render.
#[inline(never)]
pub(crate) fn ensure_folder_page(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    selection: u16,
    portrait: bool,
) {
    let started = Instant::now();
    let read = sd_session::with_root(epd, sd_cs, |root| {
        reader_cache::browse::ensure_page(library, root, selection, portrait)
    })
    .unwrap_or(false);
    // Only the crossings. A scroll inside the loaded page reads nothing, and
    // logging those would bury the number this is here to show under one line
    // per Library paint.
    if read {
        bench_log!(
            "bench: folder_page rows={} start={} depth={} ms={} t_ms={}",
            library.browse().count(),
            library.folder_start(),
            library.browse().path().depth(),
            started.elapsed().as_millis(),
            Instant::now().as_millis(),
        );
    }
}

/// Act on the Library row at `index`: enter a folder, or find the catalog row
/// that opens a book.
///
/// A book is resolved by where it is, through [`find_index_by_locator`].
/// Position would be cheaper and wrong: it ties two independent walks'
/// orderings together as a correctness requirement, and a card edited
/// between them opens some other book. The 32-bit identity derived from the
/// place would be wrong too, since two legal locators at one size can
/// collide in it and a reader who picked a row has already said which book.
#[inline(never)]
pub(crate) fn choose_library_row(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: u16,
    portrait: bool,
) -> RowChoice {
    let started = Instant::now();
    #[cfg(feature = "bench-selftest")]
    let _ = upload_store::library::walk_probe::take();
    let chosen = sd_session::with_root(epd, sd_cs, |root| {
        reader_cache::browse::choose_row(library, root, index, portrait)
    });
    match chosen {
        Ok(reader_cache::browse::RowChoice::Entered(listing)) => {
            // Entering is the number the folder-size question is about: it
            // counts the whole directory before it can page it, so this grows
            // with the folder while `folder_page` does not.
            bench_log!(
                "bench: folder_enter rows={} books={} depth={} ms={} t_ms={}",
                listing.count,
                listing.books,
                listing.depth,
                started.elapsed().as_millis(),
                Instant::now().as_millis(),
            );
            log_folder_walks();
            RowChoice::Entered(listing)
        }
        Ok(reader_cache::browse::RowChoice::Book { at, locator, size }) => {
            match find_index_by_locator(epd, sd_cs, at, locator.as_str(), size) {
                CatalogRow::Found(index) => RowChoice::Book(index),
                CatalogRow::Unreadable => {
                    // The card would not answer about the catalog. That is
                    // not evidence the catalog is behind the card, and
                    // rebuilding on it would retire a usable snapshot
                    // because one read failed.
                    esp_println::println!(
                        "library: catalog would not answer for {}",
                        locator.as_str()
                    );
                    RowChoice::Failed
                }
                CatalogRow::Rebuild => {
                    // The card lists this book and the catalog does not, so
                    // the catalog is older than the card: boot keeps a
                    // snapshot that still loads, and a computer can add or
                    // move books while the device is off. Browsing walks the
                    // card and finds them; only the catalog has to catch up.
                    esp_println::println!(
                        "library: {} at {} bytes is not in the catalog, which is stale",
                        locator.as_str(),
                        size
                    );
                    RowChoice::Stale
                }
            }
        }
        Ok(reader_cache::browse::RowChoice::Failed) | Err(_) => RowChoice::Failed,
    }
}

/// Go up one folder and list the parent, landing back on the folder just
/// left: by name where the parent still holds it, else on the row it was
/// entered from.
#[inline(never)]
pub(crate) fn leave_library_folder(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    portrait: bool,
) -> Option<Listing> {
    let started = Instant::now();
    #[cfg(feature = "bench-selftest")]
    let _ = upload_store::library::walk_probe::take();
    let listed = sd_session::with_root(epd, sd_cs, |root| {
        reader_cache::browse::leave_folder(library, root, portrait)
    })
    .ok()
    .flatten();
    // Leaving walks the whole parent past the returning name before it
    // commits, so this is the other end of the folder-size question: it grows
    // with the parent rather than with the folder being left.
    bench_log!(
        "bench: folder_leave rows={} depth={} ok={} ms={} t_ms={}",
        listed.map_or(0, |listing| listing.count),
        listed.map_or(0, |listing| listing.depth),
        listed.is_some(),
        started.elapsed().as_millis(),
        Instant::now().as_millis(),
    );
    log_folder_walks();
    listed
}

/// Split the folder operation just logged into its walks, bench builds only.
///
/// `folder_enter` and `folder_leave` report one elapsed time for what is
/// really several directory walks, each of which first scans the parent to
/// resolve its path and then iterates the folder. The two phases scale with
/// different things, so a slow folder cannot be attributed from the total.
/// This line says how many walks ran and how many entries each phase
/// visited; the counters are per operation, reset before the walk above.
#[cfg(feature = "bench-selftest")]
fn log_folder_walks() {
    let (walks, resolve_entries, iterate_entries) = upload_store::library::walk_probe::take();
    bench_log!(
        "bench: folder_walks walks={} resolve_entries={} iterate_entries={} t_ms={}",
        walks,
        resolve_entries,
        iterate_entries,
        Instant::now().as_millis(),
    );
}

#[cfg(not(feature = "bench-selftest"))]
fn log_folder_walks() {}

/// Make `index` the active book by reading its catalog record into the store,
/// so the reading path's `catalog_entry(index)` resolves without depending on
/// the list window. Idempotent when already active.
#[inline(never)]
pub(crate) fn load_active_entry(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
) -> bool {
    if library.active_index() == Some(index) {
        return true;
    }
    // The record carries the title persisted at scan/open, so the active
    // book's fallback label (Home colophon before a reopen) matches what the
    // list shows without any per-book cache probe.
    let resolved = sd_session::with_root(epd, sd_cs, |root| read_catalog_record_at(root, index))
        .ok()
        .flatten();
    match resolved {
        Some(record) => {
            library.set_active_entry(
                index,
                record.display_name.as_str(),
                record.root,
                record.path.as_str(),
                record.byte_size,
                record.source_hash,
                (!record.title.is_empty()).then_some(record.title.as_str()),
                record.book_id,
            );
            true
        }
        None => false,
    }
}

/// One catalog row's cache key and source identity, read straight off the
/// card.
///
/// Unlike [`load_active_entry`] this does not publish the row as the active
/// entry. The active entry is the *open* book's, held apart from the list
/// window precisely so the reading path does not depend on where the list is
/// scrolled — `source_identity` resolves the position record's identity
/// through it, and the Home colophon its label. A cache clear on some other
/// row has no business evicting that, so it reads what it needs and leaves
/// the store alone.
#[inline(never)]
pub(crate) fn read_row_cache_identity(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    index: usize,
) -> Option<(String<{ proto::cache::CACHE_KEY_BYTES }>, u32, u32)> {
    let record = sd_session::with_root(epd, sd_cs, |root| read_catalog_record_at(root, index))
        .ok()
        .flatten()?;
    Some((
        proto::cache::cache_key_from(record.source_hash),
        record.source_hash,
        record.byte_size,
    ))
}

/// Persist a book's just-learned title into its catalog record, so the next
/// window read (and the next boot's catalog cache) label it without probing
/// the book's cache. Runs inside the open's existing SD session. The record
/// at `index` is patched only when its identity still matches (the catalog
/// may have been rewritten under a stale index -- then the identity resolves
/// the true index) and only when the stored title actually differs, so the
/// common reopen costs one record read and no write.
pub(crate) fn update_catalog_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    index: usize,
    source_identity: (u32, u32),
    title: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if title.is_empty() {
        return false;
    }
    let Ok(cache_root) = root.open_dir(CATALOG_ROOT_DIR) else {
        return false;
    };
    let Ok(file) = cache_root.open_file_in_dir(CATALOG_FILE, Mode::ReadWriteAppend) else {
        return false;
    };
    if file.seek_from_start(0).is_err() {
        return false;
    }
    let mut header = [0u8; CATALOG_HEADER_BYTES];
    if read_exact_file(&file, &mut header).is_err() {
        return false;
    }
    let Some(count) = proto::catalog::decode_catalog_header(&header) else {
        return false;
    };
    // Trust the caller's index only if the record identity still matches:
    // the index names the row this session actually opened, so a twin
    // sharing the identity elsewhere in the catalog cannot mislead it.
    // Otherwise resolve by identity on the already-open file (one streamed
    // pass, rare -- the catalog was rewritten under a stale index), where
    // the one-match rule applies: with two rows answering, patching either
    // could retitle the other book.
    let hinted = index < count as usize
        && record_identity(&file, index)
            .map(|identity| identity == source_identity)
            .unwrap_or(false);
    let target = if hinted {
        index
    } else {
        if seek_to_record(&file, 0).is_err() {
            return false;
        }
        let mut scan = proto::catalog::IdentityScan::new(source_identity.0, source_identity.1);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for candidate in 0..count as usize {
            if read_exact_file(&file, &mut record).is_err() {
                return false;
            }
            scan.offer(candidate as u16, &record);
        }
        let Some(found) = scan.finish() else {
            return false;
        };
        found as usize
    };
    let mut field = [0u8; CATALOG_TITLE_BYTES];
    encode_catalog_title(title, &mut field);
    let title_offset =
        (CATALOG_HEADER_BYTES + target * CATALOG_RECORD_BYTES + CATALOG_RECORD_TITLE_OFFSET) as u32;
    let mut stored = [0u8; CATALOG_TITLE_BYTES];
    if file.seek_from_start(title_offset).is_err() || read_exact_file(&file, &mut stored).is_err() {
        return false;
    }
    if stored == field {
        return true;
    }
    file.seek_from_start(title_offset).is_ok() && file.write(&field).is_ok()
}

/// Resolve a saved (path-hash, byte-size) back to its catalog index, the
/// reverse of `source_identity`.
///
/// `legacy` is the saved record's own claim about which rule produced its
/// hash (`AppStateRecord::legacy_source_identity`), and exactly one reading
/// runs. Not a fallback: the two hash domains can collide on the same 32
/// bits, so trying the current reading first could resolve a pre-v8
/// identity to an unrelated nested book and then persist that mistake at
/// the next save. A legacy resolution stops being needed after one save,
/// since saves derive a fresh identity from the active entry.
///
/// Both readings stream the whole catalog and refuse more than one match:
/// the same 32 bits can also collide between two current books, and a
/// hinted or first match could be the other one.
#[inline(never)]
pub(crate) fn find_index_by_identity(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    source_hash: u32,
    byte_size: u32,
    legacy: bool,
) -> Option<u16> {
    sd_session::with_root(epd, sd_cs, |root| {
        if legacy {
            find_in_catalog_by_legacy_identity(root, source_hash, byte_size)
        } else {
            find_in_catalog(root, source_hash, byte_size)
        }
    })
    .ok()
    .flatten()
}

/// The catalog row for a book a reader just picked out of a listing, which
/// named its exact place.
///
/// Deliberately not [`find_index_by_identity`]. That lookup is for state
/// whose only persisted identity is `(hash, size)`, and it must refuse when
/// two records share those bits. A picked row is not ambiguous, and putting
/// it through the identity would manufacture the ambiguity and then fail on
/// it, leaving two perfectly good books unopenable.
#[inline(never)]
pub(crate) fn find_index_by_locator(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    at: BookRoot,
    locator: &str,
    byte_size: u32,
) -> CatalogRow {
    // A session that would not open is a card that did not answer, which is
    // the same unknown as a refused read.
    sd_session::with_root(epd, sd_cs, |root| {
        find_in_catalog_at(root, at, locator, byte_size)
    })
    .unwrap_or(CatalogRow::Unreadable)
}

/// Empty every book cache under CACHE2 whose book is no longer in the freshly
/// written catalog -- the orphans left when a book is deleted (through the shelf
/// or by pulling the card). Each cache is matched by its stored source identity,
/// not its key name, so a live book's cache is never swept. Reading position is
/// never swept either: `empty_cache_dir` leaves POS*.BIN and the directory
/// holding it, so a book that leaves the card and comes back resumes where it
/// was. The walk carries a cursor across batches, so one pass reaches every
/// cache directory rather than the first few dozen.
///
/// The catalog's identities (8 B each) load into `scratch` once, sorted, so
/// each cache dir checks membership with an in-RAM binary search --
/// O((N + C) log N) rather than streaming the whole catalog off the card per
/// cache dir. Should the catalog outgrow the scratch (2,048 books against
/// the 16 KB arena), the overflow falls back to the streamed per-cache
/// lookup, keeping the sweep exact.
fn sweep_orphan_caches<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut [u8],
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let (staged, truncated) = stage_catalog_identities(root, scratch);
    let mut swept = 0u32;
    // The walk carries its own cursor, so a card with more caches than one
    // batch holds still reaches all of them. It used to stop after the first
    // batch and start from the top next scan, which meant a moved book's
    // directory sitting past the first few dozen was reconciled on no scan
    // at all.
    reader_cache::files::for_each_cache_dir(root, reader_cache::files::CACHE_SWEEP_BATCH, |keys| {
        sweep_cache_batch(root, keys, scratch, staged, truncated, &mut swept);
    });
    if swept > 0 {
        esp_println::println!("cache: swept {} orphan cache(s)", swept);
    }
}

/// One batch of cache directory names, judged and reclaimed.
///
/// Split out so the walk above holds only names and a cursor: this is where
/// the file opens happen, and they cannot happen inside the iteration that
/// produced the names.
fn sweep_cache_batch<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    keys: &[String<8>],
    scratch: &[u8],
    staged: usize,
    truncated: bool,
    swept: &mut u32,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    for key in keys {
        // A claimed directory is judged by its exact owner, not by the
        // 32-bit identity: a full-hash twin in the catalog would vouch for
        // a departed owner's directory forever, stranding the surviving
        // twin behind a claim nobody can adopt. The filesystem is the
        // source of truth for the owner existing: the file is there and
        // still keys here, or it does not.
        match reader_cache::files::read_book_dir_claimant(root, key.as_str()) {
            reader_cache::files::DirClaimant::Claimed {
                root: at,
                locator,
                released,
                ..
            } => {
                match reader_cache::files::claimant_place(root, at, locator.as_str(), key.as_str())
                {
                    reader_cache::files::ClaimantPlace::Live => continue,
                    // Leave the directory exactly as it is, the way an
                    // unreadable claim does. Nothing here is evidence that
                    // its owner went anywhere, and retiring a claim is what
                    // ends a book's cache and gives up its place.
                    reader_cache::files::ClaimantPlace::Unreadable => continue,
                    // The card answered and the file is not there. This
                    // milestone stops here: whether the book was deleted or
                    // moved is a question about which copy a file is, and
                    // nothing available can answer it, so the cache retires
                    // and the position stays in its directory waiting for
                    // the book to come back to where it was.
                    reader_cache::files::ClaimantPlace::Gone => {}
                }
                // The owner is gone. Release the claim rather than delete
                // it, so the evidence keeps naming the owner: a return
                // resumes the surviving position, and any twin adopting the
                // key knows the place is not its own. Then reclaim the
                // rebuildables; the release keeps the claim through the
                // clear because the positions survive it.
                if !released && !reader_cache::files::release_book_dir_claim(root, key.as_str()) {
                    continue;
                }
                let _ = reader_cache::files::empty_cache_dir(root, key.as_str());
                *swept += 1;
            }
            reader_cache::files::DirClaimant::Unclaimed => {
                // Pre-claim compatibility: judged by identity, as always. A
                // readable cache that still maps to a catalog book stays;
                // anything else is reclaimed. The sweep runs against a
                // catalog it has just proven fresh, and a cache it cannot
                // match to any book on the card is garbage whoever wrote
                // it. Keeping it would strand the shells forever.
                let live = match reader_cache::files::read_cache_header(root, key.as_str()) {
                    reader_cache::files::CacheHeader::Present(h) => {
                        catalog_identity_staged(scratch, staged, h.source_hash, h.source_size)
                            || (truncated
                                && find_in_catalog(root, h.source_hash, h.source_size).is_some())
                    }
                    reader_cache::files::CacheHeader::Absent
                    | reader_cache::files::CacheHeader::Unreadable => false,
                };
                if live {
                    continue;
                }
                // An unreadable header is exactly the case that used to
                // defeat the sweep: it named section files from the
                // header's own count, so a cache with no BOOK.BIN kept its
                // sections forever. The delete lists the directory now, so
                // it needs nothing from the header.
                let _ = reader_cache::files::empty_cache_dir(root, key.as_str());
                *swept += 1;
            }
            // Failure to read the claim is not evidence of anything; the
            // directory keeps whatever it has until the card answers.
            reader_cache::files::DirClaimant::Fault => continue,
        }
    }
}

/// Load every catalog record's `(source_hash, byte_size)` into `scratch` in
/// one streamed pass, then sort them for `catalog_identity_staged`'s binary
/// search. Returns `(staged_count, truncated)`; `truncated` also covers an
/// unreadable catalog, so callers keep the streamed fallback and a broken
/// catalog behaves exactly as before.
fn stage_catalog_identities<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    scratch: &mut [u8],
) -> (usize, bool)
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let capacity = scratch.len() / CATALOG_IDENTITY_BYTES;
    let (staged, truncated) = with_catalog_file(root, |file, count| {
        seek_to_record(file, 0)?;
        let take = (count as usize).min(capacity);
        let mut record = [0u8; CATALOG_RECORD_BYTES];
        for index in 0..take {
            read_exact_file(file, &mut record)?;
            let (hash, size) = catalog_record_identity(&record);
            if !stage_catalog_identity(scratch, index, hash, size) {
                return Ok((index, true));
            }
        }
        Ok((take, count as usize > take))
    })
    .unwrap_or((0, true));
    sort_catalog_identities(scratch, staged);
    (staged, truncated)
}

/// The listing's trailing `|byte_size`, which only a campaign build carries.
/// Empty otherwise, so a shipping listing keeps its three fields.
fn listing_size_field(record: &CatalogRecord) -> String<16> {
    #[cfg(feature = "powercut-selftest")]
    {
        use core::fmt::Write as _;
        let mut field = String::new();
        let _ = write!(field, "|{}", record.byte_size);
        field
    }
    #[cfg(not(feature = "powercut-selftest"))]
    {
        let _ = record;
        String::new()
    }
}

/// Stream the whole catalog into the browser shelf buffer as
/// `flag|open_name|label` lines (B = /BOOKS, R = card root). Truncates to the
/// buffer; returns the bytes written.
///
/// A `powercut-selftest` build appends `|byte_size` to each line and, if the
/// buffer ran out, a final `!TRUNCATED|written|total` line. Neither is in a
/// shipping build. The browser destructures only the first three fields, so
/// the extra one is invisible to it; the durability campaign requires both,
/// because a listing it cannot tell is complete is a baseline it cannot
/// prove untouched, and a size is the difference between "a book with this
/// name exists" and "the book that was written is the book that is there".
#[inline(never)]
pub(crate) fn write_catalog_listing(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    out: &mut [u8],
) -> usize {
    sd_session::with_root(epd, sd_cs, |root| {
        with_catalog_file(root, |file, count| {
            seek_to_record(file, 0)?;
            let mut record = [0u8; CATALOG_RECORD_BYTES];
            let mut at = 0usize;
            // Room kept back for the truncation marker, which is only worth
            // writing when there is space left to write it in.
            #[cfg(feature = "powercut-selftest")]
            const TRUNCATION_MARKER_BYTES: usize = 32;
            #[cfg(not(feature = "powercut-selftest"))]
            const TRUNCATION_MARKER_BYTES: usize = 0;
            let budget = out.len().saturating_sub(TRUNCATION_MARKER_BYTES);
            let mut written = 0u16;
            let mut truncated = false;
            for _ in 0..count as usize {
                if read_exact_file(file, &mut record).is_err() {
                    truncated = true;
                    break;
                }
                let decoded = decode_catalog_record(&record);
                // The shelf shows the same label as the Library list: the
                // persisted title when the book has one, else the stem label.
                let mut label = String::<64>::new();
                if decoded.title.is_empty() {
                    derive_catalog_label(decoded.display_name.as_str(), &mut label);
                } else {
                    let _ = label.push_str(decoded.title.as_str());
                }
                // The shelf addresses a book by flag and alias, which can
                // only name the two directories uploads have ever landed in.
                // A book deeper than that has no line in this format, so it
                // is left out rather than given a line naming something else.
                let flag = match decoded
                    .root
                    .and_then(|at| upload_store::shelf_placement(at, decoded.path.as_str()))
                {
                    Some(upload_store::ShelfPlacement::Shelf) => b'B',
                    Some(upload_store::ShelfPlacement::Root) => b'R',
                    None => continue,
                };
                let open_name = decoded.upload_alias.as_bytes();
                let size_field = listing_size_field(&decoded);
                let line_len = 1 + 1 + open_name.len() + 1 + label.len() + size_field.len() + 1;
                if at + line_len > budget {
                    truncated = true;
                    break;
                }
                out[at] = flag;
                at += 1;
                out[at] = b'|';
                at += 1;
                out[at..at + open_name.len()].copy_from_slice(open_name);
                at += open_name.len();
                out[at] = b'|';
                at += 1;
                out[at..at + label.len()].copy_from_slice(label.as_bytes());
                at += label.len();
                out[at..at + size_field.len()].copy_from_slice(size_field.as_bytes());
                at += size_field.len();
                out[at] = b'\n';
                at += 1;
                written += 1;
            }
            #[cfg(feature = "powercut-selftest")]
            if truncated {
                use core::fmt::Write as _;
                let mut marker = String::<32>::new();
                let _ = writeln!(marker, "!TRUNCATED|{}|{}", written, count);
                if at + marker.len() <= out.len() {
                    out[at..at + marker.len()].copy_from_slice(marker.as_bytes());
                    at += marker.len();
                }
            }
            #[cfg(not(feature = "powercut-selftest"))]
            let _ = (written, truncated);
            Ok(at)
        })
    })
    .ok()
    .and_then(Result::ok)
    .unwrap_or(0)
}

pub(crate) fn open_or_make_dir<
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

fn read_exact_file<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut out: &mut [u8],
) -> Result<(), CatalogFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !out.is_empty() {
        // A refused read is the card; a read that returns nothing with bytes
        // still owed is a file shorter than its header claims.
        let read = file.read(out).map_err(|_| CatalogFault::Device)?;
        if read == 0 {
            return Err(CatalogFault::Invalid);
        }
        let tmp = out;
        out = &mut tmp[read..];
    }
    Ok(())
}

fn collect_epubs<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    dir: &embedded_sdmmc::Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    prefix: &str,
    at: BookRoot,
    visit: &mut impl FnMut(&str, BookRoot, &str, &str, u32),
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Sized so no name FAT can store overflows it. An overflowing long name
    // is one this callback never sees, and an AppleDouble sidecar whose long
    // name is lost is indistinguishable from a book: `._<book>.epub` shortens
    // to something like `_BOOK~1.EPU`. `MAX_LFN_UTF8_BYTES` documents the
    // bound. It costs 573 bytes over the 192-byte buffer it replaces, less
    // the 8.3 staging string dropped below: measured on X3, the two
    // instantiations of this function go from 464 bytes each to 1,024 and
    // 976. The scan runs from the storage dispatcher, not under the reader's
    // deep call chain, and both frames stay far inside the 24 KB per-frame
    // budget and the 41,944-byte X3 stack region.
    let mut lfn_storage = [0u8; proto::storage::MAX_LFN_UTF8_BYTES];
    let mut lfn_buffer = LfnBuffer::new(&mut lfn_storage);
    dir.iterate_dir_lfn(&mut lfn_buffer, |entry, long_name| {
        if entry.attributes.is_directory() || entry.attributes.is_volume() {
            return ControlFlow::Continue(());
        }

        // The 8.3 name stays the open handle whichever name is catalogued, so
        // a prefix of one is worse than no entry: it names a different file,
        // or none. Sized so it cannot be one: a short name of accented
        // characters renders to two bytes each, and a buffer that overflowed
        // would drop the book out of the catalog rather than misname it.
        let mut open_name = String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
        use core::fmt::Write;
        if write!(open_name, "{}", entry.name).is_err() {
            return ControlFlow::Continue(());
        }
        let Some(name) = proto::storage::catalog_scan_name(long_name, &open_name) else {
            return ControlFlow::Continue(());
        };
        visit_prefixed(prefix, at, name, &open_name, entry.size, visit);
        ControlFlow::Continue(())
    })
    .map_err(|_| ())
}

fn visit_prefixed(
    prefix: &str,
    at: BookRoot,
    name: &str,
    open_name: &str,
    byte_size: u32,
    visit: &mut impl FnMut(&str, BookRoot, &str, &str, u32),
) {
    let Ok(locator) = proto::library_path::LibraryPath::root().child(name) else {
        return;
    };
    let mut rendered = String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
    let _ = rendered.push_str(open_name);
    visit_located_rendered(at, prefix, &locator, rendered, byte_size, visit);
}

/// Report one located book to the scan: the display label, built over the
/// whole locator so a nested path never passes the legacy position shape
/// gate, then the visit itself.
fn visit_located(
    at: BookRoot,
    prefix: &str,
    locator: &proto::library_path::LibraryPath,
    alias: &embedded_sdmmc::ShortFileName,
    byte_size: u32,
    visit: &mut impl FnMut(&str, BookRoot, &str, &str, u32),
) {
    use core::fmt::Write;
    let mut rendered = String::<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>::new();
    // Cannot overflow: the buffer is sized for the widest rendered alias.
    if write!(rendered, "{}", alias).is_err() {
        return;
    }
    visit_located_rendered(at, prefix, locator, rendered, byte_size, visit);
}

fn visit_located_rendered(
    at: BookRoot,
    prefix: &str,
    locator: &proto::library_path::LibraryPath,
    rendered_alias: String<{ proto::storage::MAX_ALIAS_UTF8_BYTES }>,
    byte_size: u32,
    visit: &mut impl FnMut(&str, BookRoot, &str, &str, u32),
) {
    // The display label covers the whole locator, not just the file name:
    // identity and the cache key hash the root and locator
    // (`proto::cache::source_hash_at`), so the label is free to trim, and a
    // nested label keeps a separator in it, which is what keeps
    // `legacy_position_cache_key` from ever treating a nested book as a
    // pre-v8 flat one.
    let mut path = String::<64>::new();
    proto::storage::catalog_display_path(prefix, locator.as_str(), &mut path);
    visit(
        &path,
        at,
        locator.as_str(),
        rendered_alias.as_str(),
        byte_size,
    );
}
