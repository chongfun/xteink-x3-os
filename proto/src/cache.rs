use crate::text::{FontStyle, TextAlign, TextRole};
use heapless::String;

pub const CACHE_MAGIC: u32 = 0x5834_5244; // X4RD
pub const CACHE_VERSION: u16 = 1;
// Bumped 21 -> 23 with the spine-cap fix. A long book cached under the old
// 96-item spine cap was written with partial=false (truncation never tripped
// book_partial), so it would load as a clean hit and keep stranding the tail
// chapters on patched firmware. Rejecting the old versions forces a one-time,
// lazy, per-book re-paginate; surviving chapters lay out identically, so
// chapter-keyed positions carry over.
//
// Bumped 23 -> 24 with the EPUB 3 nav page-list fix. Bumped 24 -> 25 when the
// nav classifier widened beyond `epub:type` to also reject page lists,
// landmarks, lists of illustrations/tables, and back-matter index navs marked
// by role/label/id/class. A book already indexed on buggy firmware loads via
// the fast book-cache path, which never re-parses the nav or rewrites TOC.BIN,
// so the bogus chapter list would persist. Rejecting v24 forces the one-time
// rebuild that re-runs the corrected nav parser; the re-paginate is incidental
// and chapter-keyed positions carry over.
//
// Bumped 25 -> 26 when custom font identity joined v2 book/section headers.
// A custom pack replacement can keep the same FontFamily::Custom setting while
// changing metrics, so the cache key needs the pack hash too.
//
// NOT bumped for `BookV2Header::resume_spine`, deliberately: it took a field
// every v26 writer filled with an explicit zero, and zero is exactly what the
// reader must conclude about an index written before the field meant anything
// ("no build is coming back for this"). Bumping would have thrown away every
// cache on every card to add a value they already carry correctly. Only do the
// same for a field whose old bytes are a *provably* fixed constant, and pin it
// with a test the way `book_v2_header` does.
pub const CACHE_V2_VERSION: u16 = 26;
const CACHE_V2_COMPAT_VERSION: u16 = 26;
/// Everything this firmware keeps on the card, under one directory.
///
/// Named for the reader rather than for a board: the same firmware runs on
/// more than one vendor's hardware, and a card written by one of them should
/// not be stamped with another's name.
pub const CACHE_ROOT_DIR: &str = "READER";
/// The catalog snapshot, under the cache root. Named here because both the
/// scan that writes it and the upload session that must prove it gone need
/// to mean the same file.
pub const CATALOG_FILE: &str = "CATALOG.BIN";
pub const CACHE_DIR: &str = "CACHE";
pub const CACHE_V2_DIR: &str = "CACHE2";
pub const CACHE_SECTIONS_DIR: &str = "SECTIONS";
pub const CACHE_BOOK_FILE: &str = "BOOK.BIN";
pub const CACHE_COVER_FILE: &str = "COVER.BIN";
pub const CACHE_STATE_FILE: &str = "STATE.BIN";
pub const CACHE_KEY_BYTES: usize = 8;
pub const CACHE_SECTION_FILE_BYTES: usize = 8;
pub const BOOK_HEADER_BYTES: usize = 16;
pub const SPINE_RECORD_BYTES: usize = 12;
pub const TOC_RECORD_BYTES: usize = 24;
pub const SECTION_HEADER_BYTES: usize = 40;
pub const SECTION_V2_HEADER_BYTES: usize = 56;
pub const BOOK_V2_HEADER_BYTES: usize = 56;
pub const BOOK_V2_SECTION_RECORD_BYTES: usize = 16;
pub const PAGE_HEADER_BYTES: usize = 28;
pub const PAGE_RECORD_BYTES: usize = 4;
pub const LINE_RECORD_BYTES: usize = 12;
pub const WORD_RECORD_BYTES: usize = 12;
pub const BLOCK_RECORD_BYTES: usize = 12;
pub const COVER_MAGIC: &[u8; 4] = b"X4CV";
pub const COVER_VERSION: u8 = 1;
pub const COVER_HEADER_BYTES: usize = 12;
pub const COVER_WIDTH: usize = 202;
pub const COVER_HEIGHT: usize = 303;
pub const COVER_STRIDE: usize = COVER_WIDTH.div_ceil(8);
pub const COVER_BYTES: usize = COVER_STRIDE * COVER_HEIGHT;
/// Chapter list (TOC) cache, kept on disk so the full table of contents
/// never has to be resident -- a long book's TOC (HPMOR's runs to a couple
/// hundred entries) would otherwise blow the tight reader RAM budget. Fixed
/// 48-byte records keep it randomly addressable: chapter `i` lives at
/// `TOC_FILE_HEADER_BYTES + i * TOC_CHAPTER_RECORD_BYTES`.
pub const CACHE_TOC_FILE: &str = "TOC.BIN";
pub const TOC_FILE_MAGIC: u32 = 0x5834_5443; // X4TC
                                             // v2: chapter title budget grew 44->60 bytes (record 48->64). 64-byte records
                                             // keep a 256-record overview window (TOC_WINDOW_CAPACITY) fitting the 16KB
                                             // overview text buffer exactly (256*64 == 16384). A v1 TOC.BIN is rejected
                                             // here and rebuilt (chapter-list re-parse only, no re-pagination).
pub const TOC_FILE_VERSION: u16 = 2;
pub const TOC_FILE_HEADER_BYTES: usize = 16;
pub const TOC_CHAPTER_TITLE_BYTES: usize = 60;
pub const TOC_CHAPTER_RECORD_BYTES: usize = 64;

/// Settings-independent content cache: the exact `push_block` argument
/// stream captured during a full EPUB build (fragment text plus role,
/// style, align, paragraph_end, and spine boundaries). A type-settings
/// change replays this file through the same build sink instead of
/// re-reading, re-inflating, and re-parsing the whole EPUB. Keyed by
/// source identity and `CONTENT_VERSION` only — never by layout config.
/// Bump `CONTENT_VERSION` whenever XHTML parsing, entity decoding, or sink
/// normalization semantics change (the `READER_LAYOUT_VERSION` discipline).
/// The header also stamps the `CACHE_V2_VERSION` in force at capture time
/// and decoding rejects any other value, so a BOOK.BIN version bump —
/// even one the `CACHE_V2_COMPAT_VERSION` window would accept — can never
/// replay a stream captured under older parse semantics.
pub const CACHE_CONTENT_FILE: &str = "CONT.BIN";
pub const CONTENT_MAGIC: u32 = 0x5834_434E; // X4CN
pub const CONTENT_VERSION: u16 = 3;
pub const CONTENT_HEADER_BYTES: usize = 24;
pub const CONTENT_RECORD_HEADER_BYTES: usize = 8;
const CONTENT_FLAG_COMPLETE: u8 = 1;
const CONTENT_RECORD_FLAG_PARAGRAPH_END: u8 = 1;
const CONTENT_RECORD_FLAG_SPINE_END: u8 = 1 << 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentHeader {
    pub source_hash: u32,
    pub source_size: u32,
    /// False until the capture has recorded the whole spine walk; replay
    /// only ever runs from a complete capture.
    pub complete: bool,
    pub spine_count: u16,
    pub content_len: u32,
}

/// One captured `push_block` call (text follows the header), or — with
/// `spine_end` set and `text_len` 0 — the end-of-spine-item marker that
/// tells replay to finish the current section run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRecordHeader {
    pub spine_index: u16,
    pub text_len: u16,
    pub role: TextRole,
    pub style: FontStyle,
    pub align: TextAlign,
    pub paragraph_end: bool,
    pub spine_end: bool,
}

pub fn encode_content_header(header: ContentHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, CONTENT_HEADER_BYTES)?;
    write_u32(out, 0, CONTENT_MAGIC);
    write_u16(out, 4, CONTENT_VERSION);
    out[6] = if header.complete {
        CONTENT_FLAG_COMPLETE
    } else {
        0
    };
    out[7] = 0;
    write_u32(out, 8, header.source_hash);
    write_u32(out, 12, header.source_size);
    write_u16(out, 16, header.spine_count);
    write_u16(out, 18, CACHE_V2_VERSION);
    write_u32(out, 20, header.content_len);
    Ok(CONTENT_HEADER_BYTES)
}

pub fn decode_content_header(input: &[u8]) -> Result<ContentHeader, CacheError> {
    require(input, CONTENT_HEADER_BYTES)?;
    if read_u32(input, 0)? != CONTENT_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != CONTENT_VERSION {
        return Err(CacheError::BadVersion);
    }
    // Exact match, not the BOOK.BIN compat window: any CACHE_V2_VERSION
    // bump invalidates captured content even when old indexes stay
    // readable, so a semantics change can't leak through replay.
    if read_u16(input, 18)? != CACHE_V2_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(ContentHeader {
        source_hash: read_u32(input, 8)?,
        source_size: read_u32(input, 12)?,
        complete: input[6] & CONTENT_FLAG_COMPLETE != 0,
        spine_count: read_u16(input, 16)?,
        content_len: read_u32(input, 20)?,
    })
}

pub fn encode_content_record_header(
    record: ContentRecordHeader,
    out: &mut [u8],
) -> Result<usize, CacheError> {
    require(out, CONTENT_RECORD_HEADER_BYTES)?;
    if record.spine_end && record.text_len != 0 {
        return Err(CacheError::BadLength);
    }
    write_u16(out, 0, record.spine_index);
    write_u16(out, 2, record.text_len);
    out[4] = role_byte(record.role);
    out[5] = style_byte(record.style);
    out[6] = align_byte(record.align);
    out[7] = (u8::from(record.paragraph_end) * CONTENT_RECORD_FLAG_PARAGRAPH_END)
        | (u8::from(record.spine_end) * CONTENT_RECORD_FLAG_SPINE_END);
    Ok(CONTENT_RECORD_HEADER_BYTES)
}

pub fn decode_content_record_header(input: &[u8]) -> Result<ContentRecordHeader, CacheError> {
    require(input, CONTENT_RECORD_HEADER_BYTES)?;
    let record = ContentRecordHeader {
        spine_index: read_u16(input, 0)?,
        text_len: read_u16(input, 2)?,
        role: role_from_byte(input[4])?,
        style: style_from_byte(input[5])?,
        align: align_from_byte(input[6])?,
        paragraph_end: input[7] & CONTENT_RECORD_FLAG_PARAGRAPH_END != 0,
        spine_end: input[7] & CONTENT_RECORD_FLAG_SPINE_END != 0,
    };
    if record.spine_end && record.text_len != 0 {
        return Err(CacheError::BadLength);
    }
    Ok(record)
}

/// A content-stream walk failure: the stream is truncated, corrupt, or the
/// underlying reader failed. Every case gets the same treatment — delete
/// the file and fall back to the full build — so the error carries no
/// detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentReplayError;

/// Driver verdict after one replayed spine group: keep walking, or stop
/// early and publish what's built so far (the same early-out a full build
/// takes when section capacity runs out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentReplayFlow {
    Continue,
    Stop,
}

/// How a content-stream walk ended: `Complete` means every expected spine
/// group replayed and the stream ended exactly on the final marker;
/// `Stopped` means the driver ended the walk early via
/// [`ContentReplayFlow::Stop`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentReplayOutcome {
    Complete,
    Stopped,
}

/// Ensure at least `need` bytes sit at `buf[*pos..*fill]`, compacting the
/// buffered remainder to the front and refilling from `read`. `Ok(false)`
/// is a clean end-of-stream exactly on a record boundary; an error is a
/// short stream mid-record or a read failure.
fn refill_content_buf(
    read: &mut dyn FnMut(&mut [u8]) -> Result<usize, ContentReplayError>,
    buf: &mut [u8],
    pos: &mut usize,
    fill: &mut usize,
    need: usize,
) -> Result<bool, ContentReplayError> {
    if *fill - *pos >= need {
        return Ok(true);
    }
    buf.copy_within(*pos..*fill, 0);
    *fill -= *pos;
    *pos = 0;
    while *fill < need {
        let read_len = read(&mut buf[*fill..])?;
        if read_len == 0 {
            return if *fill == 0 {
                Ok(false)
            } else {
                Err(ContentReplayError)
            };
        }
        *fill += read_len;
    }
    Ok(true)
}

/// Hands the driver one spine group's records in order. Every violation of
/// the framing discipline — EOF mid-group, a record from another spine
/// index, undecodable bytes, or text that can't fit the buffer — is an
/// error: a complete capture never produces any of them.
pub struct ContentGroupReader<'w> {
    read: &'w mut dyn FnMut(&mut [u8]) -> Result<usize, ContentReplayError>,
    buf: &'w mut [u8],
    pos: &'w mut usize,
    fill: &'w mut usize,
    group_spine: u16,
    done: bool,
}

impl ContentGroupReader<'_> {
    /// The next block in this group: `Ok(Some((record, text)))` for a
    /// captured `push_block`, `Ok(None)` once at the group's spine-end
    /// marker, an error on any framing or read failure. The text borrows
    /// the walk buffer, so consume it before the next call.
    pub fn next_block(
        &mut self,
    ) -> Result<Option<(ContentRecordHeader, &str)>, ContentReplayError> {
        if self.done {
            return Ok(None);
        }
        // A complete capture never ends mid-group: EOF here is corrupt.
        if !refill_content_buf(
            self.read,
            self.buf,
            self.pos,
            self.fill,
            CONTENT_RECORD_HEADER_BYTES,
        )? {
            return Err(ContentReplayError);
        }
        let record = decode_content_record_header(
            &self.buf[*self.pos..*self.pos + CONTENT_RECORD_HEADER_BYTES],
        )
        .map_err(|_| ContentReplayError)?;
        if record.spine_index != self.group_spine {
            return Err(ContentReplayError);
        }
        *self.pos += CONTENT_RECORD_HEADER_BYTES;
        if record.spine_end {
            self.done = true;
            return Ok(None);
        }
        let text_len = record.text_len as usize;
        if text_len > self.buf.len() {
            return Err(ContentReplayError);
        }
        if !refill_content_buf(self.read, self.buf, self.pos, self.fill, text_len)? {
            return Err(ContentReplayError);
        }
        let start = *self.pos;
        *self.pos += text_len;
        let text = core::str::from_utf8(&self.buf[start..start + text_len])
            .map_err(|_| ContentReplayError)?;
        Ok(Some((record, text)))
    }
}

/// Walk a CONT.BIN record stream (positioned just past the file header),
/// calling `on_group` once per spine group with a [`ContentGroupReader`]
/// the driver drains. This is the single owner of the stream's framing
/// discipline — both the firmware replay path and the host tests drive
/// this walker, so writer and reader semantics cannot drift apart:
///
/// - every group ends in a spine-end marker and never changes spine index
///   mid-group;
/// - group spine indices strictly increase (gaps are fine — the capture
///   skips navigation items): the writer walks the spine in `enumerate()`
///   order and its indices can't saturate (`MAX_SPINE_ITEMS` is far below
///   `u16::MAX`), so a reordered or duplicated group is corruption that
///   would otherwise publish chapters out of order;
/// - clean EOF is only accepted on a group boundary with exactly
///   `expected_spines` groups replayed;
/// - a driver returning [`ContentReplayFlow::Continue`] must have drained
///   its group to the marker.
///
/// `read` fills a buffer from the stream (`Ok(0)` at EOF); `buf` is the
/// walk window and bounds the largest replayable text (the capture side
/// enforces the same bound at write time).
pub fn replay_content_stream(
    read: &mut dyn FnMut(&mut [u8]) -> Result<usize, ContentReplayError>,
    buf: &mut [u8],
    expected_spines: u16,
    on_group: &mut dyn FnMut(
        u16,
        &mut ContentGroupReader<'_>,
    ) -> Result<ContentReplayFlow, ContentReplayError>,
) -> Result<ContentReplayOutcome, ContentReplayError> {
    let mut pos = 0usize;
    let mut fill = 0usize;
    let mut replayed_spines = 0u16;
    let mut last_group_spine: Option<u16> = None;
    loop {
        // Peek the next group's spine index; clean EOF here ends the walk.
        if !refill_content_buf(read, buf, &mut pos, &mut fill, CONTENT_RECORD_HEADER_BYTES)? {
            break;
        }
        let group_spine =
            decode_content_record_header(&buf[pos..pos + CONTENT_RECORD_HEADER_BYTES])
                .map_err(|_| ContentReplayError)?
                .spine_index;
        // Rejected before the group runs, so a reordered stream can't
        // publish a single section in the wrong place.
        if last_group_spine.is_some_and(|last| group_spine <= last) {
            return Err(ContentReplayError);
        }
        let mut group = ContentGroupReader {
            read: &mut *read,
            buf: &mut *buf,
            pos: &mut pos,
            fill: &mut fill,
            group_spine,
            done: false,
        };
        match on_group(group_spine, &mut group)? {
            ContentReplayFlow::Stop => return Ok(ContentReplayOutcome::Stopped),
            ContentReplayFlow::Continue => {}
        }
        if !group.done {
            return Err(ContentReplayError);
        }
        last_group_spine = Some(group_spine);
        replayed_spines = replayed_spines.saturating_add(1);
    }
    if replayed_spines != expected_spines {
        return Err(ContentReplayError);
    }
    Ok(ContentReplayOutcome::Complete)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheError {
    BufferTooSmall,
    BadMagic,
    BadVersion,
    BadLength,
    Utf8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookCacheHeader {
    pub spine_count: u16,
    pub toc_count: u16,
    pub string_bytes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpineRecord {
    pub href_offset: u32,
    pub href_len: u16,
    pub toc_index: i16,
    pub byte_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TocRecord {
    pub title_offset: u32,
    pub title_len: u16,
    pub href_offset: u32,
    pub href_len: u16,
    pub anchor_offset: u32,
    pub anchor_len: u16,
    pub level: u8,
    pub spine_index: i16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    pub page_count: u16,
    pub block_count: u16,
    pub line_count: u16,
    pub word_count: u16,
    pub text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub bytes_consumed: u32,
    pub total_bytes: u32,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionV2Header {
    pub source_hash: u32,
    pub source_size: u32,
    pub spine: u16,
    pub page_count: u16,
    pub block_count: u16,
    pub text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub custom_font_identity: u64,
    pub bytes_consumed: u32,
    pub total_bytes: u32,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookV2Header {
    pub source_hash: u32,
    pub source_size: u32,
    pub total_pages: u32,
    pub section_count: u16,
    pub spine_count: u16,
    pub toc_count: u16,
    pub toc_text_bytes: u32,
    pub title_text_bytes: u32,
    pub author_text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub custom_font_identity: u64,
    pub partial: bool,
    /// The spine item a progressive build was going to walk next when it
    /// published this index early, or `0` for an index nothing is still
    /// building — complete, or partial for a reason no further walking would
    /// fix (a spine clipped at `MAX_SPINE_ITEMS`, sections at capacity).
    ///
    /// `partial` alone cannot carry this. It says "pages are missing", not
    /// "someone is still coming for them", and the difference decides whether
    /// a reader who opens this cache is fenced in behind a page count nothing
    /// will ever raise. A build abandoned by sleep or a reboot leaves exactly
    /// that, and the reader cannot ask for the first missing page to provoke a
    /// rebuild, because the page count is what the request is clamped to.
    ///
    /// Written into a field that has always been an explicit zero, so an index
    /// from before this existed reads as `0` — "nothing is building it" — which
    /// is both true and the safe answer. No version bump, no migration.
    /// A real cursor is never `0`: it is the index *after* a walked item.
    pub resume_spine: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookV2SectionRecord {
    pub section: u16,
    pub spine: u16,
    pub start_page: u32,
    pub page_count: u16,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCacheHeader {
    pub page_count: u16,
    pub block_count: u16,
    pub text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub bytes_consumed: u32,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRecord {
    pub first_block: u16,
    pub block_count: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRecord {
    pub first_word: u16,
    pub word_count: u16,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub align: TextAlign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordRecord {
    pub text_offset: u32,
    pub text_len: u16,
    pub x: i16,
    pub width: u16,
    pub style: FontStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRecord {
    pub text_offset: u32,
    pub text_len: u16,
    pub line_count: u8,
    pub role: TextRole,
    pub style: FontStyle,
    pub align: TextAlign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverCacheHeader {
    pub width: u16,
    pub height: u16,
    pub stride: u16,
}

impl CoverCacheHeader {
    pub const fn x4_dock_clean() -> Self {
        Self {
            width: COVER_WIDTH as u16,
            height: COVER_HEIGHT as u16,
            stride: COVER_STRIDE as u16,
        }
    }
}

pub fn book_cache_size(header: BookCacheHeader) -> usize {
    BOOK_HEADER_BYTES
        + header.spine_count as usize * SPINE_RECORD_BYTES
        + header.toc_count as usize * TOC_RECORD_BYTES
        + header.string_bytes as usize
}

pub fn page_cache_size(header: PageCacheHeader) -> usize {
    PAGE_HEADER_BYTES
        + header.page_count as usize * PAGE_RECORD_BYTES
        + header.block_count as usize * BLOCK_RECORD_BYTES
        + header.text_bytes as usize
}

pub fn section_cache_size(header: SectionHeader) -> usize {
    SECTION_HEADER_BYTES
        + header.page_count as usize * PAGE_RECORD_BYTES
        + header.block_count as usize * BLOCK_RECORD_BYTES
        + header.block_count as usize
        + header.line_count as usize * LINE_RECORD_BYTES
        + header.word_count as usize * WORD_RECORD_BYTES
        + header.text_bytes as usize
}

pub fn section_v2_cache_size(header: SectionV2Header) -> usize {
    SECTION_V2_HEADER_BYTES
        + header.page_count as usize * PAGE_RECORD_BYTES
        + header.block_count as usize * BLOCK_RECORD_BYTES
        + header.block_count as usize
        + header.text_bytes as usize
}

pub fn book_v2_cache_size(header: BookV2Header) -> usize {
    BOOK_V2_HEADER_BYTES
        + header.section_count as usize * BOOK_V2_SECTION_RECORD_BYTES
        + header.toc_count as usize * TOC_RECORD_BYTES
        + header.toc_text_bytes as usize
        + header.title_text_bytes as usize
        + header.author_text_bytes as usize
}

/// The byte a root contributes to stored identity hashes. Frozen: changing a
/// value re-keys every cache and every persisted identity on every card.
const fn root_hash_byte(root: crate::library_path::BookRoot) -> u8 {
    match root {
        crate::library_path::BookRoot::Library => 0,
        crate::library_path::BookRoot::CardRoot => 1,
    }
}

/// FNV-1a identity of a book's source: where it is, and how large.
///
/// Hashes the root discriminant and the full locator, with a NUL after each
/// so adjacent fields cannot alias, then the size. The 64-byte display label
/// is deliberately not the input: a nested locator can spend the whole label
/// budget before reaching the filename, so two distinct books can share a
/// truncated label, and a label collision must not become a cache collision.
/// The label is presentation only.
pub fn source_hash_at(root: crate::library_path::BookRoot, locator: &str, byte_size: u32) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in core::iter::once(root_hash_byte(root))
        .chain(core::iter::once(0))
        .chain(locator.bytes())
        .chain(core::iter::once(0))
        .chain(byte_size.to_le_bytes())
    {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// The cache directory name for a book, as a pure function of its source
/// hash, so the key and the identity cannot disagree: whatever feeds the
/// hash names the cache. 28 bits of the hash, like the display-path key it
/// replaces; the header check on open still compares the whole hash and the
/// size, so a key collision reads as a miss rather than as another book.
pub fn cache_key_from(source_hash: u32) -> String<CACHE_KEY_BYTES> {
    let mut out = String::<CACHE_KEY_BYTES>::new();
    let _ = out.push('E');
    push_hex(&mut out, source_hash & 0x0FFF_FFFF, 7);
    out
}

/// The source hash firmware before catalog v8 derived for this book, when
/// one exists: FNV-1a over the display path plus the size. One frozen
/// historical rule behind two migrations: [`legacy_position_cache_key`] for
/// per-book positions, and the saved-state fallback in fw's
/// `find_index_by_identity`, where the global state record written by pre-v8
/// firmware carries this full 32-bit value.
///
/// Old firmware could only address a book sitting directly at the card root
/// or directly in `/BOOKS`, so only those display shapes have a legacy
/// identity: one path component after a frozen `/` or `/books/` prefix. A
/// nested book's display path fails the shape test and gets `None`, so a
/// nested path that happens to render like an old flat one cannot adopt
/// another book's state. The prefixes are spelled here rather than shared
/// with the scan because they are frozen history: the scan's spelling may
/// move, and this one may not.
pub fn legacy_source_hash(display_name: &str, byte_size: u32) -> Option<u32> {
    let rest = display_name
        .strip_prefix("/books/")
        .or_else(|| display_name.strip_prefix('/'))?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    let mut hash = 0x811c_9dc5u32;
    for byte in display_name.bytes().chain(byte_size.to_le_bytes()) {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    Some(hash)
}

/// The book a keyed cache access is on behalf of: the directory name and
/// the exact owner its claim must name. Carried together so no call can
/// pass a key while forgetting whose it is.
#[derive(Clone, Copy, Debug)]
pub struct CacheOwner<'a> {
    /// The directory name under `CACHE2`, from [`cache_key_from`].
    pub key: &'a str,
    pub root: crate::library_path::BookRoot,
    /// The root-relative locator, exactly as the catalog stores it.
    pub locator: &'a str,
}

/// The claim file naming which book owns a cache directory: the exact root
/// and locator, not their hash. The directory name and every artifact
/// identity are 32-bit hashes, and two legal locators can share one, so a
/// full-hash twin would otherwise pass every `(hash, size)` check and load
/// the other book's cache. The claim is what makes the directory belong to
/// one physical file.
pub const CACHE_CLAIM_FILE: &str = "WHO.BIN";

const CLAIM_MAGIC: [u8; 4] = *b"X4WH";
/// The claim as first written: who owns this directory, and nothing about
/// which physical file that owner is. Still read, so a card written by an
/// older build keeps its caches and its positions.
const CLAIM_VERSION_NAMED: u8 = 1;
/// Adds room for evidence about the file itself. A locator says where a book
/// was, which is exactly what a move invalidates, so the record has space for
/// the chain it occupies and the bytes it holds. The bytes are written once
/// per copy, by the background reading a book's first open owes, and are
/// what the scan matches a copy that moved by. Nothing writes the chain.
const CLAIM_VERSION: u8 = 2;
const CLAIM_ACTIVE: u8 = 1;
const CLAIM_RELEASED: u8 = 2;
const CLAIM_HEADER: usize = 4 + 1 + 1 + 1 + 2;
/// first cluster + digest-present flag + byte length + sha256.
const CLAIM_EVIDENCE: usize = 4 + 1 + 8 + crate::source::SHA256_BYTES;
/// magic + version + state + root byte + locator length + locator +
/// evidence + checksum.
pub const CACHE_CLAIM_MAX_BYTES: usize =
    CLAIM_HEADER + crate::library_path::MAX_PATH_BYTES + CLAIM_EVIDENCE + 4;

/// What a directory's stored claim says about one owner. The claim has a
/// lifecycle, not just a name: ownership evidence must live exactly as long
/// as the positions it vouches for, so a sweep that retires a departed
/// book's cache releases the claim rather than deleting it or leaving it
/// armed. A released claim still names its book: the same owner returning
/// resumes the directory and the positions the evidence proves are its own,
/// while a different book adopting it knows the surviving positions are not
/// its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheClaimReading {
    /// A well-formed active claim naming this owner.
    MineActive,
    /// A claim naming this owner that a sweep has released.
    MineReleased,
    /// A well-formed active claim naming another book.
    OtherActive,
    /// Another book's claim, released after its owner left the card.
    OtherReleased,
    /// Not a claim: torn, truncated, or alien bytes. Recoverable by a
    /// writer, unusable as evidence by a reader. Distinct from a failure to
    /// read the file, which is not evidence of anything; the storage layer
    /// keeps those apart.
    Invalid,
}

/// What a claim records about the physical file its owner names, beyond
/// where that file was.
///
/// Two halves that answer different questions, and neither answers the one a
/// move turns on. The digest says *what bytes it held*, which identifies a
/// book and not a copy of it: a card can hold the same bytes twice. The
/// chain says *which cluster it started at*, which a rename leaves alone but
/// a deletion frees, so a file written afterwards can be handed the same
/// number and equality with it proves nothing about which copy this is.
///
/// So both narrow and neither concludes on its own. What acts on the digest
/// is the scan's move search, and only against a file it has just read: the
/// recorded bytes say which book to look for, and the reading says which
/// file holds it. Either half may be absent: a claim written before this
/// field existed has neither, and a book nobody has opened long enough has
/// no digest yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheEvidence {
    /// The FAT first cluster of the file when the claim was written.
    pub cluster: Option<u32>,
    pub digest: Option<crate::source::CachedSourceDigest>,
}

/// Encode the claim for one book. `None` when the locator does not fit,
/// which a legal [`crate::library_path::LibraryPath`] cannot reach.
pub fn encode_cache_claim(
    root: crate::library_path::BookRoot,
    locator: &str,
    released: bool,
    evidence: &CacheEvidence,
    out: &mut [u8; CACHE_CLAIM_MAX_BYTES],
) -> Option<usize> {
    if locator.len() > crate::library_path::MAX_PATH_BYTES {
        return None;
    }
    out[..4].copy_from_slice(&CLAIM_MAGIC);
    out[4] = CLAIM_VERSION;
    out[5] = if released {
        CLAIM_RELEASED
    } else {
        CLAIM_ACTIVE
    };
    out[6] = root_hash_byte(root);
    let len = locator.len();
    out[7..9].copy_from_slice(&(len as u16).to_le_bytes());
    out[9..9 + len].copy_from_slice(locator.as_bytes());
    let at = CLAIM_HEADER + len;
    // Cluster zero is the FAT's own "no chain", so it doubles as "not
    // recorded" without a second flag to keep in step with it.
    out[at..at + 4].copy_from_slice(&evidence.cluster.unwrap_or(0).to_le_bytes());
    match &evidence.digest {
        Some(digest) => {
            out[at + 4] = 1;
            out[at + 5..at + 13].copy_from_slice(&digest.byte_len().to_le_bytes());
            out[at + 13..at + 13 + crate::source::SHA256_BYTES].copy_from_slice(digest.sha256());
        }
        None => out[at + 4..at + CLAIM_EVIDENCE].fill(0),
    }
    let body = at + CLAIM_EVIDENCE;
    let mut hash = 0x811c_9dc5u32;
    for byte in &out[..body] {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    out[body..body + 4].copy_from_slice(&hash.to_le_bytes());
    Some(body + 4)
}

/// One decoded claim: who owns the directory, and what is known about the
/// physical file that owner named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheClaimant<'a> {
    pub root: crate::library_path::BookRoot,
    pub locator: &'a str,
    pub released: bool,
    pub evidence: CacheEvidence,
}

/// The book a well-formed stored claim names, its locator borrowed from the
/// stored bytes. `None` for anything that is not a claim.
///
/// A version 1 claim decodes with empty evidence rather than failing: it is a
/// well-formed statement of ownership, which is all it ever claimed to be,
/// and refusing it would cost a reader the position it protects.
pub fn decode_cache_claimant(stored: &[u8]) -> Option<CacheClaimant<'_>> {
    if stored.len() < CLAIM_HEADER + 4 || stored[..4] != CLAIM_MAGIC {
        return None;
    }
    let evidence_bytes = match stored[4] {
        CLAIM_VERSION_NAMED => 0,
        CLAIM_VERSION => CLAIM_EVIDENCE,
        _ => return None,
    };
    let released = match stored[5] {
        CLAIM_ACTIVE => false,
        CLAIM_RELEASED => true,
        _ => return None,
    };
    let root = match stored[6] {
        0 => crate::library_path::BookRoot::Library,
        1 => crate::library_path::BookRoot::CardRoot,
        _ => return None,
    };
    let len = u16::from_le_bytes([stored[7], stored[8]]) as usize;
    if len > crate::library_path::MAX_PATH_BYTES
        || stored.len() != CLAIM_HEADER + len + evidence_bytes + 4
    {
        return None;
    }
    let body = CLAIM_HEADER + len + evidence_bytes;
    let mut hash = 0x811c_9dc5u32;
    for byte in &stored[..body] {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    if stored[body..body + 4] != hash.to_le_bytes() {
        return None;
    }
    let locator = core::str::from_utf8(&stored[CLAIM_HEADER..CLAIM_HEADER + len]).ok()?;
    let evidence = if evidence_bytes == 0 {
        CacheEvidence::default()
    } else {
        let at = CLAIM_HEADER + len;
        let cluster = u32::from_le_bytes(stored[at..at + 4].try_into().ok()?);
        let digest = if stored[at + 4] == 1 {
            let byte_len = u64::from_le_bytes(stored[at + 5..at + 13].try_into().ok()?);
            let sha256: [u8; crate::source::SHA256_BYTES] = stored
                [at + 13..at + 13 + crate::source::SHA256_BYTES]
                .try_into()
                .ok()?;
            Some(crate::source::CachedSourceDigest::new(
                crate::source::SourceDigest::from_parts(byte_len, sha256),
            ))
        } else {
            None
        };
        CacheEvidence {
            cluster: (cluster != 0).then_some(cluster),
            digest,
        }
    };
    Some(CacheClaimant {
        root,
        locator,
        released,
        evidence,
    })
}

/// Classify stored claim bytes against one owner.
pub fn read_cache_claim(
    stored: &[u8],
    root: crate::library_path::BookRoot,
    locator: &str,
) -> CacheClaimReading {
    match decode_cache_claimant(stored) {
        Some(claim) => {
            let mine = claim.root == root && claim.locator == locator;
            match (mine, claim.released) {
                (true, false) => CacheClaimReading::MineActive,
                (true, true) => CacheClaimReading::MineReleased,
                (false, false) => CacheClaimReading::OtherActive,
                (false, true) => CacheClaimReading::OtherReleased,
            }
        }
        None => CacheClaimReading::Invalid,
    }
}

/// The cache key firmware before catalog v8 derived for this book, when one
/// exists: [`legacy_source_hash`], truncated the way the display-path key
/// was. Read-only compatibility for per-book reading positions, which are
/// the one thing under a key that cannot be rebuilt; everything else under
/// an old key is derived data and stays orphaned. Migration by identity
/// wants the full hash, not this 28-bit directory name.
pub fn legacy_position_cache_key(
    display_name: &str,
    byte_size: u32,
) -> Option<String<CACHE_KEY_BYTES>> {
    let hash = legacy_source_hash(display_name, byte_size)?;
    let mut out = String::<CACHE_KEY_BYTES>::new();
    let _ = out.push('E');
    push_hex(&mut out, hash & 0x0FFF_FFFF, 7);
    Some(out)
}

pub fn section_file_name<const N: usize>(spine: u16, out: &mut String<N>) {
    out.clear();
    let _ = out.push('S');
    push_dec3(out, spine);
    let _ = out.push_str(".BIN");
}

pub fn encode_book_header(header: BookCacheHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, BOOK_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_VERSION);
    write_u16(out, 6, header.spine_count);
    write_u16(out, 8, header.toc_count);
    write_u16(out, 10, 0);
    write_u32(out, 12, header.string_bytes);
    Ok(BOOK_HEADER_BYTES)
}

pub fn decode_book_header(input: &[u8]) -> Result<BookCacheHeader, CacheError> {
    require(input, BOOK_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != CACHE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(BookCacheHeader {
        spine_count: read_u16(input, 6)?,
        toc_count: read_u16(input, 8)?,
        string_bytes: read_u32(input, 12)?,
    })
}

pub fn encode_spine(record: SpineRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, SPINE_RECORD_BYTES)?;
    write_u32(out, 0, record.href_offset);
    write_u16(out, 4, record.href_len);
    write_i16(out, 6, record.toc_index);
    write_u32(out, 8, record.byte_size);
    Ok(SPINE_RECORD_BYTES)
}

pub fn decode_spine(input: &[u8]) -> Result<SpineRecord, CacheError> {
    require(input, SPINE_RECORD_BYTES)?;
    Ok(SpineRecord {
        href_offset: read_u32(input, 0)?,
        href_len: read_u16(input, 4)?,
        toc_index: read_i16(input, 6)?,
        byte_size: read_u32(input, 8)?,
    })
}

pub fn encode_toc(record: TocRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, TOC_RECORD_BYTES)?;
    write_u32(out, 0, record.title_offset);
    write_u16(out, 4, record.title_len);
    write_u16(out, 6, 0);
    write_u32(out, 8, record.href_offset);
    write_u16(out, 12, record.href_len);
    write_u16(out, 14, 0);
    write_u32(out, 16, record.anchor_offset);
    write_u16(out, 20, record.anchor_len);
    out[22] = record.level;
    out[23] = 0;
    write_i16(out, 14, record.spine_index);
    Ok(TOC_RECORD_BYTES)
}

pub fn decode_toc(input: &[u8]) -> Result<TocRecord, CacheError> {
    require(input, TOC_RECORD_BYTES)?;
    Ok(TocRecord {
        title_offset: read_u32(input, 0)?,
        title_len: read_u16(input, 4)?,
        href_offset: read_u32(input, 8)?,
        href_len: read_u16(input, 12)?,
        anchor_offset: read_u32(input, 16)?,
        anchor_len: read_u16(input, 20)?,
        level: input[22],
        spine_index: read_i16(input, 14)?,
    })
}

pub fn encode_page_header(header: PageCacheHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, PAGE_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_VERSION);
    write_u16(out, 6, header.page_count);
    write_u16(out, 8, header.block_count);
    out[10] = header.partial as u8;
    out[11] = 0;
    write_u32(out, 12, header.text_bytes);
    write_u16(out, 16, header.viewport_width);
    write_u16(out, 18, header.viewport_height);
    write_u16(out, 20, header.font_config);
    write_u16(out, 22, 0);
    write_u32(out, 24, header.bytes_consumed);
    Ok(PAGE_HEADER_BYTES)
}

pub fn encode_section_header(header: SectionHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, SECTION_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_VERSION);
    write_u16(out, 6, header.page_count);
    write_u16(out, 8, header.block_count);
    write_u16(out, 10, header.line_count);
    write_u16(out, 12, header.word_count);
    out[14] = header.partial as u8;
    out[15] = 0;
    write_u32(out, 16, header.text_bytes);
    write_u16(out, 20, header.viewport_width);
    write_u16(out, 22, header.viewport_height);
    write_u16(out, 24, header.font_config);
    write_u16(out, 26, 0);
    write_u32(out, 28, header.bytes_consumed);
    write_u32(out, 32, header.total_bytes);
    write_u32(out, 36, 0);
    Ok(SECTION_HEADER_BYTES)
}

pub fn decode_section_header(input: &[u8]) -> Result<SectionHeader, CacheError> {
    require(input, SECTION_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != CACHE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(SectionHeader {
        page_count: read_u16(input, 6)?,
        block_count: read_u16(input, 8)?,
        line_count: read_u16(input, 10)?,
        word_count: read_u16(input, 12)?,
        partial: input[14] != 0,
        text_bytes: read_u32(input, 16)?,
        viewport_width: read_u16(input, 20)?,
        viewport_height: read_u16(input, 22)?,
        font_config: read_u16(input, 24)?,
        bytes_consumed: read_u32(input, 28)?,
        total_bytes: read_u32(input, 32)?,
    })
}

pub fn encode_section_v2_header(
    header: SectionV2Header,
    out: &mut [u8],
) -> Result<usize, CacheError> {
    require(out, SECTION_V2_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_V2_VERSION);
    write_u16(out, 6, header.spine);
    write_u16(out, 8, header.page_count);
    write_u16(out, 10, header.block_count);
    out[12] = header.partial as u8;
    out[13] = 0;
    write_u16(out, 14, 0);
    write_u32(out, 16, header.text_bytes);
    write_u16(out, 20, header.viewport_width);
    write_u16(out, 22, header.viewport_height);
    write_u16(out, 24, header.font_config);
    write_u16(out, 26, 0);
    write_u32(out, 28, header.bytes_consumed);
    write_u32(out, 32, header.total_bytes);
    write_u32(out, 36, header.source_hash);
    write_u32(out, 40, header.source_size);
    write_u64(out, 44, header.custom_font_identity);
    write_u32(out, 52, 0);
    Ok(SECTION_V2_HEADER_BYTES)
}

pub fn decode_section_v2_header(input: &[u8]) -> Result<SectionV2Header, CacheError> {
    require(input, SECTION_V2_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if !valid_cache_v2_version(read_u16(input, 4)?) {
        return Err(CacheError::BadVersion);
    }
    Ok(SectionV2Header {
        spine: read_u16(input, 6)?,
        page_count: read_u16(input, 8)?,
        block_count: read_u16(input, 10)?,
        partial: input[12] != 0,
        text_bytes: read_u32(input, 16)?,
        viewport_width: read_u16(input, 20)?,
        viewport_height: read_u16(input, 22)?,
        font_config: read_u16(input, 24)?,
        custom_font_identity: read_u64(input, 44)?,
        bytes_consumed: read_u32(input, 28)?,
        total_bytes: read_u32(input, 32)?,
        source_hash: read_u32(input, 36)?,
        source_size: read_u32(input, 40)?,
    })
}

pub fn encode_book_v2_header(header: BookV2Header, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, BOOK_V2_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_V2_VERSION);
    out[6] = header.partial as u8;
    out[7] = 0;
    write_u32(out, 8, header.source_hash);
    write_u32(out, 12, header.source_size);
    write_u32(out, 16, header.total_pages);
    write_u16(out, 20, header.section_count);
    write_u16(out, 22, header.spine_count);
    write_u16(out, 24, header.toc_count);
    write_u16(out, 26, header.resume_spine);
    write_u16(out, 28, header.viewport_width);
    write_u16(out, 30, header.viewport_height);
    write_u16(out, 32, header.font_config);
    write_u16(out, 34, 0);
    write_u32(out, 36, header.toc_text_bytes);
    write_u32(out, 40, header.title_text_bytes);
    write_u32(out, 44, header.author_text_bytes);
    write_u64(out, 48, header.custom_font_identity);
    Ok(BOOK_V2_HEADER_BYTES)
}

pub fn decode_book_v2_header(input: &[u8]) -> Result<BookV2Header, CacheError> {
    require(input, BOOK_V2_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if !valid_cache_v2_version(read_u16(input, 4)?) {
        return Err(CacheError::BadVersion);
    }
    Ok(BookV2Header {
        partial: input[6] != 0,
        source_hash: read_u32(input, 8)?,
        source_size: read_u32(input, 12)?,
        total_pages: read_u32(input, 16)?,
        section_count: read_u16(input, 20)?,
        spine_count: read_u16(input, 22)?,
        toc_count: read_u16(input, 24)?,
        toc_text_bytes: read_u32(input, 36)?,
        title_text_bytes: read_u32(input, 40)?,
        author_text_bytes: read_u32(input, 44)?,
        viewport_width: read_u16(input, 28)?,
        viewport_height: read_u16(input, 30)?,
        font_config: read_u16(input, 32)?,
        custom_font_identity: read_u64(input, 48)?,
        resume_spine: read_u16(input, 26)?,
    })
}

pub fn encode_book_v2_section(
    record: BookV2SectionRecord,
    out: &mut [u8],
) -> Result<usize, CacheError> {
    require(out, BOOK_V2_SECTION_RECORD_BYTES)?;
    write_u16(out, 0, record.section);
    write_u16(out, 2, record.spine);
    write_u32(out, 4, record.start_page);
    write_u16(out, 8, record.page_count);
    out[10] = record.partial as u8;
    out[11] = 0;
    write_u32(out, 12, 0);
    Ok(BOOK_V2_SECTION_RECORD_BYTES)
}

pub fn decode_book_v2_section(input: &[u8]) -> Result<BookV2SectionRecord, CacheError> {
    require(input, BOOK_V2_SECTION_RECORD_BYTES)?;
    Ok(BookV2SectionRecord {
        section: read_u16(input, 0)?,
        spine: read_u16(input, 2)?,
        start_page: read_u32(input, 4)?,
        page_count: read_u16(input, 8)?,
        partial: input[10] != 0,
    })
}

fn valid_cache_v2_version(version: u16) -> bool {
    version == CACHE_V2_VERSION || version == CACHE_V2_COMPAT_VERSION
}

/// Record, into `chapter_start`, that TOC entry `chapter` opens at the section
/// carrying `spine`. `chapter_start` runs parallel to `sections` and holds
/// `chapter + 1` (0 = no chapter opens here) so an all-zero boot value means
/// "empty". The first chapter to claim a section keeps it: a run of TOC
/// entries sharing one spine resolves to the run's first entry.
///
/// Chapter start pages are always section start pages, so a map bounded by the
/// section count covers a table of contents of any length -- the reason this
/// is keyed by section rather than by chapter index.
pub fn mark_chapter_start(
    chapter_start: &mut [u16],
    sections: &[BookV2SectionRecord],
    chapter: u16,
    spine: i16,
) {
    if spine < 0 || chapter == u16::MAX {
        return;
    }
    let spine = spine as u16;
    // A spine split across several sections keeps the chapter on its first
    // section, mirroring `page_for_spine`.
    let Some(index) = sections.iter().position(|section| section.spine == spine) else {
        return;
    };
    if let Some(slot) = chapter_start.get_mut(index) {
        if *slot == 0 {
            *slot = chapter + 1;
        }
    }
}

/// The chapter `page` falls in: the chapter marked on the section with the
/// greatest start page not beyond `page` (ties go to the lowest chapter
/// index). Sections without a mark defer to the nearest marked section before
/// them, so pages deep in a split or TOC-less spine still name the chapter
/// that opened it. 0 when no marked section starts at or before `page`.
pub fn chapter_for_page(chapter_start: &[u16], sections: &[BookV2SectionRecord], page: u32) -> u16 {
    let mut best: Option<(u32, u16)> = None;
    for (slot, section) in chapter_start.iter().zip(sections) {
        if *slot == 0 || section.start_page > page {
            continue;
        }
        let chapter = *slot - 1;
        let better = match best {
            None => true,
            Some((best_page, best_chapter)) => {
                section.start_page > best_page
                    || (section.start_page == best_page && chapter < best_chapter)
            }
        };
        if better {
            best = Some((section.start_page, chapter));
        }
    }
    best.map_or(0, |(_, chapter)| chapter)
}

pub fn decode_page_header(input: &[u8]) -> Result<PageCacheHeader, CacheError> {
    require(input, PAGE_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != CACHE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(PageCacheHeader {
        page_count: read_u16(input, 6)?,
        block_count: read_u16(input, 8)?,
        partial: input[10] != 0,
        text_bytes: read_u32(input, 12)?,
        viewport_width: read_u16(input, 16)?,
        viewport_height: read_u16(input, 18)?,
        font_config: read_u16(input, 20)?,
        bytes_consumed: read_u32(input, 24)?,
    })
}

pub fn encode_page(record: PageRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, PAGE_RECORD_BYTES)?;
    write_u16(out, 0, record.first_block);
    write_u16(out, 2, record.block_count);
    Ok(PAGE_RECORD_BYTES)
}

pub fn decode_page(input: &[u8]) -> Result<PageRecord, CacheError> {
    require(input, PAGE_RECORD_BYTES)?;
    Ok(PageRecord {
        first_block: read_u16(input, 0)?,
        block_count: read_u16(input, 2)?,
    })
}

pub fn encode_line(record: LineRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, LINE_RECORD_BYTES)?;
    write_u16(out, 0, record.first_word);
    write_u16(out, 2, record.word_count);
    write_i16(out, 4, record.x);
    write_i16(out, 6, record.y);
    write_u16(out, 8, record.width);
    out[10] = align_byte(record.align);
    out[11] = 0;
    Ok(LINE_RECORD_BYTES)
}

pub fn decode_line(input: &[u8]) -> Result<LineRecord, CacheError> {
    require(input, LINE_RECORD_BYTES)?;
    Ok(LineRecord {
        first_word: read_u16(input, 0)?,
        word_count: read_u16(input, 2)?,
        x: read_i16(input, 4)?,
        y: read_i16(input, 6)?,
        width: read_u16(input, 8)?,
        align: align_from_byte(input[10])?,
    })
}

pub fn encode_word(record: WordRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, WORD_RECORD_BYTES)?;
    write_u32(out, 0, record.text_offset);
    write_u16(out, 4, record.text_len);
    write_i16(out, 6, record.x);
    write_u16(out, 8, record.width);
    out[10] = style_byte(record.style);
    out[11] = 0;
    Ok(WORD_RECORD_BYTES)
}

pub fn decode_word(input: &[u8]) -> Result<WordRecord, CacheError> {
    require(input, WORD_RECORD_BYTES)?;
    Ok(WordRecord {
        text_offset: read_u32(input, 0)?,
        text_len: read_u16(input, 4)?,
        x: read_i16(input, 6)?,
        width: read_u16(input, 8)?,
        style: style_from_byte(input[10])?,
    })
}

pub fn encode_block(record: BlockRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, BLOCK_RECORD_BYTES)?;
    write_u32(out, 0, record.text_offset);
    write_u16(out, 4, record.text_len);
    out[6] = record.line_count;
    out[7] = role_byte(record.role);
    out[8] = style_byte(record.style);
    out[9] = align_byte(record.align);
    write_u16(out, 10, 0);
    Ok(BLOCK_RECORD_BYTES)
}

pub fn decode_block(input: &[u8]) -> Result<BlockRecord, CacheError> {
    require(input, BLOCK_RECORD_BYTES)?;
    Ok(BlockRecord {
        text_offset: read_u32(input, 0)?,
        text_len: read_u16(input, 4)?,
        line_count: input[6],
        role: role_from_byte(input[7])?,
        style: style_from_byte(input[8])?,
        align: align_from_byte(input[9])?,
    })
}

pub fn encode_cover_header(header: CoverCacheHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, COVER_HEADER_BYTES)?;
    out[..4].copy_from_slice(COVER_MAGIC);
    out[4] = COVER_VERSION;
    write_u16(out, 5, header.width);
    write_u16(out, 7, header.height);
    write_u16(out, 9, header.stride);
    out[11] = 0;
    Ok(COVER_HEADER_BYTES)
}

pub fn decode_cover_header(input: &[u8]) -> Result<CoverCacheHeader, CacheError> {
    require(input, COVER_HEADER_BYTES)?;
    if &input[..4] != COVER_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if input[4] != COVER_VERSION {
        return Err(CacheError::BadVersion);
    }
    if input[11] != 0 {
        return Err(CacheError::BadLength);
    }
    let header = CoverCacheHeader {
        width: read_u16(input, 5)?,
        height: read_u16(input, 7)?,
        stride: read_u16(input, 9)?,
    };
    if header != CoverCacheHeader::x4_dock_clean() {
        return Err(CacheError::BadLength);
    }
    Ok(header)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TocFileHeader {
    pub source_hash: u32,
    pub source_size: u32,
    pub chapter_count: u16,
}

#[derive(Clone, Copy)]
pub struct TocChapterRecord {
    pub spine_index: i16,
    pub level: u8,
    pub title_len: u8,
    pub title: [u8; TOC_CHAPTER_TITLE_BYTES],
}

impl TocChapterRecord {
    pub fn title_str(&self) -> &str {
        let len = (self.title_len as usize).min(TOC_CHAPTER_TITLE_BYTES);
        core::str::from_utf8(&self.title[..len]).unwrap_or("")
    }
}

/// Build a record from a title, truncating to the title budget on a UTF-8
/// char boundary so `title_str` always decodes.
pub fn toc_chapter_record(title: &str, level: u8, spine_index: i16) -> TocChapterRecord {
    let mut len = title.len().min(TOC_CHAPTER_TITLE_BYTES);
    while len > 0 && !title.is_char_boundary(len) {
        len -= 1;
    }
    let mut buf = [0u8; TOC_CHAPTER_TITLE_BYTES];
    buf[..len].copy_from_slice(&title.as_bytes()[..len]);
    TocChapterRecord {
        spine_index,
        level,
        title_len: len as u8,
        title: buf,
    }
}

pub fn encode_toc_file_header(header: TocFileHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, TOC_FILE_HEADER_BYTES)?;
    write_u32(out, 0, TOC_FILE_MAGIC);
    write_u16(out, 4, TOC_FILE_VERSION);
    write_u16(out, 6, header.chapter_count);
    write_u32(out, 8, header.source_hash);
    write_u32(out, 12, header.source_size);
    Ok(TOC_FILE_HEADER_BYTES)
}

pub fn decode_toc_file_header(input: &[u8]) -> Result<TocFileHeader, CacheError> {
    require(input, TOC_FILE_HEADER_BYTES)?;
    if read_u32(input, 0)? != TOC_FILE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != TOC_FILE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(TocFileHeader {
        chapter_count: read_u16(input, 6)?,
        source_hash: read_u32(input, 8)?,
        source_size: read_u32(input, 12)?,
    })
}

pub fn encode_toc_chapter(record: &TocChapterRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, TOC_CHAPTER_RECORD_BYTES)?;
    write_i16(out, 0, record.spine_index);
    out[2] = record.level;
    out[3] = record.title_len;
    out[4..4 + TOC_CHAPTER_TITLE_BYTES].copy_from_slice(&record.title);
    Ok(TOC_CHAPTER_RECORD_BYTES)
}

pub fn decode_toc_chapter(input: &[u8]) -> Result<TocChapterRecord, CacheError> {
    require(input, TOC_CHAPTER_RECORD_BYTES)?;
    let mut title = [0u8; TOC_CHAPTER_TITLE_BYTES];
    title.copy_from_slice(&input[4..4 + TOC_CHAPTER_TITLE_BYTES]);
    Ok(TocChapterRecord {
        spine_index: read_i16(input, 0)?,
        level: input[2],
        title_len: input[3],
        title,
    })
}

fn require(slice: &[u8], len: usize) -> Result<(), CacheError> {
    if slice.len() < len {
        Err(CacheError::BufferTooSmall)
    } else {
        Ok(())
    }
}

fn read_u16(input: &[u8], offset: usize) -> Result<u16, CacheError> {
    require(&input[offset.min(input.len())..], 2)?;
    Ok(u16::from_le_bytes([input[offset], input[offset + 1]]))
}

fn read_i16(input: &[u8], offset: usize) -> Result<i16, CacheError> {
    Ok(read_u16(input, offset)? as i16)
}

fn read_u32(input: &[u8], offset: usize) -> Result<u32, CacheError> {
    require(&input[offset.min(input.len())..], 4)?;
    Ok(u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn read_u64(input: &[u8], offset: usize) -> Result<u64, CacheError> {
    require(&input[offset.min(input.len())..], 8)?;
    Ok(u64::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
        input[offset + 4],
        input[offset + 5],
        input[offset + 6],
        input[offset + 7],
    ]))
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_i16(out: &mut [u8], offset: usize, value: i16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn push_hex<const N: usize>(out: &mut String<N>, value: u32, digits: u8) {
    for shift in (0..digits).rev() {
        let nibble = ((value >> (shift * 4)) & 0x0F) as u8;
        let ch = if nibble < 10 {
            b'0' + nibble
        } else {
            b'A' + nibble - 10
        };
        let _ = out.push(ch as char);
    }
}

fn push_dec3<const N: usize>(out: &mut String<N>, value: u16) {
    let value = value.min(999);
    let _ = out.push((b'0' + ((value / 100) % 10) as u8) as char);
    let _ = out.push((b'0' + ((value / 10) % 10) as u8) as char);
    let _ = out.push((b'0' + (value % 10) as u8) as char);
}

fn role_byte(role: TextRole) -> u8 {
    match role {
        TextRole::Body => 0,
        TextRole::Heading1 => 1,
        TextRole::Heading2 => 2,
        TextRole::Heading3 => 3,
        TextRole::BlockQuote => 4,
    }
}

fn role_from_byte(byte: u8) -> Result<TextRole, CacheError> {
    match byte {
        0 => Ok(TextRole::Body),
        1 => Ok(TextRole::Heading1),
        2 => Ok(TextRole::Heading2),
        3 => Ok(TextRole::Heading3),
        4 => Ok(TextRole::BlockQuote),
        _ => Err(CacheError::BadLength),
    }
}

fn style_byte(style: FontStyle) -> u8 {
    match style {
        FontStyle::Regular => 0,
        FontStyle::Italic => 1,
        FontStyle::Bold => 2,
        FontStyle::BoldItalic => 3,
    }
}

fn style_from_byte(byte: u8) -> Result<FontStyle, CacheError> {
    match byte {
        0 => Ok(FontStyle::Regular),
        1 => Ok(FontStyle::Italic),
        2 => Ok(FontStyle::Bold),
        3 => Ok(FontStyle::BoldItalic),
        _ => Err(CacheError::BadLength),
    }
}

fn align_byte(align: TextAlign) -> u8 {
    match align {
        TextAlign::Left => 0,
        TextAlign::Center => 1,
        TextAlign::Justify => 2,
    }
}

fn align_from_byte(byte: u8) -> Result<TextAlign, CacheError> {
    match byte {
        0 => Ok(TextAlign::Left),
        1 => Ok(TextAlign::Center),
        2 => Ok(TextAlign::Justify),
        _ => Err(CacheError::BadLength),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn content_header_round_trips_and_rejects_foreign_bytes() {
        for complete in [false, true] {
            let header = ContentHeader {
                source_hash: 0xDEAD_BEEF,
                source_size: 3_200_141,
                complete,
                spine_count: 42,
                content_len: 12345,
            };
            let mut bytes = [0u8; CONTENT_HEADER_BYTES];
            encode_content_header(header, &mut bytes).expect("header encodes");
            assert_eq!(decode_content_header(&bytes), Ok(header));
        }

        let mut bytes = [0u8; CONTENT_HEADER_BYTES];
        encode_content_header(
            ContentHeader {
                source_hash: 1,
                source_size: 2,
                complete: true,
                spine_count: 0,
                content_len: 0,
            },
            &mut bytes,
        )
        .expect("header encodes");
        let mut bad_magic = bytes;
        bad_magic[0] ^= 0xFF;
        assert_eq!(decode_content_header(&bad_magic), Err(CacheError::BadMagic));
        let mut bad_version = bytes;
        bad_version[4] = 0xFF;
        assert_eq!(
            decode_content_header(&bad_version),
            Err(CacheError::BadVersion)
        );
        // A stream stamped with a different BOOK.BIN format version was
        // captured under other parse semantics and must not replay.
        let mut bad_book_version = bytes;
        bad_book_version[18] ^= 0xFF;
        assert_eq!(
            decode_content_header(&bad_book_version),
            Err(CacheError::BadVersion)
        );
        assert_eq!(
            decode_content_header(&bytes[..CONTENT_HEADER_BYTES - 1]),
            Err(CacheError::BufferTooSmall)
        );
    }

    fn section(section: u16, spine: u16, start_page: u32) -> BookV2SectionRecord {
        BookV2SectionRecord {
            section,
            spine,
            start_page,
            page_count: 10,
            partial: false,
        }
    }

    /// Regression: a 322-entry TOC must keep resolving past chapter 255. The
    /// map is bounded by the section count (chapter starts are section
    /// starts), so no per-chapter array caps it.
    #[test]
    fn chapter_tracking_resolves_past_256_chapters() {
        // 320 sections (the firmware's MAX_BOOK_SECTIONS), 10 pages each.
        // Chapters 0..=2 share spine 0 (front-matter entries on the title
        // page); chapters 3..=321 sit one per spine on spines 1..=319.
        let sections: std::vec::Vec<BookV2SectionRecord> = (0..320u16)
            .map(|i| section(i, i, u32::from(i) * 10))
            .collect();
        let mut chapter_start = [0u16; 320];
        for chapter in 0..322u16 {
            let spine: i16 = if chapter <= 2 { 0 } else { chapter as i16 - 2 };
            mark_chapter_start(&mut chapter_start, &sections, chapter, spine);
        }

        // Jumping to the last chapter and rendering its pages stays at 321.
        assert_eq!(chapter_for_page(&chapter_start, &sections, 3190), 321);
        assert_eq!(chapter_for_page(&chapter_start, &sections, 3195), 321);
        // A chapter past the old 256-entry cap but before the end.
        assert_eq!(chapter_for_page(&chapter_start, &sections, 2990), 301);
        // The shared-spine plateau names the run's first entry.
        assert_eq!(chapter_for_page(&chapter_start, &sections, 5), 0);
        assert_eq!(chapter_for_page(&chapter_start, &sections, 10), 3);
    }

    #[test]
    fn chapter_tracking_covers_split_and_unlisted_spines() {
        // Spine 1 spans sections 1-2; spine 2 (section 3) has no TOC entry.
        let sections = [
            section(0, 0, 0),
            section(1, 1, 10),
            section(2, 1, 20),
            section(3, 2, 30),
        ];
        let mut chapter_start = [0u16; 4];
        mark_chapter_start(&mut chapter_start, &sections, 0, 0);
        mark_chapter_start(&mut chapter_start, &sections, 1, 1);
        // Unresolvable spines never claim a section.
        mark_chapter_start(&mut chapter_start, &sections, 2, -1);
        assert_eq!(chapter_start, [1, 2, 0, 0]);

        // Pages in the split spine's second section keep its chapter, and the
        // TOC-less spine defers to the chapter that opened before it.
        assert_eq!(chapter_for_page(&chapter_start, &sections, 25), 1);
        assert_eq!(chapter_for_page(&chapter_start, &sections, 35), 1);
        // No marked section at or before the page resolves to chapter 0.
        let unmarked = [0u16; 4];
        assert_eq!(chapter_for_page(&unmarked, &sections, 25), 0);
    }

    #[test]
    fn content_record_header_round_trips() {
        let block = ContentRecordHeader {
            spine_index: 12,
            text_len: 517,
            role: TextRole::BlockQuote,
            style: FontStyle::Italic,
            align: TextAlign::Center,
            paragraph_end: true,
            spine_end: false,
        };
        let marker = ContentRecordHeader {
            spine_index: 12,
            text_len: 0,
            role: TextRole::Body,
            style: FontStyle::Regular,
            align: TextAlign::Left,
            paragraph_end: false,
            spine_end: true,
        };
        for record in [block, marker] {
            let mut bytes = [0u8; CONTENT_RECORD_HEADER_BYTES];
            encode_content_record_header(record, &mut bytes).expect("record encodes");
            assert_eq!(decode_content_record_header(&bytes), Ok(record));
        }
    }

    #[test]
    fn content_record_header_rejects_bad_bytes() {
        let mut bytes = [0u8; CONTENT_RECORD_HEADER_BYTES];
        encode_content_record_header(
            ContentRecordHeader {
                spine_index: 3,
                text_len: 9,
                role: TextRole::Body,
                style: FontStyle::Regular,
                align: TextAlign::Justify,
                paragraph_end: false,
                spine_end: false,
            },
            &mut bytes,
        )
        .expect("record encodes");
        let mut bad_role = bytes;
        bad_role[4] = 9;
        assert!(decode_content_record_header(&bad_role).is_err());
        // A spine-end marker carrying text is structurally invalid: replay
        // would mis-frame the stream.
        let mut text_on_marker = bytes;
        text_on_marker[7] = 2;
        assert!(decode_content_record_header(&text_on_marker).is_err());
        assert!(encode_content_record_header(
            ContentRecordHeader {
                spine_index: 0,
                text_len: 1,
                role: TextRole::Body,
                style: FontStyle::Regular,
                align: TextAlign::Left,
                paragraph_end: false,
                spine_end: true,
            },
            &mut bytes,
        )
        .is_err());
    }

    #[test]
    fn page_cache_records_round_trip() {
        let header = PageCacheHeader {
            page_count: 2,
            block_count: 3,
            text_bytes: 17,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 1,
            bytes_consumed: 4096,
            partial: true,
        };
        let page = PageRecord {
            first_block: 1,
            block_count: 2,
        };
        let block = BlockRecord {
            text_offset: 42,
            text_len: 11,
            line_count: 2,
            role: TextRole::Heading2,
            style: FontStyle::BoldItalic,
            align: TextAlign::Center,
        };

        let mut bytes = [0u8; 48];
        encode_page_header(header, &mut bytes[..PAGE_HEADER_BYTES]).expect("header encodes");
        encode_page(
            page,
            &mut bytes[PAGE_HEADER_BYTES..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES],
        )
        .expect("page encodes");
        encode_block(
            block,
            &mut bytes[PAGE_HEADER_BYTES + PAGE_RECORD_BYTES
                ..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES + BLOCK_RECORD_BYTES],
        )
        .expect("block encodes");

        assert_eq!(
            decode_page_header(&bytes[..PAGE_HEADER_BYTES]).unwrap(),
            header
        );
        assert_eq!(
            decode_page(&bytes[PAGE_HEADER_BYTES..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES]).unwrap(),
            page
        );
        assert_eq!(
            decode_block(
                &bytes[PAGE_HEADER_BYTES + PAGE_RECORD_BYTES
                    ..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES + BLOCK_RECORD_BYTES],
            )
            .unwrap(),
            block
        );
    }

    #[test]
    fn book_cache_records_round_trip() {
        let header = BookCacheHeader {
            spine_count: 1,
            toc_count: 1,
            string_bytes: 27,
        };
        let spine = SpineRecord {
            href_offset: 7,
            href_len: 12,
            toc_index: -1,
            byte_size: 1234,
        };
        let toc = TocRecord {
            title_offset: 20,
            title_len: 5,
            href_offset: 7,
            href_len: 12,
            anchor_offset: 0,
            anchor_len: 0,
            level: 2,
            spine_index: -1,
        };
        let mut bytes = [0u8; BOOK_HEADER_BYTES + SPINE_RECORD_BYTES + TOC_RECORD_BYTES];
        encode_book_header(header, &mut bytes[..BOOK_HEADER_BYTES]).expect("book header encodes");
        encode_spine(
            spine,
            &mut bytes[BOOK_HEADER_BYTES..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES],
        )
        .expect("spine encodes");
        encode_toc(
            toc,
            &mut bytes[BOOK_HEADER_BYTES + SPINE_RECORD_BYTES
                ..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES + TOC_RECORD_BYTES],
        )
        .expect("toc encodes");

        assert_eq!(
            decode_book_header(&bytes[..BOOK_HEADER_BYTES]).unwrap(),
            header
        );
        assert_eq!(
            decode_spine(&bytes[BOOK_HEADER_BYTES..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES])
                .unwrap(),
            spine
        );
        assert_eq!(
            decode_toc(
                &bytes[BOOK_HEADER_BYTES + SPINE_RECORD_BYTES
                    ..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES + TOC_RECORD_BYTES],
            )
            .unwrap(),
            toc
        );
    }

    #[test]
    fn section_cache_records_round_trip() {
        let header = SectionHeader {
            page_count: 1,
            block_count: 1,
            line_count: 1,
            word_count: 2,
            text_bytes: 13,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 2,
            bytes_consumed: 8192,
            total_bytes: 12_000,
            partial: true,
        };
        let line = LineRecord {
            first_word: 0,
            word_count: 2,
            x: 8,
            y: 24,
            width: 760,
            align: TextAlign::Justify,
        };
        let word = WordRecord {
            text_offset: 6,
            text_len: 7,
            x: 120,
            width: 54,
            style: FontStyle::Italic,
        };
        let mut bytes = [0u8; SECTION_HEADER_BYTES + LINE_RECORD_BYTES + WORD_RECORD_BYTES];

        encode_section_header(header, &mut bytes[..SECTION_HEADER_BYTES])
            .expect("section header encodes");
        encode_line(
            line,
            &mut bytes[SECTION_HEADER_BYTES..SECTION_HEADER_BYTES + LINE_RECORD_BYTES],
        )
        .expect("line encodes");
        encode_word(
            word,
            &mut bytes[SECTION_HEADER_BYTES + LINE_RECORD_BYTES
                ..SECTION_HEADER_BYTES + LINE_RECORD_BYTES + WORD_RECORD_BYTES],
        )
        .expect("word encodes");

        assert_eq!(
            decode_section_header(&bytes[..SECTION_HEADER_BYTES]).unwrap(),
            header
        );
        assert_eq!(
            decode_line(&bytes[SECTION_HEADER_BYTES..SECTION_HEADER_BYTES + LINE_RECORD_BYTES])
                .unwrap(),
            line
        );
        assert_eq!(
            decode_word(
                &bytes[SECTION_HEADER_BYTES + LINE_RECORD_BYTES
                    ..SECTION_HEADER_BYTES + LINE_RECORD_BYTES + WORD_RECORD_BYTES],
            )
            .unwrap(),
            word
        );
        assert_eq!(
            section_cache_size(header),
            SECTION_HEADER_BYTES
                + PAGE_RECORD_BYTES
                + BLOCK_RECORD_BYTES
                + 1
                + LINE_RECORD_BYTES
                + WORD_RECORD_BYTES * 2
                + 13
        );
    }

    #[test]
    fn section_v2_cache_records_round_trip() {
        let header = SectionV2Header {
            source_hash: 0x1234_5678,
            source_size: 98_765,
            spine: 7,
            page_count: 2,
            block_count: 3,
            text_bytes: 19,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 2,
            custom_font_identity: 0x1122_3344_5566_7788,
            bytes_consumed: 8192,
            total_bytes: 12_000,
            partial: true,
        };
        let mut bytes = [0u8; SECTION_V2_HEADER_BYTES];
        encode_section_v2_header(header, &mut bytes).expect("section v2 header encodes");

        assert_eq!(decode_section_v2_header(&bytes).unwrap(), header);
        assert_eq!(
            section_v2_cache_size(header),
            SECTION_V2_HEADER_BYTES + PAGE_RECORD_BYTES * 2 + BLOCK_RECORD_BYTES * 3 + 3 + 19
        );

        bytes[4] = CACHE_VERSION as u8;
        bytes[5] = 0;
        assert_eq!(
            decode_section_v2_header(&bytes),
            Err(CacheError::BadVersion)
        );
    }

    #[test]
    fn book_v2_cache_records_round_trip() {
        let header = BookV2Header {
            source_hash: 0x1234_5678,
            source_size: 98_765,
            total_pages: 123,
            section_count: 2,
            spine_count: 9,
            toc_count: 4,
            toc_text_bytes: 128,
            title_text_bytes: 20,
            author_text_bytes: 18,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 1,
            custom_font_identity: 0x8877_6655_4433_2211,
            partial: true,
            resume_spine: 31,
        };
        let section = BookV2SectionRecord {
            section: 1,
            spine: 7,
            start_page: 42,
            page_count: 12,
            partial: false,
        };
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        let mut section_bytes = [0u8; BOOK_V2_SECTION_RECORD_BYTES];

        encode_book_v2_header(header, &mut header_bytes).expect("book v2 header encodes");
        encode_book_v2_section(section, &mut section_bytes).expect("book v2 section encodes");

        assert_eq!(decode_book_v2_header(&header_bytes).unwrap(), header);
        assert_eq!(decode_book_v2_section(&section_bytes).unwrap(), section);
        assert_eq!(
            book_v2_cache_size(header),
            BOOK_V2_HEADER_BYTES
                + BOOK_V2_SECTION_RECORD_BYTES * 2
                + TOC_RECORD_BYTES * 4
                + 128
                + 20
                + 18
        );

        header_bytes[4] = CACHE_VERSION as u8;
        header_bytes[5] = 0;
        assert_eq!(
            decode_book_v2_header(&header_bytes),
            Err(CacheError::BadVersion)
        );
    }

    // `resume_spine` went into a field every previous writer left as an
    // explicit zero, which is what lets it ship without a version bump. If it
    // ever moves onto a byte an older build wrote something else into, an
    // existing cache would decode as "a build is still coming for this" and
    // rebuild a book that was already complete.
    #[test]
    fn a_book_index_written_before_resume_spine_existed_reads_as_unbuilt_by_nobody() {
        let header = BookV2Header {
            source_hash: 1,
            source_size: 2,
            total_pages: 3,
            section_count: 1,
            spine_count: 1,
            toc_count: 0,
            toc_text_bytes: 0,
            title_text_bytes: 0,
            author_text_bytes: 0,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 1,
            custom_font_identity: 0,
            partial: false,
            resume_spine: 0,
        };
        let mut bytes = [0u8; BOOK_V2_HEADER_BYTES];
        encode_book_v2_header(header, &mut bytes).expect("header encodes");
        // The two bytes an older writer zero-filled.
        assert_eq!(&bytes[26..28], &[0, 0]);
        assert_eq!(decode_book_v2_header(&bytes).unwrap().resume_spine, 0);
    }

    // A suspended build's cursor is the item *after* one it walked, so zero is
    // free to mean "nobody is building this". Guarding the encoding both ways
    // keeps that reading honest.
    #[test]
    fn a_suspended_builds_cursor_survives_the_round_trip() {
        let mut bytes = [0u8; BOOK_V2_HEADER_BYTES];
        for cursor in [1u16, 42, u16::MAX] {
            let header = BookV2Header {
                source_hash: 1,
                source_size: 2,
                total_pages: 3,
                section_count: 1,
                spine_count: 1,
                toc_count: 0,
                toc_text_bytes: 0,
                title_text_bytes: 0,
                author_text_bytes: 0,
                viewport_width: 800,
                viewport_height: 480,
                font_config: 1,
                custom_font_identity: 0,
                partial: true,
                resume_spine: cursor,
            };
            encode_book_v2_header(header, &mut bytes).expect("header encodes");
            let decoded = decode_book_v2_header(&bytes).expect("header decodes");
            assert_eq!(decoded.resume_spine, cursor);
            assert!(decoded.partial);
        }
    }

    #[test]
    fn artifact_names_and_cache_key_are_stable() {
        assert_eq!(CACHE_ROOT_DIR, "READER");
        assert_eq!(CATALOG_FILE, "CATALOG.BIN");
        assert_eq!(CACHE_DIR, "CACHE");
        assert_eq!(CACHE_V2_DIR, "CACHE2");
        assert_eq!(CACHE_SECTIONS_DIR, "SECTIONS");
        assert_eq!(CACHE_BOOK_FILE, "BOOK.BIN");
        assert_eq!(CACHE_COVER_FILE, "COVER.BIN");
        assert_eq!(CACHE_STATE_FILE, "STATE.BIN");
        // The exact digits matter less than the shape and the stability of
        // the inputs: 'E' plus seven hex digits of the identity hash. The
        // hash inputs themselves are pinned by the tests below.
        let key = cache_key_from(source_hash_at(
            crate::library_path::BookRoot::Library,
            "Book.epub",
            12_345,
        ));
        assert_eq!(key.len(), CACHE_KEY_BYTES);
        assert!(key.as_str().starts_with('E'));
        assert!(key.as_str()[1..].bytes().all(|b| b.is_ascii_hexdigit()));

        let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
        section_file_name(7, &mut name);
        assert_eq!(name.as_str(), "S007.BIN");
        section_file_name(1234, &mut name);
        assert_eq!(name.as_str(), "S999.BIN");
    }

    /// The identity input is the full location, not the 64-byte display
    /// label. Two legal nested locators that agree for more than 64 bytes,
    /// holding same-sized books, must land in different caches: any label
    /// truncated at 64 bytes shows the same text for both, and a label
    /// collision must not become a cache collision.
    #[test]
    fn nested_locators_sharing_a_long_prefix_keep_distinct_identities() {
        use crate::library_path::{BookRoot, LibraryPath};

        let shared = "A".repeat(70);
        let one = LibraryPath::parse(&std::format!("{shared}/Folder-A/Dune.epub")).unwrap();
        let two = LibraryPath::parse(&std::format!("{shared}/Folder-B/Dune.epub")).unwrap();
        assert_eq!(
            one.as_str().as_bytes()[..64],
            two.as_str().as_bytes()[..64],
            "the scenario needs the two to agree past any 64-byte label",
        );

        let size = 12_345;
        let hash_one = source_hash_at(BookRoot::Library, one.as_str(), size);
        let hash_two = source_hash_at(BookRoot::Library, two.as_str(), size);
        assert_ne!(hash_one, hash_two);
        assert_ne!(
            cache_key_from(hash_one).as_str(),
            cache_key_from(hash_two).as_str(),
        );
    }

    /// The same locator under the other root is a different book: a loose
    /// card-root EPUB and a shelved one may share a name and a size.
    #[test]
    fn the_root_separates_identities_that_agree_on_locator_and_size() {
        use crate::library_path::BookRoot;

        assert_ne!(
            source_hash_at(BookRoot::Library, "Dune.epub", 9),
            source_hash_at(BookRoot::CardRoot, "Dune.epub", 9),
        );
    }

    /// The size participates in the identity: replacing a book's bytes under
    /// the same locator is a different source, which is what invalidates the
    /// old cache.
    #[test]
    fn the_size_separates_identities_that_agree_on_root_and_locator() {
        use crate::library_path::BookRoot;

        assert_ne!(
            source_hash_at(BookRoot::Library, "Dune.epub", 9),
            source_hash_at(BookRoot::Library, "Dune.epub", 10),
        );
    }

    /// The legacy key must reproduce what pre-v8 firmware derived, byte for
    /// byte, or the position fallback looks in a directory no old firmware
    /// ever wrote. "EEE2AC55" is the exact value the retired `cache_key_for`
    /// pinned for this input while it was the live derivation.
    #[test]
    fn the_legacy_position_key_matches_what_old_firmware_derived() {
        assert_eq!(
            legacy_position_cache_key("/books/Book.epub", 12_345)
                .expect("a direct shelf book has a legacy key")
                .as_str(),
            "EEE2AC55",
        );
    }

    /// The claim names exactly one book across its whole lifecycle: the
    /// same locator under the same root matches whether active or released,
    /// The evidence a move is recognised by survives a round trip, and a
    /// claim written before it existed still reads as the statement of
    /// ownership it always was. Refusing a version 1 claim would cost its
    /// owner the position it protects, over a field that claim never
    /// promised.
    #[test]
    fn a_claim_carries_its_move_evidence_and_still_reads_one_without_any() {
        use crate::library_path::BookRoot;
        use crate::source::{CachedSourceDigest, SourceDigest};

        let digest = CachedSourceDigest::new(SourceDigest::from_parts(50_033, [7u8; 32]));
        let evidence = CacheEvidence {
            cluster: Some(4_211),
            digest: Some(digest),
        };
        let mut bytes = [0u8; CACHE_CLAIM_MAX_BYTES];
        let len = encode_cache_claim(
            BookRoot::Library,
            "Fiction/Dune.epub",
            false,
            &evidence,
            &mut bytes,
        )
        .expect("encodes");
        let claim = decode_cache_claimant(&bytes[..len]).expect("a claim");
        assert_eq!(claim.locator, "Fiction/Dune.epub");
        assert_eq!(claim.evidence.cluster, Some(4_211));
        assert_eq!(claim.evidence.digest, Some(digest));

        // Evidence changes nothing about who the claim names.
        assert_eq!(
            read_cache_claim(&bytes[..len], BookRoot::Library, "Fiction/Dune.epub"),
            CacheClaimReading::MineActive,
        );

        // A cluster of zero is the FAT's own "no chain", so it reads back as
        // absent rather than as chain zero.
        let mut none = [0u8; CACHE_CLAIM_MAX_BYTES];
        let none_len = encode_cache_claim(
            BookRoot::Library,
            "Fiction/Dune.epub",
            false,
            &CacheEvidence::default(),
            &mut none,
        )
        .expect("encodes");
        let bare = decode_cache_claimant(&none[..none_len]).expect("a claim");
        assert_eq!(bare.evidence, CacheEvidence::default());

        // A version 1 claim: the same bytes without the evidence block, and
        // with the checksum taken over the shorter body.
        let locator = b"Fiction/Dune.epub";
        let mut v1 = [0u8; CACHE_CLAIM_MAX_BYTES];
        v1[..4].copy_from_slice(&CLAIM_MAGIC);
        v1[4] = CLAIM_VERSION_NAMED;
        v1[5] = CLAIM_ACTIVE;
        v1[6] = 0;
        v1[7..9].copy_from_slice(&(locator.len() as u16).to_le_bytes());
        v1[9..9 + locator.len()].copy_from_slice(locator);
        let body = CLAIM_HEADER + locator.len();
        let mut hash = 0x811c_9dc5u32;
        for byte in &v1[..body] {
            hash ^= *byte as u32;
            hash = hash.wrapping_mul(0x0100_0193);
        }
        v1[body..body + 4].copy_from_slice(&hash.to_le_bytes());
        let old = decode_cache_claimant(&v1[..body + 4]).expect("a version 1 claim still reads");
        assert_eq!(old.locator, "Fiction/Dune.epub");
        assert_eq!(old.evidence, CacheEvidence::default());
        assert_eq!(
            read_cache_claim(&v1[..body + 4], BookRoot::Library, "Fiction/Dune.epub"),
            CacheClaimReading::MineActive,
            "an older claim still protects its owner's position",
        );
    }

    /// A version 2 claim whose evidence block is torn is not a claim at all.
    /// Reading it as ownership with empty evidence would hand a directory to
    /// whoever the surviving bytes happened to name.
    #[test]
    fn a_torn_claim_is_not_a_claim() {
        use crate::library_path::BookRoot;

        let evidence = CacheEvidence {
            cluster: Some(9),
            digest: None,
        };
        let mut bytes = [0u8; CACHE_CLAIM_MAX_BYTES];
        let len = encode_cache_claim(BookRoot::Library, "Dune.epub", false, &evidence, &mut bytes)
            .expect("encodes");

        for cut in 1..CLAIM_EVIDENCE + 4 {
            assert_eq!(
                decode_cache_claimant(&bytes[..len - cut]),
                None,
                "a claim cut {cut} bytes short is not a claim",
            );
        }
        let mut flipped = bytes;
        flipped[len - CLAIM_EVIDENCE - 2] ^= 0xFF;
        assert_eq!(decode_cache_claimant(&flipped[..len]), None);
    }

    /// everything else is another book, and torn bytes are not a claim at
    /// all, so one bad sector cannot read as somebody and brick the
    /// directory for everyone.
    #[test]
    fn a_cache_claim_names_exactly_one_book_through_its_lifecycle() {
        use crate::library_path::BookRoot;

        let mut bytes = [0u8; CACHE_CLAIM_MAX_BYTES];
        let len = encode_cache_claim(
            BookRoot::Library,
            "Fiction/Dune.epub",
            false,
            &CacheEvidence::default(),
            &mut bytes,
        )
        .expect("a legal locator encodes");
        let stored = &bytes[..len];

        assert_eq!(
            read_cache_claim(stored, BookRoot::Library, "Fiction/Dune.epub"),
            CacheClaimReading::MineActive,
        );
        assert_eq!(
            read_cache_claim(stored, BookRoot::Library, "Fiction/Other.epub"),
            CacheClaimReading::OtherActive,
            "another locator is another book",
        );
        assert_eq!(
            read_cache_claim(stored, BookRoot::CardRoot, "Fiction/Dune.epub"),
            CacheClaimReading::OtherActive,
            "the same locator under the other root is another book",
        );
        let claim = decode_cache_claimant(stored).expect("a claim");
        assert_eq!(claim.root, BookRoot::Library);
        assert_eq!(claim.locator, "Fiction/Dune.epub");
        assert!(!claim.released);
        assert_eq!(claim.evidence, CacheEvidence::default());

        let mut released = [0u8; CACHE_CLAIM_MAX_BYTES];
        let released_len = encode_cache_claim(
            BookRoot::Library,
            "Fiction/Dune.epub",
            true,
            &CacheEvidence::default(),
            &mut released,
        )
        .expect("encodes");
        assert_eq!(
            read_cache_claim(
                &released[..released_len],
                BookRoot::Library,
                "Fiction/Dune.epub"
            ),
            CacheClaimReading::MineReleased,
            "a released claim still names its owner",
        );
        assert_eq!(
            read_cache_claim(
                &released[..released_len],
                BookRoot::Library,
                "Fiction/Twin.epub"
            ),
            CacheClaimReading::OtherReleased,
        );

        let mut torn = [0u8; CACHE_CLAIM_MAX_BYTES];
        torn[..len].copy_from_slice(stored);
        torn[len - 1] ^= 0xFF;
        assert_eq!(
            read_cache_claim(&torn[..len], BookRoot::Library, "Fiction/Dune.epub"),
            CacheClaimReading::Invalid,
            "a torn claim is nobody's, the owner included",
        );
        assert_eq!(
            decode_cache_claimant(&stored[..len - 1]),
            None,
            "truncation fails"
        );
        assert_eq!(decode_cache_claimant(&[]), None, "emptiness fails");
    }

    /// Only display shapes old firmware could produce get a legacy key. A
    /// nested display path that resembles a flat one must not adopt some
    /// other book's old position.
    #[test]
    fn only_legacy_representable_display_paths_get_a_legacy_key() {
        assert!(legacy_position_cache_key("/books/Dune.epub", 9).is_some());
        assert!(legacy_position_cache_key("/Dune.epub", 9).is_some());
        assert!(legacy_position_cache_key("/books/fiction/dune.epub", 9).is_none());
        assert!(legacy_position_cache_key("/a/b.epub", 9).is_none());
        assert!(legacy_position_cache_key("Dune.epub", 9).is_none());
        assert!(legacy_position_cache_key("/books/", 9).is_none());
        assert!(legacy_position_cache_key("/", 9).is_none());
    }

    #[test]
    fn cover_cache_header_round_trips_and_validates_shape() {
        let header = CoverCacheHeader::x4_dock_clean();
        let mut bytes = [0u8; COVER_HEADER_BYTES];
        encode_cover_header(header, &mut bytes).expect("cover header encodes");

        assert_eq!(decode_cover_header(&bytes).unwrap(), header);
        assert_eq!(COVER_BYTES, 7878);

        bytes[0] = b'?';
        assert_eq!(decode_cover_header(&bytes), Err(CacheError::BadMagic));
        bytes[0] = b'X';
        bytes[9] = 1;
        assert_eq!(decode_cover_header(&bytes), Err(CacheError::BadLength));
    }

    #[test]
    fn toc_chapter_records_round_trip_and_truncate() {
        let header = TocFileHeader {
            source_hash: 0xABCD_1234,
            source_size: 1_726_241,
            chapter_count: 242,
        };
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        encode_toc_file_header(header, &mut header_bytes).expect("toc header encodes");
        assert_eq!(decode_toc_file_header(&header_bytes).unwrap(), header);

        // A short title survives a round-trip intact.
        let short = toc_chapter_record("Chapter 12", 1, 45);
        let mut bytes = [0u8; TOC_CHAPTER_RECORD_BYTES];
        encode_toc_chapter(&short, &mut bytes).expect("toc record encodes");
        let back = decode_toc_chapter(&bytes).unwrap();
        assert_eq!(
            (back.spine_index, back.level, back.title_str()),
            (45, 1, "Chapter 12")
        );

        // An over-budget title truncates to a valid char-boundary prefix,
        // backing off at most one multibyte char.
        let long = "A long chapter title \u{2014} reaching past the forty-four byte title budget";
        let record = toc_chapter_record(long, 2, -1);
        assert!(record.title_str().len() <= TOC_CHAPTER_TITLE_BYTES);
        assert!(record.title_str().len() >= TOC_CHAPTER_TITLE_BYTES - 3);
        assert!(long.starts_with(record.title_str()));
    }

    /// Drive the production stream walker over a CONT.BIN byte image the
    /// way the firmware does: decode the header, require the exact file
    /// length, then stream the records. Returns the replayed
    /// `(spine_index, text)` pairs on success.
    fn run_content_replay(
        data: &[u8],
    ) -> Result<
        (
            ContentReplayOutcome,
            std::vec::Vec<(u16, std::string::String)>,
        ),
        ContentReplayError,
    > {
        if data.len() < CONTENT_HEADER_BYTES {
            return Err(ContentReplayError);
        }
        let header =
            decode_content_header(&data[..CONTENT_HEADER_BYTES]).map_err(|_| ContentReplayError)?;
        if data.len() != header.content_len as usize {
            return Err(ContentReplayError);
        }
        let mut cursor = CONTENT_HEADER_BYTES;
        let mut read = |dst: &mut [u8]| -> Result<usize, ContentReplayError> {
            let len = dst.len().min(data.len() - cursor);
            dst[..len].copy_from_slice(&data[cursor..cursor + len]);
            cursor += len;
            Ok(len)
        };
        let mut blocks = std::vec::Vec::new();
        let mut on_group = |spine: u16,
                            group: &mut ContentGroupReader<'_>|
         -> Result<ContentReplayFlow, ContentReplayError> {
            while let Some((record, text)) = group.next_block()? {
                assert_eq!(record.spine_index, spine);
                blocks.push((spine, std::string::String::from(text)));
            }
            Ok(ContentReplayFlow::Continue)
        };
        // A deliberately small window forces compaction and refills.
        let mut buf = [0u8; 32];
        let outcome =
            replay_content_stream(&mut read, &mut buf, header.spine_count, &mut on_group)?;
        Ok((outcome, blocks))
    }

    #[test]
    fn test_content_replay_truncation_regression() {
        use crate::text::{TextAlign, TextRole};

        // Create a buffer for mock CONT.BIN
        let mut buffer = [0u8; 1024];

        // 1. Initial header (will overwrite at the end)
        let header = ContentHeader {
            source_hash: 0x12345678,
            source_size: 100000,
            complete: true,
            spine_count: 2,
            content_len: 0, // placeholder
        };
        encode_content_header(header, &mut buffer[..CONTENT_HEADER_BYTES]).unwrap();

        let mut offset = CONTENT_HEADER_BYTES;

        // Spine 0:
        // Record 1 (text block)
        let rec1 = ContentRecordHeader {
            spine_index: 0,
            text_len: 4,
            role: TextRole::Body,
            style: FontStyle::Regular,
            align: TextAlign::Left,
            paragraph_end: true,
            spine_end: false,
        };
        offset += encode_content_record_header(rec1, &mut buffer[offset..]).unwrap();
        buffer[offset..offset + 4].copy_from_slice(b"abcd");
        offset += 4;

        // Record 2 (spine end)
        let rec2 = ContentRecordHeader {
            spine_index: 0,
            text_len: 0,
            role: TextRole::Body,
            style: FontStyle::Regular,
            align: TextAlign::Left,
            paragraph_end: false,
            spine_end: true,
        };
        offset += encode_content_record_header(rec2, &mut buffer[offset..]).unwrap();
        let spine_0_end_offset = offset;

        // Spine 1:
        // Record 3 (text block)
        let rec3 = ContentRecordHeader {
            spine_index: 1,
            text_len: 4,
            role: TextRole::Body,
            style: FontStyle::Regular,
            align: TextAlign::Left,
            paragraph_end: true,
            spine_end: false,
        };
        offset += encode_content_record_header(rec3, &mut buffer[offset..]).unwrap();
        buffer[offset..offset + 4].copy_from_slice(b"efgh");
        offset += 4;

        // Record 4 (spine end)
        let rec4 = ContentRecordHeader {
            spine_index: 1,
            text_len: 0,
            role: TextRole::Body,
            style: FontStyle::Regular,
            align: TextAlign::Left,
            paragraph_end: false,
            spine_end: true,
        };
        offset += encode_content_record_header(rec4, &mut buffer[offset..]).unwrap();
        let full_len = offset;

        // Update the header with the correct total size
        let final_header = ContentHeader {
            source_hash: 0x12345678,
            source_size: 100000,
            complete: true,
            spine_count: 2,
            content_len: full_len as u32,
        };
        encode_content_header(final_header, &mut buffer[..CONTENT_HEADER_BYTES]).unwrap();

        // 2. Validate parsing of the FULL buffer
        let parsed_header = decode_content_header(&buffer[..CONTENT_HEADER_BYTES]).unwrap();
        assert_eq!(parsed_header.spine_count, 2);
        assert_eq!(parsed_header.content_len, full_len as u32);

        // Full replay through the production walker yields every block in
        // capture order and ends Complete.
        let (outcome, blocks) = run_content_replay(&buffer[..full_len]).expect("full replay");
        assert_eq!(outcome, ContentReplayOutcome::Complete);
        assert_eq!(
            blocks,
            std::vec![
                (0u16, std::string::String::from("abcd")),
                (1u16, std::string::String::from("efgh")),
            ]
        );

        // 3. Truncating immediately after spine 0's spine_end: the stale
        // header's content_len no longer matches the file length.
        assert!(run_content_replay(&buffer[..spine_0_end_offset]).is_err());

        // 4. Same truncation with a doctored header whose content_len
        // matches the truncated length: the walker itself must reject the
        // stream because only one of the two expected spine groups
        // replayed before clean EOF.
        let mut patched = [0u8; 1024];
        patched[..spine_0_end_offset].copy_from_slice(&buffer[..spine_0_end_offset]);
        encode_content_header(
            ContentHeader {
                source_hash: 0x12345678,
                source_size: 100000,
                complete: true,
                spine_count: 2,
                content_len: spine_0_end_offset as u32,
            },
            &mut patched[..CONTENT_HEADER_BYTES],
        )
        .unwrap();
        assert!(run_content_replay(&patched[..spine_0_end_offset]).is_err());

        // 5. Truncation mid-record (two bytes into spine 1's text), with
        // the header patched to match: EOF inside a group is corrupt.
        let cut = spine_0_end_offset + CONTENT_RECORD_HEADER_BYTES + 2;
        let mut mid_record = [0u8; 1024];
        mid_record[..cut].copy_from_slice(&buffer[..cut]);
        encode_content_header(
            ContentHeader {
                source_hash: 0x12345678,
                source_size: 100000,
                complete: true,
                spine_count: 2,
                content_len: cut as u32,
            },
            &mut mid_record[..CONTENT_HEADER_BYTES],
        )
        .unwrap();
        assert!(run_content_replay(&mid_record[..cut]).is_err());

        // 6. A record that changes spine index mid-group (spine 1's end
        // marker rewritten to claim spine 0) violates the group framing.
        let mut flipped = buffer;
        let rec4_offset = full_len - CONTENT_RECORD_HEADER_BYTES;
        encode_content_record_header(
            ContentRecordHeader {
                spine_index: 0,
                text_len: 0,
                role: TextRole::Body,
                style: FontStyle::Regular,
                align: TextAlign::Left,
                paragraph_end: false,
                spine_end: true,
            },
            &mut flipped[rec4_offset..rec4_offset + CONTENT_RECORD_HEADER_BYTES],
        )
        .unwrap();
        assert!(run_content_replay(&flipped[..full_len]).is_err());

        // 7. Two complete, internally valid groups in the wrong order
        // (spine 1's group before spine 0's): group indices must strictly
        // increase, or replay would publish chapters out of order instead
        // of falling back to the full build.
        let group_0 = &buffer[CONTENT_HEADER_BYTES..spine_0_end_offset];
        let group_1 = &buffer[spine_0_end_offset..full_len];
        let mut swapped = [0u8; 1024];
        swapped[..CONTENT_HEADER_BYTES].copy_from_slice(&buffer[..CONTENT_HEADER_BYTES]);
        swapped[CONTENT_HEADER_BYTES..CONTENT_HEADER_BYTES + group_1.len()]
            .copy_from_slice(group_1);
        swapped[CONTENT_HEADER_BYTES + group_1.len()..full_len].copy_from_slice(group_0);
        assert!(run_content_replay(&swapped[..full_len]).is_err());

        // 8. The same complete group twice: a duplicate index is equally
        // impossible in a real capture and must not publish a chapter
        // twice.
        let dup_len = CONTENT_HEADER_BYTES + 2 * group_0.len();
        let mut duplicated = [0u8; 1024];
        encode_content_header(
            ContentHeader {
                source_hash: 0x12345678,
                source_size: 100000,
                complete: true,
                spine_count: 2,
                content_len: dup_len as u32,
            },
            &mut duplicated[..CONTENT_HEADER_BYTES],
        )
        .unwrap();
        duplicated[CONTENT_HEADER_BYTES..CONTENT_HEADER_BYTES + group_0.len()]
            .copy_from_slice(group_0);
        duplicated[CONTENT_HEADER_BYTES + group_0.len()..dup_len].copy_from_slice(group_0);
        assert!(run_content_replay(&duplicated[..dup_len]).is_err());
    }
}
