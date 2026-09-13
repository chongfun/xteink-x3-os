use app_core::browse::Browse;
use app_core::{ReaderSource, MAX_SD_CHAPTERS};
use display::font::{FontFamily, FontSize, FontStyle, FontWeight, LineSpacing, TypeSettings};
use heapless::String;
use proto::cache::{
    BlockRecord, BookV2SectionRecord, PageRecord, TocRecord, CACHE_KEY_BYTES, COVER_BYTES,
    COVER_HEIGHT, COVER_STRIDE, COVER_WIDTH, TOC_CHAPTER_RECORD_BYTES, TOC_CHAPTER_TITLE_BYTES,
};
use proto::text::{TextAlign, TextRole};
pub use proto::upload::derive_catalog_label;

/// Resident slice of the on-disk catalog kept for the Library list. The full
/// catalog lives in CATALOG.BIN and is streamed a window at a time around the
/// selection, so the book count is bounded by the card, not by RAM. Sized a
/// little above `ui::render::library_visible_rows(true)` so ordinary scrolling stays
/// inside one loaded window and only crossings re-read the card.
pub const LIBRARY_WINDOW: usize = 16;
pub(crate) const MAX_SD_TOC_ITEMS: usize = 128;
/// Longest current-chapter title kept resident for the Home/sleep colophon;
/// read on demand from TOC.BIN as the chapter changes.
const MAX_CURRENT_CHAPTER_TITLE: usize = 60;
pub(crate) const MAX_CUSTOM_FONT_NAME: usize = proto::font_pack::FONT_PACK_MAX_NAME_BYTES;
pub(crate) const MAX_CUSTOM_FONT_FACES: usize = 12;
// ~14 pages per 16 KB section at the default size, so 320 covers ~4,500
// pages -- enough for very long books (e.g. HPMOR) to cache whole rather
// than tripping book_partial partway. The two persistent arrays this sizes
// (here and EPUB_BOOK_SECTIONS) live in static cells, not the stack; the
// one stack-resident copy is on the shallow book-index load path, clear of
// the deep EPUB-build watermark.
pub const MAX_BOOK_SECTIONS: usize = 320;
const MAX_PUBLISHED_CHAPTER_EVENTS: usize = MAX_SD_CHAPTERS;
// Titles only (hrefs/anchors are no longer stored), so 4 KB covers the full
// 128-item TOC at ~32 bytes per title and the saved RAM widens the stack
// region, which the EPUB open path runs close to.
pub(crate) const MAX_SD_TOC_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_READER_BLOCKS: usize = 384;
pub(crate) const MAX_READER_PAGES: usize = 96;
pub(crate) const MAX_READER_TEXT_BYTES: usize = 16_384;
/// TOC records the `text` buffer can hold at once for the Chapters overview;
/// longer TOCs are windowed around the visible rows (HPMOR-sized lists fit
/// whole, but e.g. a 322-entry trade-book TOC does not).
pub const TOC_WINDOW_CAPACITY: usize = MAX_READER_TEXT_BYTES / TOC_CHAPTER_RECORD_BYTES;
pub const MAX_READER_BLOCK_TEXT: usize = 768;
pub(crate) const EMPTY_BLOCK_RECORD: BlockRecord = BlockRecord {
    text_offset: 0,
    text_len: 0,
    line_count: 0,
    role: TextRole::Body,
    style: proto::text::FontStyle::Regular,
    align: TextAlign::Justify,
};
pub(crate) const EMPTY_PAGE_RECORD: PageRecord = PageRecord {
    first_block: 0,
    block_count: 0,
};
pub(crate) const EMPTY_TOC_RECORD: TocRecord = TocRecord {
    title_offset: 0,
    title_len: 0,
    href_offset: 0,
    href_len: 0,
    anchor_offset: 0,
    anchor_len: 0,
    level: 0,
    spine_index: -1,
};
pub const EMPTY_BOOK_SECTION_RECORD: BookV2SectionRecord = BookV2SectionRecord {
    section: 0,
    spine: 0,
    start_page: 0,
    page_count: 0,
    partial: false,
};

/// All-zero-byte stand-ins for the non-zero defaults, used only by
/// [`ReaderStore::new`]: the const value behind the 47 KB `SD_LIBRARY`
/// static must be all zeroes so the linker places it in .bss instead of a
/// flashed-and-copied .data image. The zeroed slots are never read --
/// [`ReaderStore::init_runtime_defaults`] rewrites them to the real
/// defaults once at boot, before any use.
const ZERO_TOC_RECORD: TocRecord = TocRecord {
    title_offset: 0,
    title_len: 0,
    href_offset: 0,
    href_len: 0,
    anchor_offset: 0,
    anchor_len: 0,
    level: 0,
    spine_index: 0, // EMPTY_TOC_RECORD carries the real -1 sentinel.
};
const ZERO_BLOCK_RECORD: BlockRecord = BlockRecord {
    text_offset: 0,
    text_len: 0,
    line_count: 0,
    role: TextRole::Body,
    style: proto::text::FontStyle::Regular,
    align: TextAlign::Left, // EMPTY_BLOCK_RECORD's Justify has discriminant 2.
};
const ZERO_TYPE_SETTINGS: TypeSettings = TypeSettings {
    size: FontSize::Small,
    spacing: LineSpacing::Compact,
    weight: FontWeight::Normal,
    family: FontFamily::Literata,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryScanStatus {
    NotScanned,
    Scanning,
    Ready,
    Empty,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookLoadStatus {
    Empty,
    Loading,
    Ready,
    Error,
}

pub struct LibraryBookEntry {
    pub display_name: String<64>,
    pub display_label: String<64>,
    pub byte_size: u32,
    pub source_hash: u32,
}

impl LibraryBookEntry {
    // No `Default`: it is only ever constructed as a const array element in
    // the catalog window, which `Default` cannot do.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            display_name: String::new(),
            display_label: String::new(),
            byte_size: 0,
            source_hash: 0,
        }
    }
}

/// One row of the resident folder listing the Library screen draws.
///
/// The name is the whole component rather than a display-trimmed copy: a row
/// is what a locator gets built from, and a trimmed name would build a
/// locator naming nothing. The list truncates it at draw time like any other
/// label.
pub struct FolderRow {
    pub name: String<{ proto::library_path::MAX_COMPONENT_BYTES }>,
    pub is_dir: bool,
    /// Bytes, from the directory entry; zero for a folder. Pairs with the
    /// locator to give a book the identity its catalog row was written under.
    pub size: u32,
    /// Which root this row's locator is relative to. The library root shows
    /// the shelf and the card root's loose books in one list, and a locator
    /// says which it belongs to only by being paired with one.
    pub at: proto::library_path::BookRoot,
}

impl FolderRow {
    // No `Default`: only ever built as a const array element, which `Default`
    // cannot do. Same reason as `LibraryBookEntry`.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            name: String::new(),
            is_dir: false,
            size: 0,
            at: proto::library_path::BookRoot::Library,
        }
    }
}

/// Where browsing was before a move, from [`ReaderStore::browse_checkpoint`].
///
/// Opaque and not `Copy`: on any one path it is taken once and either dropped,
/// when the move landed, or spent restoring, which are the only two things
/// that may happen to it.
pub struct BrowseCheckpoint {
    browse: Browse,
    counts: upload_store::library::RowCounts,
}

pub struct ReaderCover<'a> {
    pub width: u16,
    pub height: u16,
    pub stride: u16,
    pub bits: &'a [u8; COVER_BYTES],
}

pub struct TocItem<'a> {
    pub title: &'a str,
    pub level: u8,
    /// 1-based book page the chapter starts on; 0 when unknown.
    pub page: u32,
}

/// The lengths a trim set aside, so they can only be put back as a set.
///
/// Opaque on purpose: `block_count`, `page_count` and `text_len` describe one
/// arena and have to move together.
#[derive(Clone, Copy)]
pub struct TrimmedTail {
    blocks: usize,
    pages: usize,
    text_len: usize,
}

pub struct ReaderStore {
    pub status: LibraryScanStatus,
    /// Full book count across CATALOG.BIN (the source of truth), independent of
    /// what is resident.
    total: u16,
    /// Resident list window: `window[i]` is the book at absolute index
    /// `window_start + i`, for `i < window_len`.
    window: [LibraryBookEntry; LIBRARY_WINDOW],
    window_start: usize,
    window_len: usize,
    /// Bumped every time the catalog is replaced wholesale (a scan, or a load
    /// of CATALOG.BIN). Row numbers only address a book within one epoch, so
    /// commands that name a row carry the epoch they were picked in and are
    /// refused once this has moved on. Scrolling the window is not a change:
    /// the same rows still name the same books.
    catalog_epoch: u32,
    /// Where the reader is in the folder tree, and the rules for moving
    /// through it. Lives here rather than in `ReaderState` because that is a
    /// `Copy` struct the reducer duplicates on every input event, and a
    /// locator plus a returning name is more than four hundred bytes to copy
    /// per keypress. The display task owns this store, and it already holds
    /// the catalog window on the same terms.
    browse: Browse,
    /// Resident folder listing: `folder_rows[i]` is row `folder_start + i` of
    /// the folder `browse` is in, for `i < folder_len`. Windowed exactly like
    /// the catalog above it, and refilled from the card before each Library
    /// render, so a folder of a thousand children costs the same as a folder
    /// of ten.
    folder_rows: [FolderRow; LIBRARY_WINDOW],
    folder_start: usize,
    folder_len: usize,
    /// Bumped every time browsing is repositioned by something other than a
    /// move the reader asked for, which today means a scan's forced return to
    /// the library root. Row numbers only address a row within one of these,
    /// so a command that names a row carries the generation it was picked in
    /// and is refused once this has moved on.
    ///
    /// Held apart from `catalog_epoch` because the two answer different
    /// questions. That one says whether the catalog was replaced; this one
    /// says whether the folder a row was counted in is still the folder this
    /// store is standing in. A scan that declines to rebuild, which is what
    /// an unfinished recovery leaves, moves the second without the first.
    browse_epoch: u32,
    /// How many rows of each region the current listing holds: the shelf's
    /// books, then the card root's loose ones, then the shelf's folders. The
    /// screen shows them in that order, so this is what tells a row number
    /// which region it is in.
    folder_counts: upload_store::library::RowCounts,
    /// The one book currently being opened/read. Held apart from the list
    /// window so the reading path never depends on where the Library list
    /// happens to be scrolled; `catalog_entry` returns it for `active_index`.
    active_entry: LibraryBookEntry,
    /// Where the active book is: the root its locator is relative to, and
    /// the locator. Only the active entry carries one, since list rows are
    /// labels and identity and every open stages its row here first, so
    /// paying a path's width sixteen times over for the window would buy
    /// nothing. `None` is a record this build could not place.
    active_root: Option<proto::library_path::BookRoot>,
    active_path: String<{ proto::library_path::MAX_PATH_BYTES }>,
    /// Which copy the active book is, as the library ledger knows it: the
    /// id per-copy state is addressed by, cached in the catalog row the open
    /// staged. Distinct from the `book_id` the app's views pass around,
    /// which numbers a place in the reading list.
    ///
    /// Lives exactly as long as the locator beside it, a rescan included: a
    /// locator says where the copy is and this says which copy it is, and
    /// rebuilding the catalog changes neither. `None` is a row written
    /// before the ledger adopted it.
    active_copy_id: Option<proto::identity::BookId>,
    active_index: Option<usize>,
    pub(crate) current_index: Option<usize>,
    pub loaded_index: Option<usize>,
    pub(crate) loaded_chapter: u16,
    pub(crate) reader_status: BookLoadStatus,
    pub title: String<64>,
    pub(crate) author: String<64>,
    pub(crate) error: String<32>,
    pub(crate) cache_key: String<CACHE_KEY_BYTES>,
    pub(crate) cover_ready: bool,
    pub(crate) cover_width: u16,
    pub(crate) cover_height: u16,
    pub(crate) cover_bits: [u8; COVER_BYTES],
    pub(crate) cached_spine: u16,
    pub(crate) section_partial: bool,
    pub(crate) book_total_pages: u32,
    pub current_section_start_page: u32,
    pub current_section_page_count: u16,
    pub(crate) book_cache_ready: bool,
    pub(crate) book_cache_partial: bool,
    pub(crate) book_section_count: usize,
    pub(crate) book_sections: [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    pub(crate) toc_text: [u8; MAX_SD_TOC_TEXT_BYTES],
    pub(crate) toc_text_len: usize,
    pub(crate) toc: [TocRecord; MAX_SD_TOC_ITEMS],
    pub(crate) toc_page: [u16; MAX_SD_TOC_ITEMS],
    pub(crate) toc_count: usize,
    /// Full chapter count from TOC.BIN (the on-disk list), which can exceed
    /// the resident `toc_count`. The Chapters overview reads the full list.
    pub(crate) toc_total: usize,
    /// While the Chapters view is open, `text` holds the on-disk TOC records
    /// instead of section content; the reading section is reloaded on exit.
    /// The buffer fits `TOC_WINDOW_CAPACITY` records, so for longer TOCs it
    /// holds a window: `text` record `i` is chapter `toc_window_start + i`,
    /// for `i < toc_window_len`. Slid around the visible rows before each
    /// Chapters render, like the Library's catalog window.
    pub(crate) text_holds_toc: bool,
    pub(crate) toc_window_start: usize,
    pub(crate) toc_window_len: usize,
    /// Per-section chapter-start marks (`chapter + 1`, 0 = none), parallel to
    /// `book_sections` and filled once at open from TOC.BIN. Chapter start
    /// pages are always section start pages, so this section-bounded map
    /// covers a TOC of any length -- the current chapter never gets stuck at
    /// the 128-entry resident cap or a fixed per-chapter array (322-chapter
    /// trade books resolve past 255). 640 bytes resident.
    pub(crate) chapter_start: [u16; MAX_BOOK_SECTIONS],
    /// Whether `chapter_start` holds the current book's marks; independent of
    /// the overview's `toc_total` so the map survives a Chapters visit.
    pub(crate) chapter_start_ready: bool,
    /// `(source_hash, source_size, font_config, custom_font_identity)` the
    /// `chapter_start` map was built for. The book index reloads every section
    /// crossing, so this token keeps the map from being re-read from disk
    /// except on a new book, a repaginating settings change, or a custom pack
    /// replacement.
    pub(crate) chapter_start_token: (u32, u32, u16, u64),
    /// Current chapter and its title, resolved by the firmware from
    /// `chapter_start` + the reading page on each section load, for the
    /// Home/sleep colophon and the overview's starting selection.
    pub(crate) current_chapter: u16,
    pub(crate) current_chapter_title: String<MAX_CURRENT_CHAPTER_TITLE>,
    /// Source identity (hash, size) of the book `current_chapter_title` belongs
    /// to, so a colophon shows it only for that book -- the resolved title
    /// outlives a single load (it is also set on boot restore, before the book
    /// is opened, so wake-to-Home names the chapter without a full open).
    pub(crate) current_chapter_source: (u32, u32),
    pub(crate) text: [u8; MAX_READER_TEXT_BYTES],
    pub(crate) text_len: usize,
    pub(crate) blocks: [BlockRecord; MAX_READER_BLOCKS],
    pub(crate) block_styles: [FontStyle; MAX_READER_BLOCKS],
    pub(crate) block_spine: [u16; MAX_READER_BLOCKS],
    pub(crate) block_page_break_before: [bool; MAX_READER_BLOCKS],
    pub block_paragraph_end: [bool; MAX_READER_BLOCKS],
    /// True for a block that opens a paragraph (its opening line takes the
    /// first-line indent). Persisted rather than derived so a section that
    /// carries a half-finished paragraph in at its front keeps that
    /// continuation line flush left.
    pub(crate) block_paragraph_start: [bool; MAX_READER_BLOCKS],
    pub(crate) block_count: usize,
    pub(crate) pages: [PageRecord; MAX_READER_PAGES],
    pub(crate) page_spine: [u16; MAX_READER_PAGES],
    pub(crate) page_count: usize,
    type_settings: TypeSettings,
    /// Whether the current layout paginates into the portrait page box.
    portrait: bool,
    custom_font_available: bool,
    custom_font_identity: u64,
    custom_font_name: String<MAX_CUSTOM_FONT_NAME>,
    custom_font_faces: [proto::font_pack::FontPackFaceRecord; MAX_CUSTOM_FONT_FACES],
    custom_font_face_count: usize,
}

impl ReaderStore {
    /// The boot value of the `SD_LIBRARY` static. Every byte of this const
    /// must be zero (enums by zero-discriminant variants, records by the
    /// `ZERO_*` stand-ins) so the linker keeps the 47 KB static in .bss:
    /// one non-zero byte anywhere moves the whole struct into .data, which
    /// costs a flash image copy of the full size plus a boot-time memcpy.
    /// [`Self::init_runtime_defaults`] restores the handful of non-zero
    /// defaults in place, immediately after the cell is taken.
    // No `Default`, deliberately: this struct is ~47 KB and lives in a static
    // initialised in place. `ReaderStore::default()` would materialise it by
    // value, which is the shape that overflowed the 42 KB stack during B4.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        Self {
            status: LibraryScanStatus::NotScanned,
            total: 0,
            window: [const { LibraryBookEntry::new() }; LIBRARY_WINDOW],
            window_start: 0,
            window_len: 0,
            catalog_epoch: 0,
            browse: Browse::root(),
            folder_rows: [const { FolderRow::new() }; LIBRARY_WINDOW],
            folder_start: 0,
            folder_len: 0,
            browse_epoch: 0,
            folder_counts: upload_store::library::RowCounts {
                shelf_books: 0,
                root_books: 0,
                shelf_folders: 0,
            },
            active_entry: LibraryBookEntry::new(),
            active_root: None,
            active_path: String::new(),
            active_copy_id: None,
            active_index: None,
            current_index: None,
            loaded_index: None,
            loaded_chapter: 0,
            reader_status: BookLoadStatus::Empty,
            title: String::new(),
            author: String::new(),
            error: String::new(),
            cache_key: String::new(),
            cover_ready: false,
            cover_width: 0,
            cover_height: 0,
            cover_bits: [0; COVER_BYTES],
            cached_spine: 0,
            section_partial: false,
            book_total_pages: 0,
            current_section_start_page: 0,
            current_section_page_count: 0,
            book_cache_ready: false,
            book_cache_partial: false,
            book_section_count: 0,
            book_sections: [EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS],
            toc_text: [0; MAX_SD_TOC_TEXT_BYTES],
            toc_text_len: 0,
            toc: [ZERO_TOC_RECORD; MAX_SD_TOC_ITEMS],
            toc_page: [0; MAX_SD_TOC_ITEMS],
            toc_count: 0,
            toc_total: 0,
            text_holds_toc: false,
            toc_window_start: 0,
            toc_window_len: 0,
            chapter_start: [0; MAX_BOOK_SECTIONS],
            chapter_start_ready: false,
            chapter_start_token: (0, 0, 0, 0),
            current_chapter: 0,
            current_chapter_title: String::new(),
            current_chapter_source: (0, 0),
            text: [0; MAX_READER_TEXT_BYTES],
            text_len: 0,
            blocks: [ZERO_BLOCK_RECORD; MAX_READER_BLOCKS],
            block_styles: [FontStyle::Regular; MAX_READER_BLOCKS],
            block_spine: [0; MAX_READER_BLOCKS],
            block_page_break_before: [false; MAX_READER_BLOCKS],
            block_paragraph_end: [false; MAX_READER_BLOCKS],
            block_paragraph_start: [false; MAX_READER_BLOCKS],
            block_count: 0,
            pages: [EMPTY_PAGE_RECORD; MAX_READER_PAGES],
            page_spine: [0; MAX_READER_PAGES],
            page_count: 0,
            type_settings: ZERO_TYPE_SETTINGS,
            portrait: false,
            custom_font_available: false,
            custom_font_identity: 0,
            custom_font_name: String::new(),
            custom_font_faces: [proto::font_pack::FontPackFaceRecord::EMPTY; MAX_CUSTOM_FONT_FACES],
            custom_font_face_count: 0,
        }
    }

    /// Restore the non-zero defaults the all-zero [`Self::new`] const cannot
    /// carry, mutating the static in place (never a stack copy of the 47 KB
    /// store). Called exactly once, right after `SD_LIBRARY.take()`, so no
    /// reader ever observes the `ZERO_*` stand-ins: cover dimensions, the
    /// default type settings, the -1 spine sentinel in unused TOC slots, the
    /// Justify align of unused blocks, and the paragraph-end default that
    /// `clear_lines` also maintains.
    pub fn init_runtime_defaults(&mut self) {
        self.cover_width = COVER_WIDTH as u16;
        self.cover_height = COVER_HEIGHT as u16;
        self.type_settings = TypeSettings::DEFAULT;
        for record in self.toc.iter_mut() {
            *record = EMPTY_TOC_RECORD;
        }
        for record in self.blocks.iter_mut() {
            *record = EMPTY_BLOCK_RECORD;
        }
        for end in self.block_paragraph_end.iter_mut() {
            *end = true;
        }
    }

    pub fn set_custom_font(
        &mut self,
        name: Option<&str>,
        identity: u64,
        faces: &[proto::font_pack::FontPackFaceRecord],
    ) {
        self.custom_font_name.clear();
        if let Some(name) = name {
            let _ = self.custom_font_name.push_str(name);
            self.custom_font_available = true;
            self.custom_font_identity = identity;
            self.custom_font_face_count = faces.len().min(MAX_CUSTOM_FONT_FACES);
            self.custom_font_faces =
                [proto::font_pack::FontPackFaceRecord::EMPTY; MAX_CUSTOM_FONT_FACES];
            self.custom_font_faces[..self.custom_font_face_count]
                .copy_from_slice(&faces[..self.custom_font_face_count]);
        } else {
            self.custom_font_available = false;
            self.custom_font_identity = 0;
            self.custom_font_face_count = 0;
            self.custom_font_faces =
                [proto::font_pack::FontPackFaceRecord::EMPTY; MAX_CUSTOM_FONT_FACES];
        }
    }

    pub fn custom_font_available(&self) -> bool {
        self.custom_font_available
    }

    pub fn custom_font_name(&self) -> &str {
        self.custom_font_name.as_str()
    }

    pub fn custom_font_identity(&self) -> u64 {
        if self.custom_font_available {
            self.custom_font_identity
        } else {
            0
        }
    }

    pub fn custom_font_face(
        &self,
        size_px: u8,
        style: u8,
    ) -> Option<proto::font_pack::FontPackFaceRecord> {
        self.custom_font_faces[..self.custom_font_face_count]
            .iter()
            .copied()
            .find(|face| face.size_px == size_px && face.style == style)
    }

    pub fn type_settings(&self) -> TypeSettings {
        self.type_settings
    }

    pub fn portrait(&self) -> bool {
        self.portrait
    }

    pub fn page_box(&self) -> ui::reading::PageBox {
        ui::reading::PageBox::for_portrait(self.portrait)
    }

    /// Adopt a new reading layout — type settings plus the page box — and
    /// drop the in-RAM section window's page coverage, so the next
    /// open/extend reloads the section under the new layout (a size or
    /// orientation change rebuilds it from the EPUB) instead of serving
    /// pages paginated under the old one.
    pub fn set_layout(&mut self, settings: TypeSettings, portrait: bool) {
        if self.type_settings == settings && self.portrait == portrait {
            return;
        }
        self.type_settings = settings;
        self.portrait = portrait;
        self.page_count = 0;
        self.current_section_page_count = 0;
    }

    pub fn clear_catalog(&mut self) {
        self.total = 0;
        self.window_start = 0;
        self.window_len = 0;
        self.active_index = None;
        self.current_index = None;
        // The rows the reader is looking at came off the card the catalog is
        // being rebuilt from, so they are as stale as it is. Where the reader
        // is stays: a locator outlives a rescan, and a folder that went away
        // fails its next listing rather than showing another folder's rows.
        self.clear_folder_page();
        // Every catalog replacement starts here, so this is the one place the
        // epoch has to move. Wrapping is harmless: a row command in flight
        // across 2^32 catalog rebuilds is not a case worth a wider counter.
        self.catalog_epoch = self.catalog_epoch.wrapping_add(1);
    }

    /// The catalog generation the resident rows belong to.
    pub fn catalog_epoch(&self) -> u32 {
        self.catalog_epoch
    }

    pub fn catalog_count(&self) -> usize {
        self.total as usize
    }

    pub fn catalog_count_u16(&self) -> u16 {
        self.total
    }

    pub fn set_catalog_total(&mut self, total: u16) {
        self.total = total;
    }

    pub fn catalog_is_empty(&self) -> bool {
        self.total == 0
    }

    /// The resident list window and its absolute start, for the Library view.
    pub fn catalog_window(&self) -> &[LibraryBookEntry] {
        &self.window[..self.window_len]
    }

    pub fn catalog_window_start(&self) -> usize {
        self.window_start
    }

    /// True when the loaded window already covers `[start, start+len)`, so the
    /// firmware can skip a re-read while scrolling inside it.
    pub fn window_covers(&self, start: usize, len: usize) -> bool {
        self.window_len > 0
            && start >= self.window_start
            && start + len <= self.window_start + self.window_len
    }

    /// Begin filling a fresh window at `start`; `push_window_entry` appends.
    pub fn begin_window(&mut self, start: usize) {
        self.window_start = start;
        self.window_len = 0;
    }

    pub fn push_window_entry(
        &mut self,
        display_name: &str,
        byte_size: u32,
        source_hash: u32,
        label_override: Option<&str>,
    ) {
        if self.window_len >= self.window.len() {
            return;
        }
        let slot = self.window_len;
        fill_entry(
            &mut self.window[slot],
            display_name,
            byte_size,
            source_hash,
            label_override,
        );
        self.window_len += 1;
    }

    /// Where the reader is in the folder tree.
    pub fn browse(&self) -> &Browse {
        &self.browse
    }

    pub fn browse_mut(&mut self) -> &mut Browse {
        &mut self.browse
    }

    /// The resident folder listing and its absolute start, for the Library
    /// view. Mirrors [`ReaderStore::catalog_window`].
    pub fn folder_rows(&self) -> &[FolderRow] {
        &self.folder_rows[..self.folder_len]
    }

    pub fn folder_start(&self) -> usize {
        self.folder_start
    }

    /// The generation of the position the resident rows were counted in.
    pub fn browse_epoch(&self) -> u32 {
        self.browse_epoch
    }

    /// Take browsing somewhere the reader did not ask to go, retiring every
    /// row number picked before it.
    ///
    /// Wrapping is harmless: a row command in flight across 2^32 forced
    /// repositions is not a case worth a wider counter, which is the same
    /// trade `clear_catalog` makes.
    pub fn reposition_browse(&mut self) {
        self.browse.reset();
        self.set_folder_counts(upload_store::library::RowCounts::default());
        self.clear_folder_page();
        self.browse_epoch = self.browse_epoch.wrapping_add(1);
    }

    /// Where browsing is, kept so a move that fails can be put back.
    ///
    /// A move that reports failure means standstill to everything that reads
    /// it, and the app keeps its own depth and rows on that word. A recovery
    /// that quietly left this store somewhere else would make the two halves
    /// describe different folders, with the screen naming one and every later
    /// command landing on the other. So a move either lands or comes back
    /// here.
    pub fn browse_checkpoint(&self) -> BrowseCheckpoint {
        BrowseCheckpoint {
            browse: self.browse.clone(),
            counts: self.folder_counts,
        }
    }

    /// Put browsing back where [`ReaderStore::browse_checkpoint`] found it.
    ///
    /// The resident rows go rather than being restored with it: they are read
    /// again on the next render, and what has to come back is the position
    /// they are read from.
    pub fn restore_browse(&mut self, point: BrowseCheckpoint) {
        self.browse = point.browse;
        self.folder_counts = point.counts;
        self.clear_folder_page();
    }

    /// How many rows of each region the current listing holds.
    pub fn folder_counts(&self) -> upload_store::library::RowCounts {
        self.folder_counts
    }

    pub fn set_folder_counts(&mut self, counts: upload_store::library::RowCounts) {
        self.folder_counts = counts;
    }

    /// True when the loaded page already covers `[start, start+len)`, so the
    /// firmware can skip a re-read while scrolling inside it.
    pub fn folder_covers(&self, start: usize, len: usize) -> bool {
        self.folder_len > 0
            && start >= self.folder_start
            && start + len <= self.folder_start + self.folder_len
    }

    /// Begin filling a fresh folder page at `start`; `push_folder_row`
    /// appends.
    pub fn begin_folder_page(&mut self, start: usize) {
        self.folder_start = start;
        self.folder_len = 0;
    }

    pub fn push_folder_row(
        &mut self,
        name: &str,
        is_dir: bool,
        size: u32,
        at: proto::library_path::BookRoot,
    ) {
        if self.folder_len >= self.folder_rows.len() {
            return;
        }
        let slot = &mut self.folder_rows[self.folder_len];
        slot.name.clear();
        let _ = slot.name.push_str(name);
        slot.is_dir = is_dir;
        slot.size = size;
        slot.at = at;
        self.folder_len += 1;
    }

    /// The row at absolute index `index`, or `None` when the resident page
    /// does not reach it. A press acts through this, so a row the page never
    /// covered is a press that does nothing rather than one that acts on a
    /// neighbour.
    pub fn folder_row(&self, index: usize) -> Option<&FolderRow> {
        let offset = index.checked_sub(self.folder_start)?;
        self.folder_rows().get(offset)
    }

    /// Forget the resident page, without moving where the reader is. The next
    /// Library render reads the folder again.
    pub fn clear_folder_page(&mut self) {
        self.folder_start = 0;
        self.folder_len = 0;
    }

    /// Adopt `index` as the active book whose entry the reading path reads
    /// through `catalog_entry`. Read once from CATALOG.BIN at open/restore.
    #[allow(clippy::too_many_arguments)]
    pub fn set_active_entry(
        &mut self,
        index: usize,
        display_name: &str,
        root: Option<proto::library_path::BookRoot>,
        path: &str,
        byte_size: u32,
        source_hash: u32,
        label_override: Option<&str>,
        copy_id: Option<proto::identity::BookId>,
    ) {
        fill_entry(
            &mut self.active_entry,
            display_name,
            byte_size,
            source_hash,
            label_override,
        );
        self.active_root = root;
        self.active_path.clear();
        let _ = self.active_path.push_str(path);
        self.active_copy_id = copy_id;
        self.active_index = Some(index);
    }

    /// Which copy the active book is, for the state that hangs from a copy
    /// rather than from a place. `None` is a book whose row carries no id,
    /// which is a card the ledger has not been over.
    pub fn active_copy_id(&self) -> Option<proto::identity::BookId> {
        self.active_copy_id
    }

    /// Where row `index` is on the card, for a path that is about to open it:
    /// the root its locator is relative to, and the locator.
    ///
    /// Answers for the active book alone. A row the list merely shows has no
    /// locator resident, and an open reaches this only after staging its row
    /// as active, so `None` here means the staging failed or the record named
    /// a root this build cannot place.
    pub fn book_location(&self, index: usize) -> Option<(proto::library_path::BookRoot, &str)> {
        if self.active_index != Some(index) || self.active_path.is_empty() {
            return None;
        }
        Some((self.active_root?, self.active_path.as_str()))
    }

    /// Copy the loaded book's title into the resident catalog entries for
    /// `index` -- the list window entry when it is on screen, and the active
    /// entry when it is the active book -- so the list shows the real title
    /// right after an open and keeps showing it when the cursor moves on.
    fn note_loaded_title(&mut self, index: usize) {
        if self.title.is_empty() {
            return;
        }
        if self.active_index == Some(index) {
            copy_label(&mut self.active_entry.display_label, self.title.as_str());
        }
        if index >= self.window_start {
            let offset = index - self.window_start;
            if offset < self.window_len {
                copy_label(&mut self.window[offset].display_label, self.title.as_str());
            }
        }
    }

    pub fn catalog_entry(&self, index: usize) -> Option<&LibraryBookEntry> {
        if self.active_index == Some(index) {
            return Some(&self.active_entry);
        }
        if index >= self.window_start {
            if let Some(entry) = self.window.get(index - self.window_start) {
                if index - self.window_start < self.window_len {
                    return Some(entry);
                }
            }
        }
        None
    }

    pub fn active_index(&self) -> Option<usize> {
        self.active_index
    }

    pub fn selected_book_index(book_id: u32) -> Option<usize> {
        ReaderSource::from_book_id(book_id)
            .sd_index()
            .map(|index| index as usize)
    }

    pub fn source_identity(&self, book_id: u32) -> (u32, u32) {
        let Some(entry) =
            Self::selected_book_index(book_id).and_then(|index| self.catalog_entry(index))
        else {
            return (0, 0);
        };
        (entry.source_hash, entry.byte_size)
    }

    pub fn clear_toc(&mut self) {
        self.toc_text_len = 0;
        self.toc_count = 0;
        for (index, record) in self.toc.iter_mut().enumerate() {
            *record = EMPTY_TOC_RECORD;
            self.toc_page[index] = 0;
        }
    }

    pub fn clear_cover(&mut self) {
        self.cover_ready = false;
        self.cover_width = COVER_WIDTH as u16;
        self.cover_height = COVER_HEIGHT as u16;
        self.cover_bits.fill(0);
    }

    /// Expose the cover buffer for direct file reads. The cover is marked
    /// not-ready until [`Self::finish_cover_load`] validates it, so a failed
    /// read can never leave a half-written cover visible.
    pub(crate) fn cover_bits_mut(&mut self) -> &mut [u8; COVER_BYTES] {
        self.cover_ready = false;
        &mut self.cover_bits
    }

    pub(crate) fn finish_cover_load(&mut self, width: u16, height: u16) {
        self.cover_width = width;
        self.cover_height = height;
        self.cover_ready = true;
    }

    pub fn clear_lines(&mut self) {
        self.text_len = 0;
        self.block_count = 0;
        self.page_count = 0;
        self.section_partial = false;
        for (index, block) in self.blocks.iter_mut().enumerate() {
            *block = EMPTY_BLOCK_RECORD;
            self.block_styles[index] = FontStyle::Regular;
            self.block_spine[index] = 0;
            self.block_page_break_before[index] = false;
            self.block_paragraph_end[index] = true;
            self.block_paragraph_start[index] = false;
        }
        for (index, page) in self.pages.iter_mut().enumerate() {
            *page = EMPTY_PAGE_RECORD;
            self.page_spine[index] = 0;
        }
    }

    /// Keep only the blocks of the in-progress final page at the front of
    /// the store, rebasing their text. Lets an intermediate section flush on
    /// a whole-page boundary and carry the half-finished page into the next
    /// section, instead of writing it as a short, half-empty page the reader
    /// stops on. `first_block` is that page's first block; callers guarantee
    /// `0 < first_block < block_count`.
    pub fn carry_last_page(&mut self, first_block: usize) {
        if first_block == 0 || first_block >= self.block_count {
            return;
        }
        let text_start = self.blocks[first_block].text_offset as usize;
        let carried_blocks = self.block_count - first_block;
        let carried_text = self.text_len.saturating_sub(text_start);
        self.text.copy_within(text_start..self.text_len, 0);
        for offset in 0..carried_blocks {
            let src = first_block + offset;
            let mut record = self.blocks[src];
            record.text_offset = record.text_offset.saturating_sub(text_start as u32);
            self.blocks[offset] = record;
            self.block_styles[offset] = self.block_styles[src];
            self.block_spine[offset] = self.block_spine[src];
            self.block_page_break_before[offset] = self.block_page_break_before[src];
            self.block_paragraph_end[offset] = self.block_paragraph_end[src];
            self.block_paragraph_start[offset] = self.block_paragraph_start[src];
        }
        for index in carried_blocks..self.block_count {
            self.blocks[index] = EMPTY_BLOCK_RECORD;
            self.block_styles[index] = FontStyle::Regular;
            self.block_spine[index] = 0;
            self.block_page_break_before[index] = false;
            self.block_paragraph_end[index] = true;
            self.block_paragraph_start[index] = false;
        }
        self.block_count = carried_blocks;
        self.text_len = carried_text;
        self.page_count = 0;
        for (index, page) in self.pages.iter_mut().enumerate() {
            *page = EMPTY_PAGE_RECORD;
            self.page_spine[index] = 0;
        }
    }

    pub fn clear_book_index(&mut self) {
        self.book_total_pages = 0;
        self.current_section_start_page = 0;
        self.current_section_page_count = 0;
        self.book_cache_ready = false;
        self.book_cache_partial = false;
        self.book_section_count = 0;
        for record in self.book_sections.iter_mut() {
            *record = EMPTY_BOOK_SECTION_RECORD;
        }
        // `chapter_start` runs parallel to `book_sections`; marks derived from
        // the old section table must not pair with a new one.
        self.chapter_start_ready = false;
    }

    pub fn begin_book_load(&mut self) {
        self.loaded_index = None;
        self.reader_status = BookLoadStatus::Loading;
        self.title.clear();
        self.author.clear();
        self.error.clear();
        self.clear_toc();
        self.clear_lines();
        self.clear_book_index();
    }

    pub fn finish_book_load(&mut self, index: usize, chapter: u16, status: BookLoadStatus) {
        if matches!(status, BookLoadStatus::Ready) {
            self.set_current_index(index);
        }
        if matches!(status, BookLoadStatus::Ready | BookLoadStatus::Error) {
            self.loaded_index = Some(index);
            self.loaded_chapter = chapter;
            // Bake the just-learned title into the resident list/active entries
            // so the Library keeps showing it once the cursor moves to another
            // book -- without waiting for the next window refill from the card.
            // A later refill re-reads the same title from the book's cache, so
            // the label also survives scrolling away and reboots.
            self.note_loaded_title(index);
        }
        self.reader_status = status;
    }

    /// Trim the section under construction back to a whole-page boundary at
    /// block `cut`, so the section written to the card ends on a page break.
    ///
    /// Returns what it set aside for [`Self::restore_trimmed`]. The three
    /// lengths that describe the arena move as one here, because a caller
    /// setting them separately is exactly how they come apart.
    /// Pages in the section currently in the arena. Mirrors `block_count()`:
    /// readable anywhere, writable only through the methods that keep it
    /// consistent with the records it counts.
    pub fn page_count(&self) -> usize {
        self.page_count
    }

    /// First block of `page` in the resident page index, or 0 if out of range.
    pub fn page_first_block(&self, page: usize) -> usize {
        self.pages
            .get(page)
            .map_or(0, |record| record.first_block as usize)
    }

    pub fn trim_to_page_boundary(&mut self, cut: usize, pages: usize) -> TrimmedTail {
        let tail = TrimmedTail {
            blocks: self.block_count,
            pages: self.page_count,
            text_len: self.text_len,
        };
        self.block_count = cut;
        self.page_count = pages;
        self.text_len = self.blocks[cut].text_offset as usize;
        tail
    }

    /// Put back all three counters [`Self::trim_to_page_boundary`] set aside,
    /// before the carried page is rebased.
    ///
    /// All three, not two: the trim reduces `page_count` as well, and leaving it
    /// truncated here would expose a full arena behind a short page index. The
    /// firmware happens to hide that by rebasing and rebuilding immediately
    /// afterwards, but an early return or an added assertion between the two
    /// would see it -- and relying on caller sequencing is the thing this API
    /// exists to stop.
    pub fn restore_trimmed(&mut self, tail: TrimmedTail) {
        self.block_count = tail.blocks;
        self.page_count = tail.pages;
        self.text_len = tail.text_len;
    }

    /// The TOC flag is cleared by whoever takes the text arena back for a real
    /// section; exposed as a setter so the flag cannot drift from the arena's
    /// actual contents by direct assignment.
    pub fn set_text_holds_toc(&mut self, holds: bool) {
        self.text_holds_toc = holds;
    }

    /// Whether the chapter start-page map is valid for the resident index.
    pub fn chapter_start_ready(&self) -> bool {
        self.chapter_start_ready
    }

    /// The text arena, borrowed as a scratch buffer while no section is loaded.
    ///
    /// The catalog sweep needs a few KB and the arena is the only buffer that
    /// size which is idle at that point. Named rather than reached through the
    /// field so the aliasing is deliberate and searchable: any caller of this is
    /// asserting no section is resident.
    pub fn arena_as_scratch(&mut self) -> &mut [u8] {
        &mut self.text
    }

    pub fn set_reader_status(&mut self, status: BookLoadStatus) {
        self.reader_status = status;
    }

    pub fn set_reader_error(&mut self, message: &str) {
        self.error.clear();
        let _ = self.error.push_str(message);
    }

    pub fn set_cache_key(&mut self, key: &str) {
        self.cache_key.clear();
        let _ = self.cache_key.push_str(key);
    }

    pub fn set_book_labels(&mut self, title: &str, author: &str) {
        copy_string(&mut self.title, title);
        copy_string(&mut self.author, author);
    }

    pub fn page_capacity(&self) -> usize {
        self.pages.len()
    }

    pub fn block_capacity(&self) -> usize {
        self.blocks.len()
    }

    /// True once the section text arena has less than one max-length line of
    /// headroom left. The streaming builder flushes the section here so the
    /// next line lands in a fresh chunk; without this the arena overflows
    /// and `push_line_block` starts silently dropping the rest of the
    /// chapter (text is the tightest of the three section budgets, hit long
    /// before the page or block caps).
    pub fn text_capacity_reached(&self) -> bool {
        self.text_len + MAX_READER_BLOCK_TEXT > self.text.len()
    }

    pub fn block_count(&self) -> usize {
        self.block_count
    }

    pub(crate) fn can_hold_section(
        &self,
        page_count: usize,
        block_count: usize,
        text_bytes: usize,
    ) -> bool {
        page_count <= self.pages.len()
            && block_count <= self.blocks.len()
            && text_bytes <= self.text.len()
    }

    pub(crate) fn set_cached_page(&mut self, index: usize, page: PageRecord, spine: u16) -> bool {
        if index >= self.pages.len() {
            return false;
        }
        self.pages[index] = page;
        self.page_spine[index] = spine;
        true
    }

    pub(crate) fn set_cached_block(
        &mut self,
        index: usize,
        block: BlockRecord,
        style: FontStyle,
        spine: u16,
    ) -> bool {
        if index >= self.blocks.len() {
            return false;
        }
        self.blocks[index] = block;
        self.block_styles[index] = style;
        self.block_spine[index] = spine;
        self.block_page_break_before[index] =
            should_break_before_block(block.role, self.blocks.get(index.wrapping_sub(1)));
        true
    }

    pub(crate) fn set_cached_paragraph_end(&mut self, index: usize, paragraph_end: bool) -> bool {
        let Some(slot) = self.block_paragraph_end.get_mut(index) else {
            return false;
        };
        *slot = paragraph_end;
        true
    }

    pub(crate) fn set_cached_paragraph_start(
        &mut self,
        index: usize,
        paragraph_start: bool,
    ) -> bool {
        let Some(slot) = self.block_paragraph_start.get_mut(index) else {
            return false;
        };
        *slot = paragraph_start;
        true
    }

    pub fn mark_last_block_paragraph_end(&mut self) {
        if self.block_count > 0 {
            self.block_paragraph_end[self.block_count - 1] = true;
        }
    }

    pub(crate) fn cached_text_mut(&mut self, text_bytes: usize) -> Option<&mut [u8]> {
        self.text.get_mut(..text_bytes)
    }

    pub(crate) fn finish_cached_section(
        &mut self,
        spine: u16,
        page_count: usize,
        block_count: usize,
        text_len: usize,
        partial: bool,
    ) {
        self.page_count = page_count;
        self.block_count = block_count;
        self.text_len = text_len;
        self.cached_spine = spine;
        self.section_partial = partial;
        // A real section now occupies the text buffer, replacing any TOC the
        // overview had loaded there.
        self.text_holds_toc = false;
        self.current_section_page_count = page_count.min(u16::MAX as usize) as u16;
    }

    pub fn set_section_partial(&mut self, partial: bool) {
        self.section_partial = partial;
    }

    pub fn set_cached_spine(&mut self, spine: u16) {
        self.cached_spine = spine;
    }

    pub fn set_book_index(
        &mut self,
        total_pages: u32,
        partial: bool,
        sections: &[BookV2SectionRecord],
    ) {
        self.book_total_pages = total_pages.max(1);
        self.book_cache_ready = true;
        self.book_cache_partial = partial;
        let count = sections.len().min(self.book_sections.len());
        // `chapter_start` runs parallel to `book_sections`; a rebuilt or
        // regrown section table (same book, same layout token) can slot
        // sections differently, so its marks must be re-derived. The index
        // also reloads unchanged on every section crossing, so only an actual
        // change may invalidate -- otherwise every crossing would re-read the
        // whole TOC from SD.
        if count != self.book_section_count
            || sections[..count]
                .iter()
                .zip(self.book_sections.iter())
                .any(|(new, old)| new != old)
        {
            self.chapter_start_ready = false;
        }
        self.book_section_count = count;
        for (index, record) in sections.iter().take(self.book_section_count).enumerate() {
            self.book_sections[index] = *record;
        }
        for index in self.book_section_count..self.book_sections.len() {
            self.book_sections[index] = EMPTY_BOOK_SECTION_RECORD;
        }
    }

    pub(crate) fn set_current_section_range(&mut self, start_page: u32, page_count: usize) {
        self.current_section_start_page = start_page;
        self.current_section_page_count = page_count.min(u16::MAX as usize) as u16;
    }

    /// True when `global_page` of catalog entry `index` is already
    /// renderable from the loaded in-RAM section window, so an open or
    /// extend request needs no SD session at all. Partial sections keep
    /// going to SD so the bounded prefix can be regrown.
    pub fn covers_global_page(&self, index: usize, global_page: u32) -> bool {
        self.loaded_index == Some(index)
            && matches!(self.reader_status, BookLoadStatus::Ready)
            && !self.text_holds_toc
            && !self.section_partial
            && self.page_count > 0
            && global_page >= self.current_section_start_page
            && global_page
                < self
                    .current_section_start_page
                    .saturating_add(self.current_section_page_count as u32)
    }

    pub(crate) fn local_page_for_global(&self, global_page: u32) -> usize {
        global_page
            .saturating_sub(self.current_section_start_page)
            .min(self.current_section_page_count.saturating_sub(1) as u32) as usize
    }

    pub(crate) fn section_for_global_page(&self, global_page: u32) -> Option<BookV2SectionRecord> {
        self.book_sections
            .iter()
            .take(self.book_section_count)
            .copied()
            .find(|section| {
                let start = section.start_page;
                let end = start.saturating_add(section.page_count.max(1) as u32);
                global_page >= start && global_page < end
            })
            .or_else(|| {
                self.book_sections
                    .iter()
                    .take(self.book_section_count)
                    .copied()
                    .last()
                    .filter(|section| global_page >= section.start_page)
            })
    }

    /// Page position within the chapter (spine item) containing
    /// `global_page`, as (one-based page in chapter, chapter page total).
    /// A long chapter now spans several cache sections, so the footer
    /// counter must aggregate every section sharing the page's spine rather
    /// than read a single section -- otherwise it resets mid-chapter at each
    /// chunk boundary. Returns None when there is no book index to aggregate
    /// (single in-RAM section), letting the caller fall back.
    pub fn chapter_page_position(&self, global_page: u32) -> Option<(u32, u32)> {
        let spine = self.section_for_global_page(global_page)?.spine;
        let mut start = u32::MAX;
        let mut total = 0u32;
        for section in self.book_sections.iter().take(self.book_section_count) {
            if section.spine == spine {
                start = start.min(section.start_page);
                total = total.saturating_add(section.page_count.max(1) as u32);
            }
        }
        if total == 0 {
            return None;
        }
        let current = global_page
            .saturating_sub(start)
            .saturating_add(1)
            .min(total);
        Some((current, total))
    }

    pub(crate) fn block_text(&self, index: usize) -> &str {
        let Some(record) = self.blocks.get(index) else {
            return "";
        };
        let start = record.text_offset as usize;
        let end = start.saturating_add(record.text_len as usize);
        core::str::from_utf8(self.text.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub(crate) fn block_record(&self, index: usize) -> Option<BlockRecord> {
        self.blocks
            .get(index)
            .copied()
            .filter(|_| index < self.block_count)
    }

    pub(crate) fn block_style(&self, index: usize) -> FontStyle {
        self.block_styles
            .get(index)
            .copied()
            .unwrap_or(FontStyle::Regular)
    }

    pub fn advertised_page_count(&self) -> u32 {
        self.book_total_pages.max(self.page_count.max(1) as u32)
    }

    pub(crate) fn toc_title(&self, index: usize) -> &str {
        let Some(record) = self.toc.get(index) else {
            return "";
        };
        let start = record.title_offset as usize;
        let end = start.saturating_add(record.title_len as usize);
        core::str::from_utf8(self.toc_text.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub fn toc_count(&self) -> usize {
        self.toc_count
    }

    pub fn toc_item(&self, index: usize) -> Option<TocItem<'_>> {
        if index >= self.toc_count {
            return None;
        }
        Some(TocItem {
            title: self.toc_title(index),
            level: self.toc[index].level.max(1),
            page: u32::from(self.toc_page[index]) + 1,
        })
    }

    pub fn push_toc_record(&mut self, title: &str, level: u8, spine_index: i16) -> bool {
        if self.toc_count >= self.toc.len() {
            return false;
        }
        let title_offset = self.toc_text_len;
        if !self.append_toc_text(title) {
            return false;
        }
        // The spine target is resolved before push and carried by the record,
        // so href/anchor text is never read back. The whole toc_text budget
        // goes to titles; records keep empty href/anchor ranges so the cache
        // format stays unchanged.
        let empty_offset = self.toc_text_len as u32;
        self.toc[self.toc_count] = TocRecord {
            title_offset: title_offset as u32,
            title_len: title.len().min(u16::MAX as usize) as u16,
            href_offset: empty_offset,
            href_len: 0,
            anchor_offset: empty_offset,
            anchor_len: 0,
            level,
            spine_index,
        };
        self.toc_page[self.toc_count] = 0;
        self.toc_count += 1;
        true
    }

    /// Mark `text` as holding a window of the on-disk TOC records, loaded for
    /// the Chapters overview: `len` records starting at absolute chapter
    /// `start`, out of `total` on disk. The reading section is reloaded on
    /// exit because its content was overwritten.
    pub(crate) fn set_toc_window(&mut self, start: usize, len: usize, total: usize) {
        self.toc_window_start = start;
        self.toc_window_len = len;
        self.toc_total = total;
        self.text_holds_toc = true;
    }

    pub fn text_holds_toc(&self) -> bool {
        self.text_holds_toc
    }

    pub fn toc_window_start(&self) -> usize {
        self.toc_window_start
    }

    /// Whether the resident TOC window covers `need` chapters from absolute
    /// index `start` (clamped to the on-disk total).
    pub fn toc_window_covers(&self, start: usize, need: usize) -> bool {
        let end = (start + need).min(self.toc_total);
        self.text_holds_toc
            && start >= self.toc_window_start
            && end <= self.toc_window_start + self.toc_window_len
    }

    /// TOC record byte range in `text` for absolute chapter `index`, when the
    /// resident window holds it.
    fn toc_record_base(&self, index: usize) -> Option<usize> {
        if !self.text_holds_toc {
            return None;
        }
        let rel = index.checked_sub(self.toc_window_start)?;
        if rel >= self.toc_window_len {
            return None;
        }
        let base = rel * TOC_CHAPTER_RECORD_BYTES;
        (base + TOC_CHAPTER_RECORD_BYTES <= self.text.len()).then_some(base)
    }

    /// Chapters to show in the overview: the full on-disk count when the TOC
    /// is loaded, else the resident count.
    pub fn overview_chapter_count(&self) -> usize {
        if self.text_holds_toc {
            self.toc_total
        } else {
            self.toc_count
        }
    }

    /// Title of overview chapter `index` (absolute), read straight from the
    /// TOC records in `text` (borrowed, no copy).
    pub fn overview_title_at(&self, index: usize) -> &str {
        let Some(base) = self.toc_record_base(index) else {
            return "";
        };
        let title_len = (self.text[base + 3] as usize).min(TOC_CHAPTER_TITLE_BYTES);
        core::str::from_utf8(&self.text[base + 4..base + 4 + title_len]).unwrap_or("")
    }

    pub fn overview_level_at(&self, index: usize) -> u8 {
        match self.toc_record_base(index) {
            Some(base) => self.text[base + 2],
            None => 1,
        }
    }

    pub(crate) fn overview_spine_at(&self, index: usize) -> i16 {
        match self.toc_record_base(index) {
            Some(base) => i16::from_le_bytes([self.text[base], self.text[base + 1]]),
            None => -1,
        }
    }

    /// Global page a chapter starts on, computed from the section index by
    /// its spine -- so no resident chapter-page array is needed.
    pub(crate) fn page_for_spine(&self, spine: u16) -> u32 {
        self.book_sections
            .iter()
            .take(self.book_section_count)
            .find(|section| section.spine == spine)
            .map(|section| section.start_page)
            .unwrap_or(0)
    }

    pub fn overview_page_at(&self, index: usize) -> u16 {
        let spine = self.overview_spine_at(index);
        if spine < 0 {
            return 0;
        }
        self.page_for_spine(spine as u16).min(u16::MAX as u32) as u16
    }

    /// The chapter a page falls in, resolved over the per-section chapter
    /// marks -- covers the whole on-disk TOC (chapter starts are section
    /// starts), so it keeps advancing past the 128-entry resident/event caps
    /// and past chapter 255.
    pub fn current_chapter_for_page(&self, page: u32) -> u16 {
        // Dropped marks (a cleared or rebuilt section table) must never pair
        // with the current sections, even if a caller skips the ready check.
        if !self.chapter_start_ready {
            return 0;
        }
        let count = self.book_section_count.min(MAX_BOOK_SECTIONS);
        proto::cache::chapter_for_page(
            &self.chapter_start[..count],
            &self.book_sections[..count],
            page,
        )
    }

    pub fn set_current_chapter(&mut self, chapter: u16, title: &str, source: (u32, u32)) {
        self.current_chapter = chapter;
        self.current_chapter_source = source;
        self.current_chapter_title.clear();
        for ch in title.chars() {
            if self.current_chapter_title.push(ch).is_err() {
                break;
            }
        }
    }

    /// The catalog row the reader is on, if any. Read-only from outside: the
    /// only writer is [`Self::set_current_index`], which declines an index past
    /// the catalog total, and bypassing that check is how `selected_cover` and
    /// the Library window end up pointing at a row that does not exist.
    pub fn current_index(&self) -> Option<usize> {
        self.current_index
    }

    pub fn current_chapter(&self) -> u16 {
        self.current_chapter
    }

    pub fn current_chapter_title(&self) -> &str {
        self.current_chapter_title.as_str()
    }

    /// Source identity of the book `current_chapter_title` was resolved for.
    pub fn current_chapter_source(&self) -> (u32, u32) {
        self.current_chapter_source
    }

    fn append_toc_text(&mut self, value: &str) -> bool {
        let bytes = value.as_bytes();
        if self.toc_text_len + bytes.len() > self.toc_text.len()
            || self.toc_text_len > u16::MAX as usize
            || bytes.len() > u16::MAX as usize
        {
            return false;
        }
        self.toc_text[self.toc_text_len..self.toc_text_len + bytes.len()].copy_from_slice(bytes);
        self.toc_text_len += bytes.len();
        true
    }

    pub fn active_book_labels<'a>(
        &'a self,
        book_id: u32,
        fallback_title: &'a str,
        fallback_author: &'a str,
    ) -> (&'a str, &'a str) {
        if !ReaderSource::from_book_id(book_id).is_sd() {
            return (fallback_title, fallback_author);
        }
        if self.reader_status == BookLoadStatus::Ready
            && self.loaded_index == Self::selected_book_index(book_id)
        {
            let title = if self.title.is_empty() {
                fallback_title
            } else {
                self.title.as_str()
            };
            let author = if self.author.is_empty() {
                fallback_author
            } else {
                self.author.as_str()
            };
            return (title, author);
        }
        Self::selected_book_index(book_id)
            .and_then(|index| self.catalog_entry(index))
            .map(|entry| (entry.display_label.as_str(), ""))
            .unwrap_or((fallback_title, fallback_author))
    }

    pub fn selected_cover(&self, book_id: u32) -> Option<ReaderCover<'_>> {
        if !ReaderSource::from_book_id(book_id).is_sd()
            || self.current_index != Self::selected_book_index(book_id)
            || !self.cover_ready
        {
            return None;
        }
        Some(ReaderCover {
            width: self.cover_width,
            height: self.cover_height,
            stride: COVER_STRIDE as u16,
            bits: &self.cover_bits,
        })
    }

    pub fn reader_status(&self) -> BookLoadStatus {
        self.reader_status
    }

    pub fn reader_error(&self) -> &str {
        self.error.as_str()
    }

    pub fn chapter_count_for_ui(&self) -> u16 {
        // The full on-disk count (when known) drives the overview's
        // selection range; it can run past the 128-entry resident/event
        // caps, so only the u16 message width bounds it.
        let count = if self.toc_total > 0 {
            self.toc_total
        } else if self.toc_count > 0 {
            self.toc_count
        } else {
            self.book_section_count
        };
        count.min(u16::MAX as usize).max(1) as u16
    }

    pub(crate) fn set_current_index(&mut self, index: usize) {
        if index < self.total as usize {
            self.current_index = Some(index);
        }
    }

    pub fn push_line_block(
        &mut self,
        line: &str,
        style: FontStyle,
        role: TextRole,
        align: TextAlign,
        paragraph_end: bool,
        spine_index: u16,
    ) -> bool {
        let line = line.trim();
        if line.is_empty() || self.block_count >= self.blocks.len() {
            return true;
        }
        let start = self.text_len;
        let bytes = line.as_bytes();
        if start + bytes.len() > self.text.len() || bytes.len() > u16::MAX as usize {
            return false;
        }
        self.text[start..start + bytes.len()].copy_from_slice(bytes);
        self.text_len += bytes.len();
        self.blocks[self.block_count] = BlockRecord {
            text_offset: start as u32,
            text_len: bytes.len() as u16,
            line_count: 1,
            role,
            style: proto_style_for_display_style(style),
            align,
        };
        self.block_styles[self.block_count] = style;
        self.block_spine[self.block_count] = spine_index;
        self.block_page_break_before[self.block_count] =
            should_break_before_block(role, self.blocks.get(self.block_count.wrapping_sub(1)));
        self.block_paragraph_end[self.block_count] = paragraph_end;
        // A line opens a paragraph when the previous block closed one (and the
        // first block always does). This mirrors the indent budget the build
        // wrap charged this line, so draw and pagination agree.
        self.block_paragraph_start[self.block_count] = self.block_count == 0
            || self
                .block_paragraph_end
                .get(self.block_count.wrapping_sub(1))
                .copied()
                .unwrap_or(true);
        self.block_count += 1;
        true
    }
}

/// Populate a catalog entry slot from a record's fields, deriving the display
/// label. Shared by the list window and the active-book entry; both are read
/// straight from CATALOG.BIN, which already carries the stored `source_hash`.
///
/// `label_override` is the EPUB title saved when the book was last opened: when
/// present it becomes the display label, so a book whose on-disk name can't
/// carry a real title (an 8.3 upload name) still reads as its title. With no
/// override the label falls back to the prettified file stem.
fn fill_entry(
    entry: &mut LibraryBookEntry,
    display_name: &str,
    byte_size: u32,
    source_hash: u32,
    label_override: Option<&str>,
) {
    entry.display_name.clear();
    entry.display_label.clear();
    let _ = entry.display_name.push_str(display_name);
    match label_override {
        Some(label) if !label.is_empty() => {
            let _ = entry.display_label.push_str(label);
        }
        _ => derive_catalog_label(display_name, &mut entry.display_label),
    }
    entry.byte_size = byte_size;
    entry.source_hash = source_hash;
}

/// Overwrite a resident entry's display label with `label`. Used to bake a
/// freshly-loaded book's title into the list without a card re-read.
fn copy_label(out: &mut String<64>, label: &str) {
    out.clear();
    let _ = out.push_str(label);
}

fn should_break_before_block(role: TextRole, previous: Option<&BlockRecord>) -> bool {
    is_major_heading(role)
        && previous
            .map(|record| !is_major_heading(record.role))
            .unwrap_or(false)
}

fn is_major_heading(role: TextRole) -> bool {
    matches!(role, TextRole::Heading1 | TextRole::Heading2)
}

fn copy_string<const N: usize>(out: &mut String<N>, value: &str) {
    out.clear();
    for ch in value.chars() {
        if out.push(ch).is_err() {
            break;
        }
    }
}

fn proto_style_for_display_style(style: FontStyle) -> proto::text::FontStyle {
    match style {
        FontStyle::Regular => proto::text::FontStyle::Regular,
        FontStyle::Italic => proto::text::FontStyle::Italic,
        FontStyle::Bold => proto::text::FontStyle::Bold,
        FontStyle::BoldItalic => proto::text::FontStyle::BoldItalic,
    }
}

impl ui::reading::ReadingBlocks for ReaderStore {
    fn block_count(&self) -> usize {
        self.block_count
    }

    fn block(&self, index: usize) -> Option<BlockRecord> {
        self.block_record(index)
    }

    fn block_text(&self, index: usize) -> &str {
        ReaderStore::block_text(self, index)
    }

    fn block_style(&self, index: usize) -> FontStyle {
        ReaderStore::block_style(self, index)
    }

    fn page_break_before(&self, index: usize) -> bool {
        self.block_page_break_before
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    fn paragraph_end(&self, index: usize) -> bool {
        self.block_paragraph_end.get(index).copied().unwrap_or(true)
    }

    fn paragraph_start(&self, index: usize) -> bool {
        self.block_paragraph_start
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    fn type_settings(&self) -> TypeSettings {
        self.type_settings
    }

    fn page_box(&self) -> ui::reading::PageBox {
        ReaderStore::page_box(self)
    }
}

pub fn chapter_pages_for_event(store: &ReaderStore) -> [u16; MAX_SD_CHAPTERS] {
    let mut pages = [0u16; MAX_SD_CHAPTERS];
    if store.toc_count > 0 {
        let count = store
            .toc_count
            .min(MAX_SD_TOC_ITEMS)
            .min(MAX_PUBLISHED_CHAPTER_EVENTS)
            .min(u8::MAX as usize);
        pages[..count].copy_from_slice(&store.toc_page[..count]);
    } else {
        let count = store
            .book_section_count
            .min(MAX_SD_TOC_ITEMS)
            .min(MAX_PUBLISHED_CHAPTER_EVENTS)
            .min(u8::MAX as usize);
        for (page, section) in pages.iter_mut().zip(&store.book_sections[..count]) {
            *page = section.start_page.min(u16::MAX as u32) as u16;
        }
    }
    pages
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;

    /// The open book's copy id is what per-copy state is addressed by, so it
    /// has to be resident for as long as the place beside it: a rescan
    /// rebuilds the catalog, which says where copies are, and changes
    /// nothing about which copy is which.
    #[test]
    fn the_active_book_carries_the_copy_id_its_row_was_adopted_under() {
        let mut store = Box::new(ReaderStore::new());
        assert_eq!(store.active_copy_id(), None, "nothing is open yet");
        let id = proto::identity::BookId::from_bytes([3u8; 16]).expect("an id");
        store.set_active_entry(
            4,
            "/books/Dune.epub",
            Some(proto::library_path::BookRoot::Library),
            "Dune.epub",
            3_000,
            0x1234_5678,
            None,
            Some(id),
        );
        assert_eq!(store.active_copy_id(), Some(id));
        assert_eq!(
            store.book_location(4),
            Some((proto::library_path::BookRoot::Library, "Dune.epub"))
        );

        store.clear_catalog();
        assert_eq!(
            store.active_copy_id(),
            Some(id),
            "a rescan does not change which copy the reader has open"
        );

        // A card whose ledger has not been over this row yet says so, rather
        // than leaving the last book's id standing.
        store.set_active_entry(
            5,
            "/books/Emma.epub",
            Some(proto::library_path::BookRoot::Library),
            "Emma.epub",
            2_000,
            0x9abc_def0,
            None,
            None,
        );
        assert_eq!(store.active_copy_id(), None);
    }

    /// Invariant: a move that fails leaves browsing exactly where it was.
    ///
    /// The failure word every reader of it takes to mean standstill is only
    /// honest if this holds. Without it the app keeps a depth and a row set
    /// for one folder while the store sits in another, and every later
    /// command lands somewhere the screen is not describing.
    #[test]
    fn a_restored_checkpoint_puts_browsing_back_exactly() {
        let mut store = Box::new(ReaderStore::new());
        store.set_folder_counts(upload_store::library::RowCounts {
            shelf_books: 2,
            root_books: 1,
            shelf_folders: 3,
        });
        store.browse_mut().set_count(6);
        store.browse_mut().move_by(4);
        store.begin_folder_page(0);
        store.push_folder_row(
            "Dune.epub",
            false,
            12,
            proto::library_path::BookRoot::Library,
        );

        let point = store.browse_checkpoint();
        let was = (
            store.browse().path().clone(),
            store.browse().selection(),
            store.browse().count(),
            store.folder_counts(),
        );

        // Descend, and list the folder that was entered.
        assert_eq!(
            store
                .browse_mut()
                .choose("Fiction", app_core::browse::Row::Folder),
            app_core::browse::Chosen::Entered
        );
        store.set_folder_counts(upload_store::library::RowCounts {
            shelf_books: 9,
            root_books: 0,
            shelf_folders: 0,
        });
        store.browse_mut().set_count(9);
        assert_ne!(store.browse().path().as_str(), was.0.as_str());

        // The listing fails, so the move is undone.
        store.restore_browse(point);
        assert_eq!(store.browse().path().as_str(), was.0.as_str());
        assert_eq!(store.browse().selection(), was.1);
        assert_eq!(store.browse().count(), was.2);
        assert_eq!(store.folder_counts(), was.3);
        assert!(
            store.folder_rows().is_empty(),
            "the rows are read again from the position that came back",
        );
    }

    /// Leaving is undone the same way, trail included: a return that could
    /// not list its parent must not have spent the row it was going back to.
    #[test]
    fn a_restored_checkpoint_puts_a_departure_back_too() {
        let mut store = Box::new(ReaderStore::new());
        store.browse_mut().set_count(4);
        store.browse_mut().move_by(2);
        assert_eq!(
            store
                .browse_mut()
                .choose("Fiction", app_core::browse::Row::Folder),
            app_core::browse::Chosen::Entered
        );
        store.browse_mut().set_count(3);
        store.browse_mut().move_by(1);

        let point = store.browse_checkpoint();
        let inside = store.browse().path().clone();
        assert!(store.browse_mut().leave());
        assert!(store.browse().is_root());

        store.restore_browse(point);
        assert_eq!(store.browse().path().as_str(), inside.as_str());
        assert_eq!(store.browse().selection(), 1);
        // And leaving still works afterwards, landing on the row it was
        // entered from: the trail came back with the path.
        assert!(store.browse_mut().leave());
        assert!(store.browse().is_root());
        store.browse_mut().set_count(4);
        assert_eq!(store.browse().selection(), 2);
    }

    /// Invariant: a trim/restore round trip returns all three arena counters.
    ///
    /// A unit test rather than an integration one because the counters are
    /// crate-private -- which is the point of them -- and this asserts the
    /// property directly rather than through a caller that happens to rebuild
    /// the page index straight afterwards and mask a partial restore.
    #[test]
    fn a_trim_and_restore_round_trip_returns_all_three_counters() {
        let mut store = Box::new(ReaderStore::new());
        store.block_count = 40;
        store.page_count = 5;
        store.text_len = 900;

        let tail = store.trim_to_page_boundary(30, 4);
        assert_eq!(store.block_count, 30, "the trim should cut the blocks");
        assert_eq!(store.page_count, 4, "the trim should drop the partial page");
        assert_eq!(
            store.text_len, 0,
            "the trim should follow the cut block's text offset"
        );

        store.restore_trimmed(tail);
        assert_eq!(
            (store.block_count, store.page_count, store.text_len),
            (40, 5, 900),
            "all three counters describe one arena and must come back together"
        );
    }
}
