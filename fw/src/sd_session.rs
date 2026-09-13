use crate::display_flush::Epd;
use crate::upload::{UploadBegin, UploadChunk};
use crate::{
    DISPLAY_COMMANDS, UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_RESULTS, UPLOAD_RETURNS, UPLOAD_STOPPED,
    UPLOAD_STOP_REQUESTS,
};
use app_core::DisplayCommand;
use core::sync::atomic::{AtomicU8, Ordering};
use embassy_futures::select::{select, Either};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::{Operation, SpiBus as BlockingSpiBus, SpiDevice};
use embedded_sdmmc::embedded_sdmmc_types::sdcard::CardType;
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use embedded_sdmmc::{Directory, SdCard, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use esp_hal::gpio::Output;
use esp_hal::spi::master::{Config as SpiConfig, SpiDmaBus};
use esp_hal::time::Rate;
use esp_hal::Async;

/// SD SPI-mode identification must run at 100-400 kHz; data transfer is
/// specced to 25 MHz. The shared bus otherwise runs at the active panel's
/// clock (historically the X4's 40 MHz — out of SD spec entirely, and what
/// the read-retry machinery in the EPUB path was quietly absorbing; both
/// panels now run their rated 20 MHz).
const SD_IDENT_FREQ_KHZ: u32 = 400;
const SD_DATA_FREQ_MHZ: u32 = 25;
/// Restore frequency after SD access: the active panel's SPI clock. This
/// MUST stay per-panel even though both currently rate 20 MHz — the
/// UC8253 (X3) can't decode above ~20 MHz, so restoring an out-of-spec
/// X4-style clock leaves the panel deaf to every subsequent command
/// (init included, since the boot catalog read precedes it).
const DISPLAY_FREQ_HZ: u32 = display::epd::SPI_HZ;

/// Block-level SD transaction counters for `bench:` telemetry. Single-writer
/// (all SD traffic runs on the storage/display task), so plain load+store is
/// enough on this RV32IMC core — no RMW atomics needed. Read via `snapshot`
/// deltas around a workload; never reset, so concurrent snapshots stay
/// comparable.
pub(crate) mod sd_stats {
    use core::sync::atomic::{AtomicU32, Ordering};

    pub(crate) static READ_CALLS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static READ_BLOCKS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static WRITE_CALLS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static WRITE_BLOCKS: AtomicU32 = AtomicU32::new(0);

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Snapshot {
        pub(crate) read_calls: u32,
        pub(crate) read_blocks: u32,
        pub(crate) write_calls: u32,
        pub(crate) write_blocks: u32,
    }

    pub(crate) fn snapshot() -> Snapshot {
        Snapshot {
            read_calls: READ_CALLS.load(Ordering::Relaxed),
            read_blocks: READ_BLOCKS.load(Ordering::Relaxed),
            write_calls: WRITE_CALLS.load(Ordering::Relaxed),
            write_blocks: WRITE_BLOCKS.load(Ordering::Relaxed),
        }
    }

    impl Snapshot {
        pub(crate) fn since(self, start: Snapshot) -> Snapshot {
            Snapshot {
                read_calls: self.read_calls.wrapping_sub(start.read_calls),
                read_blocks: self.read_blocks.wrapping_sub(start.read_blocks),
                write_calls: self.write_calls.wrapping_sub(start.write_calls),
                write_blocks: self.write_blocks.wrapping_sub(start.write_blocks),
            }
        }
    }

    pub(crate) fn bump(counter: &AtomicU32, amount: u32) {
        let value = counter.load(Ordering::Relaxed).wrapping_add(amount);
        counter.store(value, Ordering::Relaxed);
    }
}

/// Counts physical block transactions on their way to the SD card, so bench
/// telemetry can report exact CMD17/CMD24-level traffic per workload.
pub(crate) struct CountingDevice<B>(B);

impl<B: BlockDevice> BlockDevice for CountingDevice<B> {
    type Error = B::Error;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        sd_stats::bump(&sd_stats::READ_CALLS, 1);
        sd_stats::bump(&sd_stats::READ_BLOCKS, blocks.len() as u32);
        self.0.read(blocks, start_block_idx)
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        sd_stats::bump(&sd_stats::WRITE_CALLS, 1);
        sd_stats::bump(&sd_stats::WRITE_BLOCKS, blocks.len() as u32);
        self.0.write(blocks, start_block_idx)
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        self.0.num_blocks()
    }
}

pub(crate) struct StaticTime;

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

#[derive(Clone, Copy)]
pub(crate) struct SdDelay;

impl DelayNs for SdDelay {
    fn delay_ns(&mut self, ns: u32) {
        sd_spi_pace(ns.saturating_div(100).max(1));
    }
}

pub(crate) struct SdSpiDevice<'a, SPI, CS> {
    pub(crate) spi: &'a mut SPI,
    pub(crate) cs: &'a mut CS,
    pub(crate) delay: SdDelay,
}

/// Also sizes the shared bus's RX DMA buffer in main.rs: SD traffic is the
/// only read path on SPI2 (the EPD is write-only), and every SD operation
/// bounces through one of these chunks. Sized to one SD block so a 512-B
/// data read/write is a single DMA transaction instead of eight; the
/// extra ~448 B of DRAM comes out of the stack headroom build.rs asserts.
pub(crate) const SD_SPI_CHUNK_BYTES: usize = 512;

#[repr(align(4))]
struct AlignedSdChunk([u8; SD_SPI_CHUNK_BYTES]);

/// The one bounce chunk all SD SPI operations share. Static rather than a
/// local so the 512-B block never lands on the reader's deep-call stack
/// (the 27 KB link-time budget in build.rs is nearly spent); as .bss it is
/// instead counted against the stack-headroom ASSERT. Sound for the same
/// reason `sd_stats` uses plain load/store: every SD transaction runs on
/// the storage/display task, and the borrows below never overlap.
struct SdBounce {
    chunk: core::cell::UnsafeCell<AlignedSdChunk>,
    /// A shared static cannot rule out overlapping borrows at compile
    /// time, so this flag turns any overlap (including reentrancy from
    /// the callback) into a panic instead of aliased `&mut`s.
    busy: portable_atomic::AtomicBool,
}
// Safety: only the single SD-owning task touches the chunk (see above),
// and `with_sd_bounce` panics on overlapping access.
#[allow(unsafe_code)]
unsafe impl Sync for SdBounce {}
static SD_BOUNCE: SdBounce = SdBounce {
    chunk: core::cell::UnsafeCell::new(AlignedSdChunk([0xFF; SD_SPI_CHUNK_BYTES])),
    busy: portable_atomic::AtomicBool::new(false),
};

/// Runs `f` with exclusive access to the shared bounce chunk, refilled
/// with the 0xFF idle pattern SD cards expect on MOSI during reads. The
/// closure signature keeps the borrow from escaping.
#[allow(unsafe_code)]
fn with_sd_bounce<R>(f: impl FnOnce(&mut AlignedSdChunk) -> R) -> R {
    use portable_atomic::Ordering;
    if SD_BOUNCE.busy.swap(true, Ordering::Acquire) {
        panic!("sd bounce chunk borrowed twice");
    }
    // Safety: the busy flag above makes this the only live borrow, and
    // it cannot outlive `f`.
    let chunk = unsafe { &mut *SD_BOUNCE.chunk.get() };
    chunk.0.fill(0xFF);
    let result = f(chunk);
    SD_BOUNCE.busy.store(false, Ordering::Release);
    result
}

fn sd_spi_pace(iterations: u32) {
    for _ in 0..iterations {
        core::hint::spin_loop();
    }
}

impl<SPI, CS> embedded_hal::spi::ErrorType for SdSpiDevice<'_, SPI, CS>
where
    SPI: embedded_hal::spi::ErrorType,
{
    type Error = SPI::Error;
}

impl<SPI, CS> SpiDevice for SdSpiDevice<'_, SPI, CS>
where
    SPI: BlockingSpiBus<u8>,
    CS: OutputPin,
{
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
        let _ = self.cs.set_low();
        let mut result = Ok(());

        for operation in operations {
            result = match operation {
                Operation::Read(buffer) => self.read_with_sd_clocks(buffer),
                Operation::Write(buffer) => self.write_chunked(buffer),
                Operation::Transfer(read, write) => self.transfer_chunked(read, write),
                Operation::TransferInPlace(buffer) => self.transfer_in_place_chunked(buffer),
                Operation::DelayNs(ns) => {
                    self.delay.delay_ns(*ns);
                    Ok(())
                }
            };

            if result.is_err() {
                break;
            }
        }

        let _ = self.spi.flush();
        let _ = self.cs.set_high();
        result
    }
}

impl<SPI, CS> SdSpiDevice<'_, SPI, CS>
where
    SPI: BlockingSpiBus<u8>,
{
    fn read_with_sd_clocks(&mut self, buffer: &mut [u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks_mut(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
                chunk.copy_from_slice(&bounce.0[..chunk.len()]);
                Ok(())
            })?;
        }
        Ok(())
    }

    fn write_chunked(&mut self, buffer: &[u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                bounce.0[..chunk.len()].copy_from_slice(chunk);
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])
            })?;
        }
        Ok(())
    }

    fn transfer_chunked(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), SPI::Error> {
        let common = read.len().min(write.len());
        let (read_common, read_tail) = read.split_at_mut(common);
        let (write_common, write_tail) = write.split_at(common);

        for (read_chunk, write_chunk) in read_common
            .chunks_mut(SD_SPI_CHUNK_BYTES)
            .zip(write_common.chunks(SD_SPI_CHUNK_BYTES))
        {
            with_sd_bounce(|bounce| {
                bounce.0[..write_chunk.len()].copy_from_slice(write_chunk);
                self.spi
                    .transfer_in_place(&mut bounce.0[..write_chunk.len()])?;
                read_chunk.copy_from_slice(&bounce.0[..read_chunk.len()]);
                Ok(())
            })?;
        }
        if !read_tail.is_empty() {
            self.read_with_sd_clocks(read_tail)?;
        }
        if !write_tail.is_empty() {
            self.write_chunked(write_tail)?;
        }
        Ok(())
    }

    fn transfer_in_place_chunked(&mut self, buffer: &mut [u8]) -> Result<(), SPI::Error> {
        for chunk in buffer.chunks_mut(SD_SPI_CHUNK_BYTES) {
            with_sd_bounce(|bounce| {
                bounce.0[..chunk.len()].copy_from_slice(chunk);
                self.spi.transfer_in_place(&mut bounce.0[..chunk.len()])?;
                chunk.copy_from_slice(&bounce.0[..chunk.len()]);
                Ok(())
            })?;
        }
        Ok(())
    }
}

type SdSpi<'a> = SdSpiDevice<'a, SpiDmaBus<'static, Async>, Output<'static>>;

type SdCardDevice<'a> = CountingDevice<SdCard<SdSpi<'a>, SdDelay>>;
pub(crate) type SdRoot<'a> = Directory<'a, SdCardDevice<'a>, StaticTime, 8, 8, 1>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SdSessionError {
    CardInit,
    Volume,
    Root,
}

/// Each per-page-turn SD access shares the SPI bus with the panel, so the
/// card is re-acquired every call. A *cold* acquire runs the SD SPI init
/// handshake (CMD0/ACMD41), specced to take up to hundreds of milliseconds
/// — far more than the section read itself. While the device stays awake
/// the card never loses power or its SPI-mode init, so after the first
/// successful acquire we remember its `CardType` and skip the handshake on
/// later sessions via `mark_card_as_init`. Deep sleep resets the chip and
/// clears this static, forcing one honest cold acquire on wake. A warm
/// acquire that can't open the volume falls back to a cold one, so a wrong
/// guess is at worst as slow as before — never a failed read.
const WARM_CARD_NONE: u8 = 0;
static WARM_CARD_CODE: AtomicU8 = AtomicU8::new(WARM_CARD_NONE);

fn remembered_card_type() -> Option<CardType> {
    match WARM_CARD_CODE.load(Ordering::Relaxed) {
        1 => Some(CardType::SD1),
        2 => Some(CardType::SD2),
        3 => Some(CardType::SdhcSdxc),
        _ => None,
    }
}

fn remember_card_type(card_type: CardType) {
    let code = match card_type {
        CardType::SD1 => 1,
        CardType::SD2 => 2,
        CardType::SdhcSdxc => 3,
    };
    WARM_CARD_CODE.store(code, Ordering::Relaxed);
}

fn forget_card_warmth() {
    WARM_CARD_CODE.store(WARM_CARD_NONE, Ordering::Relaxed);
}

/// Kept out of line: the VolumeManager/SdCard session state is multi-KB
/// and must not be pooled into every caller's frame.
#[inline(never)]
pub(crate) fn with_root<R>(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    f: impl for<'a> FnOnce(&SdRoot<'a>) -> R,
) -> Result<R, SdSessionError> {
    epd.deselect_display();
    sd_cs.set_high();
    esp_println::println!(
        "sd: session enter t_ms={}",
        embassy_time::Instant::now().as_millis()
    );

    // The callback is consumed only once the root dir is open, so a warm
    // acquire that bails before then leaves it intact for the cold retry.
    let mut pending = Some(f);
    let mut result = Err(SdSessionError::CardInit);
    if let Some(card_type) = remembered_card_type() {
        result = run_sd_session(epd, sd_cs, Some(card_type), &mut pending);
        if result.is_err() {
            esp_println::println!("sd: warm reuse failed, cold retry");
            forget_card_warmth();
        }
    }
    if pending.is_some() {
        result = run_sd_session(epd, sd_cs, None, &mut pending);
    }

    esp_println::println!("sd: session exit");
    sd_cs.set_high();
    let _ = epd
        .spi_mut()
        .apply_config(&SpiConfig::default().with_frequency(Rate::from_hz(DISPLAY_FREQ_HZ)));
    result
}

/// One SD acquire + open-root + callback. `assume_init` skips the init
/// handshake for a card known to still be warm. The callback is taken from
/// `f` only after the root dir opens, so any earlier failure returns with
/// `f` untouched for the caller to retry cold.
#[allow(unsafe_code)]
#[inline(never)]
fn run_sd_session<R, F>(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    assume_init: Option<CardType>,
    f: &mut Option<F>,
) -> Result<R, SdSessionError>
where
    F: for<'a> FnOnce(&SdRoot<'a>) -> R,
{
    // Identification phase: 400 kHz with at least 74 wake clocks while no
    // chip select is asserted, per the SD spec and embedded-sdmmc's docs.
    // Harmless for an already-initialised card (it ignores the bus while
    // deselected), so the warm path runs it too rather than special-casing.
    {
        let spi = epd.spi_mut();
        let _ = spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_khz(SD_IDENT_FREQ_KHZ)));
        let mut wake = [0xFFu8; 10];
        let _ = BlockingSpiBus::transfer_in_place(spi, &mut wake);
        let _ = BlockingSpiBus::flush(spi);
    }

    let spi = SdSpiDevice {
        spi: epd.spi_mut(),
        cs: sd_cs,
        delay: SdDelay,
    };
    let card = SdCard::new(spi, SdDelay);
    match assume_init {
        Some(card_type) => {
            // SAFETY: the card has stayed powered and in SPI mode since the
            // cold acquire that recorded this type; reads below skip
            // re-init because the type is already set. A stale guess
            // surfaces as an open_volume failure and a cold retry, so this
            // never reads with the wrong addressing mode silently.
            unsafe { card.mark_card_as_init(card_type) };
        }
        None => {
            esp_println::println!("sd: card probe");
            if card.num_bytes().is_err() {
                return Err(SdSessionError::CardInit);
            }
            esp_println::println!("sd: card ready");
            if let Some(card_type) = card.get_card_type() {
                remember_card_type(card_type);
            }
        }
    }

    // Card acquired: switch to the in-spec data rate for the rest of the
    // session.
    card.spi(|device| {
        let _ = device
            .spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_mhz(SD_DATA_FREQ_MHZ)));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(CountingDevice(card), StaticTime, 5000);
    // Bind the outcome so the open_volume scrutinee temporary (which borrows
    // volume_mgr) is dropped at the `;` while volume_mgr is still alive,
    // rather than racing volume_mgr's own drop at the function tail.
    let result = match volume_mgr.open_volume(VolumeIdx(0)) {
        Ok(volume) => {
            esp_println::println!("sd: volume open");
            let raw_volume = volume.to_raw_volume();
            if let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) {
                esp_println::println!("sd: root open");
                let root = Directory::new(raw_root, &volume_mgr);
                let callback = f.take().expect("sd session callback present");
                let value = callback(&root);
                esp_println::println!("sd: root callback done");
                drop(root);
                let _ = volume_mgr.close_volume(raw_volume);
                Ok(value)
            } else {
                let _ = volume_mgr.close_volume(raw_volume);
                Err(SdSessionError::Root)
            }
        }
        Err(_) => Err(SdSessionError::Volume),
    };
    result
}

/// The upload phase: one SD session held open for the rest of the sync
/// session, writing browser-sent books to /BOOKS as they stream in.
/// Returns only after every open FAT handle has been dropped and the
/// display SPI clock has been restored. Sleep is re-queued for the normal
/// display shutdown path; wireless Exit receives an explicit stopped
/// acknowledgement.
pub(crate) async fn upload_session(epd: &mut Epd, sd_cs: &mut Output<'static>) {
    crate::upload::UPLOAD_SESSION_ACTIVE.store(true, portable_atomic::Ordering::SeqCst);
    epd.deselect_display();
    sd_cs.set_high();
    esp_println::println!("upload: session enter");

    {
        let spi = epd.spi_mut();
        let _ = spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_khz(SD_IDENT_FREQ_KHZ)));
        let mut wake = [0xFFu8; 10];
        let _ = BlockingSpiBus::transfer_in_place(spi, &mut wake);
        let _ = BlockingSpiBus::flush(spi);
    }

    let spi = SdSpiDevice {
        spi: epd.spi_mut(),
        cs: sd_cs,
        delay: SdDelay,
    };
    let card = SdCard::new(spi, SdDelay);
    if card.num_bytes().is_err() {
        esp_println::println!("upload: card init failed");
        let exit = refuse_uploads_until_exit().await;
        // `card` has no Drop; its borrow of the EPD bus ends at its last
        // use above, so the SPI clock restore below is free to re-borrow.
        finish_upload_session(epd, exit).await;
        return;
    }
    card.spi(|device| {
        let _ = device
            .spi
            .apply_config(&SpiConfig::default().with_frequency(Rate::from_mhz(SD_DATA_FREQ_MHZ)));
    });
    let volume_mgr: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(CountingDevice(card), StaticTime, 5000);
    let Ok(volume) = volume_mgr.open_volume(VolumeIdx(0)) else {
        esp_println::println!("upload: volume open failed");
        let exit = refuse_uploads_until_exit().await;
        drop(volume_mgr);
        finish_upload_session(epd, exit).await;
        return;
    };
    let raw_volume = volume.to_raw_volume();
    let Ok(raw_root) = volume_mgr.open_root_dir(raw_volume) else {
        esp_println::println!("upload: root open failed");
        let exit = refuse_uploads_until_exit().await;
        let _ = volume_mgr.close_volume(raw_volume);
        drop(volume_mgr);
        finish_upload_session(epd, exit).await;
        return;
    };
    let root = Directory::new(raw_root, &volume_mgr);
    // New books invalidate the catalog snapshot: the next boot's cache load
    // misses and runs a full scan, which is how uploads surface.
    //
    // Proving it gone is a precondition for changing anything, not a
    // courtesy. A clean install clears its journal and a delete never writes
    // one, so afterwards nothing on the card would say the snapshot is stale:
    // it would simply be believed, listing a deleted book or missing a new
    // one.
    let catalog_cleared = upload_store::clear_cache_file(&root, proto::cache::CATALOG_FILE);
    if catalog_cleared {
        esp_println::println!("upload: catalog snapshot invalidated");
    } else {
        esp_println::println!(
            "upload: catalog snapshot could not be invalidated; refusing changes"
        );
    }
    // The books handle lives inside this block so every FAT handle is dead
    // by scope before the close-and-acknowledge tail below: UPLOAD_STOPPED
    // must not be sent (nor Sleep re-queued) while the volume is mounted,
    // which is also why a missing/uncreatable BOOKS directory refuses
    // uploads until an exit arrives instead of parking in a forever loop
    // that would acknowledge over live handles.
    let exit = {
        // Opened, not created, and the reclaim journal settled before the
        // shelf can be made. Creating a directory allocates, and a live
        // reclaim holds recorded cluster numbers that carry no ownership --
        // so an allocation before it settles can be handed one, and the
        // replay that follows frees it back out of whatever took it.
        //
        // Reachable since the OTA trigger began using the journal: a root
        // reclaim can be outstanding on a card that has never held a book,
        // and this is the one path that would create the shelf while it
        // stood. The mount scanner and the command gate already obey this
        // rule; setup did not.
        // Resolved rather than opened by name. A shelf carrying a long name
        // sits under an alias that is not its name, and missing it here would
        // make a second shelf beside the real one a moment later.
        // A card that will not answer an open is not one to respond to with a
        // metadata write, so the fault and the absence stay apart here.
        let opened = match upload_store::library::open_library_root(&root) {
            Ok(shelf) => Some(shelf),
            Err(error) => {
                // Both refuse an upload, and they refuse it for different
                // reasons: a card that would not answer, against a card
                // holding more than one plausible shelf.
                esp_println::println!("upload: the shelf would not open: {error:?}");
                None
            }
        };
        match opened {
            None => refuse_uploads_until_exit().await,
            Some(existing) => match upload_store::reclaim::recover(&root, existing.as_ref()) {
                Err(error) => {
                    esp_println::println!(
                        "upload: a reclaim is unfinished; refusing the session ({:?})",
                        error
                    );
                    refuse_uploads_until_exit().await
                }
                Ok(_) => {
                    // Settled, so the shelf may now be created if it is not
                    // there.
                    let books = match existing {
                        Some(books) => Some(books),
                        // Resolution said there is none, so this creates the
                        // only one, and is read back through the resolver so
                        // the handle is the same one every other path finds.
                        None => match root.make_dir_in_dir(upload_store::SHELF_DIR) {
                            Ok(()) => upload_store::library::open_library_root(&root)
                                .ok()
                                .flatten(),
                            Err(_) => None,
                        },
                    };
                    match books {
                        Some(books) => serve_uploads(&root, &books, catalog_cleared).await,
                        None => {
                            esp_println::println!("upload: BOOKS setup failed");
                            refuse_uploads_until_exit().await
                        }
                    }
                }
            },
        }
    };
    drop(root);
    let _ = volume_mgr.close_volume(raw_volume);
    drop(volume_mgr);
    finish_upload_session(epd, exit).await;
}

/// The serving loop of a fully set-up session: writes and deletes books as
/// begins arrive, until Sleep or wireless Exit ends the session.
async fn serve_uploads<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    catalog_cleared: bool,
) -> UploadSessionExit
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// What woke the session loop.
    enum SessionInput {
        Command(UploadBegin),
        Stop,
        Display(DisplayCommand),
        #[cfg(feature = "powercut-selftest")]
        Digest(crate::powercut::DigestRequest),
    }

    /// The session's wait. A `powercut-selftest` build listens for a
    /// readback alongside the ordinary commands; a shipping build compiles
    /// to exactly the two-way wait it always had.
    async fn next_session_input() -> SessionInput {
        let ordinary = async {
            match select(
                UPLOAD_BEGINS.receive(),
                select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
            )
            .await
            {
                Either::First(begin) => SessionInput::Command(begin),
                Either::Second(Either::First(())) => SessionInput::Stop,
                Either::Second(Either::Second(command)) => SessionInput::Display(command),
            }
        };
        #[cfg(feature = "powercut-selftest")]
        {
            match select(ordinary, crate::powercut::DIGEST_REQUESTS.receive()).await {
                Either::First(input) => input,
                Either::Second(request) => SessionInput::Digest(request),
            }
        }
        #[cfg(not(feature = "powercut-selftest"))]
        {
            ordinary.await
        }
    }

    // Finish anything an earlier session left in flight before touching the
    // shelf. A record still on the card owns the names it describes and is
    // the only thing that knows where its files went, so this session must
    // not write over them or over it.
    //
    // Recovery changes the shelf like anything else, so it waits on the same
    // proof: it ends by clearing the record, and the record is the only thing
    // that would tell the next mount a surviving snapshot is stale.
    if !catalog_cleared {
        esp_println::println!("upload: no install is replayed while the old snapshot stands");
    } else if let Err(error) =
        upload_store::reclaim::recover(root, Some(books)).inspect(|_replayed| {
            #[cfg(feature = "powercut-selftest")]
            if *_replayed {
                esp_println::println!("powercut: recovery replayed a reclaim");
            }
        })
    {
        // Same order as at mount, and the same reason: a reclaim part way
        // through freeing a chain is the one transaction whose record
        // cannot be reconstructed, so it settles before anything else
        // allocates. A journal this build cannot read stops the session
        // rather than being written over.
        esp_println::println!(
            "upload: a reclaim is unfinished; refusing changes until it clears ({:?})",
            error
        );
    } else {
        let outcome = upload_store::install::recover_installs(root, books);
        #[cfg(feature = "powercut-selftest")]
        crate::powercut::report_recovery(&outcome);
        if !outcome.complete {
            esp_println::println!(
                "upload: an install is unfinished; refusing changes until it clears"
            );
        } else {
            // The library's intent last, once the filesystem has decided
            // what the destination holds.
            match upload_store::replace::recover(root) {
                Ok(upload_store::replace::Recovery::Nothing) => {}
                Ok(upload_store::replace::Recovery::Settled(landed)) => {
                    esp_println::println!("upload: settled a replacement in flight ({:?})", landed);
                }
                Ok(upload_store::replace::Recovery::Refused) => {
                    esp_println::println!(
                        "upload: a replacement is unresolved; refusing changes until it clears"
                    );
                }
                Err(fault) => {
                    esp_println::println!(
                        "upload: library ledger {:?}; refusing changes until it clears",
                        fault
                    );
                }
            }
        }
    }

    loop {
        let begin = match next_session_input().await {
            // Test-only readback, served from here because this is where the
            // card's owner already holds `root` and `books` open — a digest
            // taken anywhere else would be a second task on the bus. It sits
            // in the wait rather than being polled around it, so a request
            // arriving at an idle session is answered instead of waiting for
            // some other command to wake the loop.
            #[cfg(feature = "powercut-selftest")]
            SessionInput::Digest(request) => {
                let result = if request.in_books {
                    digest_book(books, &request)
                } else {
                    digest_book(root, &request)
                };
                crate::powercut::DIGEST_RESULTS
                    .send(crate::powercut::DigestReply {
                        id: request.id,
                        result,
                    })
                    .await;
                continue;
            }
            SessionInput::Command(begin) => begin,
            SessionInput::Stop => return UploadSessionExit::Wireless,
            SessionInput::Display(DisplayCommand::Sleep { generation }) => {
                return UploadSessionExit::Sleep { generation }
            }
            // The wireless screen is already painted; renders queued during
            // the upload phase describe views this session never shows.
            SessionInput::Display(DisplayCommand::Render(_)) => continue,
        };
        // Asked once per command rather than once per session, because an
        // install that fails part way through leaves a record behind and
        // everything after it is in the same position as a session that
        // started with one.
        // Nothing may change while a snapshot of the old shelf might survive.
        let settled = catalog_cleared && storage_settled(root, books);
        let ok = if !settled {
            // Writes and deletes alike: a delete could remove the very book
            // an unfinished install parked, or the one it is about to
            // install over.
            // "storage recovery", not "an install": this gate now covers the
            // reclaim journal too, and a delete refused because a reclaim
            // has not settled is not an install being in flight.
            esp_println::println!("upload: refused, storage recovery is still in flight");
            false
        } else if begin.delete {
            // Journalled: the name goes before the space, and a reset in
            // between leaves the book wholly there or wholly gone rather
            // than listed over clusters that have already been handed back.
            let place = if begin.in_books {
                upload_store::reclaim::Place::Books
            } else {
                upload_store::reclaim::Place::Root
            };
            // Test-only: the one place a cut can be timed into the reclaim
            // rather than aimed at it from outside. Last thing before the
            // call, since anything awaited after it spends the deadline.
            #[cfg(feature = "powercut-selftest")]
            crate::powercut::arm_for_reclaim().await;
            let removed = match upload_store::reclaim::reclaim_entry(
                root,
                Some(books),
                place,
                begin.name.as_str(),
            ) {
                Ok(()) => true,
                Err(error) => {
                    esp_println::println!("upload: delete refused: {:?}", error);
                    false
                }
            };
            if removed && begin.in_books {
                upload_store::delete_upload_sidecars(root, begin.name.as_str());
            }
            removed
        } else {
            match write_one_book(root, books, &begin).await {
                UploadWrite::Finished(landed) => landed.is_some(),
                UploadWrite::Interrupted(exit) => return exit,
            }
        };
        esp_println::println!(
            "upload: '{}' {} ok={}",
            if begin.delete {
                begin.name.as_str()
            } else {
                begin.long_name.as_str()
            },
            if begin.delete { "delete" } else { "write" },
            ok
        );
        UPLOAD_RESULTS.send(ok).await;
    }
}

/// Read a book back off the card and digest the bytes that are actually
/// there (`powercut-selftest` only).
///
/// The durability campaign's oracle was the directory entry's recorded
/// length, which is metadata: an entry of the right size whose chain is
/// wrong, unreadable, or holding somebody else's clusters satisfies it. This
/// is the independent evidence — the file is opened, streamed, and hashed,
/// so what comes back describes the chain rather than the entry over it.
///
/// `None` means the book could not be opened or a read failed part way, both
/// of which the campaign treats as a book that is not there in any useful
/// sense.
#[cfg(feature = "powercut-selftest")]
fn digest_book<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    dir: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    request: &crate::powercut::DigestRequest,
) -> crate::powercut::DigestResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = dir
        .open_file_in_dir(request.name.as_str(), embedded_sdmmc::Mode::ReadOnly)
        .ok()?;
    let length = file.length();
    if request.from > 0 {
        file.seek_from_start(request.from).ok()?;
    }
    let mut hash = request.seed;
    let mut read_total = 0u32;
    // One sector at a time, on the session's stack. The campaign is the only
    // caller and it is waiting on the answer, so this is allowed to be slow;
    // it is not allowed to be large.
    let mut buffer = [0u8; 512];
    // No await in this loop, deliberately, and the signature is plain `fn` so
    // it cannot grow one. This once yielded every 32 sectors, on the reading
    // that a multi-second read would starve the socket carrying its answer.
    // That yield sits inside an open file read, and every session builds its
    // own volume manager over the one card, so whatever runs in the gap can
    // be holding its own handles on the same volume.
    //
    // The starvation it guarded against does not reproduce: a full span,
    // read straight through, answers correctly at every size up to the
    // `MAX_DIGEST_SPAN_BYTES` bound -- measured against the 29 MB book on a
    // real card, checked against a digest computed from an image of that
    // card rather than from the device's own reading of it. So the window
    // closes for nothing, which is the only reason to close it: no
    // corruption was ever traced to it.
    while read_total < request.len && !file.is_eof() {
        let want = (request.len - read_total).min(buffer.len() as u32) as usize;
        let read = file.read(&mut buffer[..want]).ok()?;
        if read == 0 {
            break;
        }
        hash = crate::powercut::digest_chunk(hash, &buffer[..read]);
        read_total += read as u32;
    }
    Some((length, read_total, hash))
}

/// Whether the journals leave the shelf free to change, replaying them once
/// more if they do not.
///
/// Both of them: an unsettled reclaim refuses a command as firmly as an
/// unfinished install, so the name says storage rather than installs.
///
/// A pass that could not finish may only have been the card refusing a read,
/// and a session that gave up at its first command would then refuse every
/// command after it. Cheap when there is nothing in flight: one read of the
/// journal, and no recovery pass at all.
fn storage_settled<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // The reclaim journal first, and it gates every command rather than only
    // the session's start. A reclaim this build cannot read may be part way
    // through freeing a chain, and an upload allocating over those clusters
    // is the one thing that must not happen while that is unresolved. Its
    // replay is cheap when there is nothing outstanding: one read of a
    // journal that says so.
    if upload_store::reclaim::recover(root, Some(books)).is_err() {
        return false;
    }
    if !journal_is_clear(root) {
        upload_store::install::recover_installs(root, books);
        if !journal_is_clear(root) {
            return false;
        }
    }
    // Then the library's own intent, which can outlive both journals. A
    // replacement that will not resolve owns its place, and nothing else
    // may change the shelf beside it.
    matches!(
        upload_store::replace::recover(root),
        Ok(upload_store::replace::Recovery::Nothing | upload_store::replace::Recovery::Settled(_))
    )
}

/// A journal with nothing left to replay. A card that will not answer is not
/// one of those.
fn journal_is_clear<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    matches!(
        upload_store::install::read_intent(root),
        Ok(upload_store::install::IntentState::Absent
            | upload_store::install::IntentState::Truncated)
    )
}

/// Restore the display's SPI clock, clear the session-active flag, and hand
/// control to whichever consumer forced the exit: Sleep re-queues the
/// display command for the normal shutdown path (progress flush, sleep
/// frame, panel power-down); wireless Exit gets its stopped
/// acknowledgement so the reset proceeds over a closed filesystem.
async fn finish_upload_session(epd: &mut Epd, exit: UploadSessionExit) {
    let _ = epd
        .spi_mut()
        .apply_config(&SpiConfig::default().with_frequency(Rate::from_hz(DISPLAY_FREQ_HZ)));
    crate::upload::UPLOAD_SESSION_ACTIVE.store(false, portable_atomic::Ordering::SeqCst);
    match exit {
        // The re-queued Sleep keeps its generation so the power task's
        // handshake still recognizes the eventual acknowledgement as its own.
        UploadSessionExit::Sleep { generation } => {
            // The book server may be stranded mid-request (blocked on a
            // returned buffer or a result that will never come). Every
            // loaned buffer is back in the channels by now — the writer
            // recycles each chunk before receiving the next — so the
            // server can fail the request and reclaim them. Matters only
            // when sleep turns out non-terminal; a completed deep sleep
            // resets before the server is polled again.
            crate::UPLOAD_INTERRUPTS.signal(());
            // A wireless Exit racing this Sleep may have sent its stop
            // request after the writer stopped listening (the active flag
            // clears only above, so exit_after_uploads still saw a live
            // session). Answer it here — the volume is already closed —
            // or that task waits forever on an ack no one would send. No
            // await separates the flag store from this drain, and none
            // separates the exit task's flag load from its send, so on
            // this single-threaded executor a request is either drained
            // here or never sent.
            if UPLOAD_STOP_REQUESTS.try_receive().is_ok() {
                UPLOAD_STOPPED.send(()).await;
            }
            DISPLAY_COMMANDS
                .send(DisplayCommand::Sleep { generation })
                .await
        }
        UploadSessionExit::Wireless => UPLOAD_STOPPED.send(()).await,
    }
}

#[derive(Clone, Copy)]
enum UploadSessionExit {
    /// Carries the interrupting Sleep's generation for the re-queue.
    Sleep {
        generation: u32,
    },
    Wireless,
}

enum UploadWrite {
    /// `Some` carries the landing: the alias the book answers to and the
    /// identity of its bytes. A rollback or a refused install has none.
    Finished(Option<upload_store::install::Landed>),
    Interrupted(UploadSessionExit),
}

async fn write_one_book<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    books: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    begin: &UploadBegin,
) -> UploadWrite
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // The book streams into scratch space outside /BOOKS, so nothing here
    // can leave a partial file on the shelf. Choosing the alias, retiring
    // whatever held the name, and surviving a reset mid-swap all belong to
    // the installer in upload-store, where the host tests exercise them.
    let mut staged = match upload_store::install::StagedUpload::begin(
        root,
        books,
        begin.long_name.as_str(),
        begin.legacy.clone(),
    ) {
        Ok(staged) => staged,
        Err(error) => {
            // Busy is the ordinary one: an earlier install is still owed
            // work, and until it clears nothing else may touch the shelf.
            esp_println::println!("upload: cannot stage '{}': {:?}", begin.long_name, error);
            return match drain_until_end().await {
                Ok(()) => UploadWrite::Finished(None),
                Err(exit) => UploadWrite::Interrupted(exit),
            };
        }
    };
    let mut failed = false;
    let mut aborted = false;
    loop {
        let chunk = match next_upload_chunk().await {
            Ok(chunk) => chunk,
            Err(exit) => {
                // Nothing has been published, so abandoning is just
                // reclaiming the scratch file. Whatever holds this name on
                // the shelf never knew about this upload.
                staged.abandon(root);
                return UploadWrite::Interrupted(exit);
            }
        };
        if !failed && !chunk.abort {
            if let Some(buffer) = &chunk.buffer {
                // One blocking whole-chunk write, on purpose. Pacing this
                // as 512-B slices with a yield between them (to keep
                // net_task fed under the theory that the 10-30 ms write
                // starves TCP) was tried and measured on hardware
                // 2026-07-11: it cost ~1 s per 3.2 MB upload and bought
                // nothing — TCP rides out the stall via buffering. Don't
                // reintroduce pacing without a timed upload A/B.
                if staged
                    .write(&buffer[..chunk.len.min(buffer.len())])
                    .is_err()
                {
                    failed = true;
                }
            }
        }
        let last = chunk.last;
        aborted |= chunk.abort;
        recycle(chunk).await;
        if last || aborted {
            break;
        }
    }
    if failed || aborted {
        staged.abandon(root);
        return UploadWrite::Finished(None);
    }
    // Test-only: the one place a cut can be timed into the install rather
    // than aimed at it from outside. Must be the last thing before the call
    // — anything awaited after it spends the deadline it just armed.
    #[cfg(feature = "powercut-selftest")]
    crate::powercut::arm_for_install().await;
    // install closes the file first: a book the card has not finished
    // writing is never published. From the moment the intent is durable the
    // swap completes here or at the next mount, never half way.
    let rng = esp_hal::rng::Rng::new();
    match staged.install(root, books, &mut || rng.random()) {
        Ok(landed) => {
            // Free here and expensive later: the bytes were hashed as they
            // streamed. Best-effort and outside the transaction, so a card
            // that will not take the record still leaves the book installed.
            if let Some(landed) = &landed {
                upload_store::record_source_identity(root, landed.alias.as_str(), &landed.source);
            }
            UploadWrite::Finished(landed)
        }
        Err(error) => {
            // Worth naming: from the outside every one of these is the same
            // silent `ok=false`, and they call for different things — Card is
            // the card, Busy is a record this session must wait out.
            esp_println::println!("upload: '{}' not installed: {:?}", begin.long_name, error);
            UploadWrite::Finished(None)
        }
    }
}

/// Consumes one file's worth of chunks without a file to write into.
async fn drain_until_end() -> Result<(), UploadSessionExit> {
    loop {
        let chunk = next_upload_chunk().await?;
        let done = chunk.last || chunk.abort;
        recycle(chunk).await;
        if done {
            return Ok(());
        }
    }
}

/// One upload chunk, or the session-ending interrupt that arrived instead.
async fn next_upload_chunk() -> Result<UploadChunk, UploadSessionExit> {
    loop {
        match select(
            UPLOAD_CHUNKS.receive(),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(chunk) => return Ok(chunk),
            Either::Second(Either::First(())) => return Err(UploadSessionExit::Wireless),
            Either::Second(Either::Second(DisplayCommand::Sleep { generation })) => {
                return Err(UploadSessionExit::Sleep { generation })
            }
            Either::Second(Either::Second(DisplayCommand::Render(_))) => {}
        }
    }
}

async fn recycle(chunk: UploadChunk) {
    if let Some(buffer) = chunk.buffer {
        UPLOAD_RETURNS.send(buffer).await;
    }
}

/// Session setup stalled short of an upload-capable BOOKS directory:
/// answer every upload attempt with failure until Sleep or wireless Exit
/// ends the session, and report which one did. Touches no SD state — the
/// caller closes whatever handles it still holds before acknowledging.
async fn refuse_uploads_until_exit() -> UploadSessionExit {
    loop {
        match select(
            UPLOAD_BEGINS.receive(),
            select(UPLOAD_STOP_REQUESTS.receive(), DISPLAY_COMMANDS.receive()),
        )
        .await
        {
            Either::First(_) => match drain_until_end().await {
                Ok(()) => UPLOAD_RESULTS.send(false).await,
                Err(exit) => return exit,
            },
            Either::Second(Either::First(())) => return UploadSessionExit::Wireless,
            Either::Second(Either::Second(DisplayCommand::Sleep { generation })) => {
                return UploadSessionExit::Sleep { generation }
            }
            Either::Second(Either::Second(DisplayCommand::Render(_))) => {}
        }
    }
}
