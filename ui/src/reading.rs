//! Shared reading-surface layout: page bounds, type metrics, styled-text
//! measurement, and line wrapping. This is the "reader page plan" seam:
//! firmware reading views, cache building, and host preview tooling must
//! all agree on these numbers and this wrap behavior, so they live here.
//!
//! Measurement is incremental: width accumulates per character instead of
//! re-measuring a whole candidate line per word, which keeps wrapping O(n)
//! in text length.

use display::fb::{FbFrame, Framebuffer};
use display::font::{
    draw_text, family_weighted, fixed_ceil, fixed_round, measure_text, style_from_marker_code,
    BitmapFont, FontSize, FontStyle, LineSpacing, TypeSettings, STYLE_MARKER,
};
use proto::cache::{BlockRecord, PageRecord};
use proto::text::{TextAlign, TextRole};

/// The reading page's text box: wrap edges and the vertical page walk
/// bounds. Landscape keeps the historical `READER_*` numbers; portrait
/// stands the panel's long axis upright, so lines wrap at the short axis
/// and pages run the full long axis. Pagination, cache building, and
/// drawing all read the box from the [`ReadingBlocks`] source, so a store
/// can never wrap under one box and paginate under another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageBox {
    pub left: i16,
    pub right: i16,
    pub top: i16,
    pub bottom: i16,
}

impl PageBox {
    pub const LANDSCAPE: Self = Self {
        left: READER_LEFT_X,
        right: READER_RIGHT_X,
        top: READER_PAGE_TOP,
        bottom: READER_PAGE_BOTTOM,
    };

    /// Portrait keeps the same insets as landscape — 8 from the wrap
    /// edges, 6 above, 23 clear of the footer — inside the upright frame.
    pub const PORTRAIT: Self = Self {
        left: 8,
        right: FbFrame::Portrait.width() as i16 - 8,
        top: 6,
        bottom: FbFrame::Portrait.height() as i16 - 23,
    };

    pub const fn for_portrait(portrait: bool) -> Self {
        if portrait {
            Self::PORTRAIT
        } else {
            Self::LANDSCAPE
        }
    }

    /// Left text edge for a role: block quotes indent into the page.
    pub const fn x_for(self, role: TextRole) -> i16 {
        if matches!(role, TextRole::BlockQuote) {
            self.left + 24
        } else {
            self.left
        }
    }
}

/// Narrow read model for the reader page plan: bounded block records,
/// their cached text, and pagination flags. Firmware's ReaderStore and
/// host-side fixtures both implement it, so pagination and page drawing
/// cannot drift between device and tools.
pub trait ReadingBlocks {
    fn block_count(&self) -> usize;
    /// Record at `index` while it is inside the live block range.
    fn block(&self, index: usize) -> Option<BlockRecord>;
    fn block_text(&self, index: usize) -> &str;
    fn block_style(&self, index: usize) -> FontStyle;
    fn page_break_before(&self, index: usize) -> bool;
    fn paragraph_end(&self, index: usize) -> bool;
    /// True when block `index` opens a paragraph: the block right after a
    /// paragraph end, and the very first block. Only the opening line of a
    /// Body paragraph takes the first-line indent. The default derives it
    /// from `paragraph_end`; a store that carries a half-finished paragraph
    /// into the next section overrides this with a persisted flag, so a
    /// carried continuation line is not mistaken for a fresh paragraph.
    fn paragraph_start(&self, index: usize) -> bool {
        index == 0 || self.paragraph_end(index.wrapping_sub(1))
    }
    /// Type settings the blocks were laid out under. Every height,
    /// pagination, and drawing call in this module reads them from the
    /// source, so a store can never paginate with one size and draw with
    /// another.
    fn type_settings(&self) -> TypeSettings {
        TypeSettings::DEFAULT
    }
    /// The page box the blocks were laid out into. Reads like
    /// `type_settings`: heights, pagination, and drawing all follow the
    /// source's box, which changes with the orientation setting.
    fn page_box(&self) -> PageBox {
        PageBox::LANDSCAPE
    }
}

pub struct ReaderDrawableBlock<'a> {
    pub record: BlockRecord,
    pub text: &'a str,
    pub y: i16,
    pub advance: i16,
    pub style: FontStyle,
    pub font: &'static BitmapFont,
    /// First-line indent for the block's opening line; 0 for continuation
    /// lines, headings, and centered text.
    pub indent: i16,
}

pub fn block_height(source: &impl ReadingBlocks, index: usize) -> i16 {
    let Some(record) = source.block(index) else {
        return 0;
    };
    let settings = source.type_settings();
    let advance = line_advance(settings, record.role);
    let height = if record.line_count == 1 {
        advance
    } else {
        wrapped_block_height(
            body_font(settings, source.block_style(index)),
            source.block_text(index),
            record.role,
            record.align,
            advance,
            block_first_line_indent(source, index),
            source.page_box(),
        )
    };
    height + paragraph_gap_after(source, index)
}

/// Block height without the trailing paragraph gap: the rows the block's
/// own ink occupies. Pagination charges this against the page edge — the
/// gap only separates blocks that share a page — while the cursor still
/// advances by the gapped height.
pub fn block_ink_height(source: &impl ReadingBlocks, index: usize) -> i16 {
    block_height(source, index) - paragraph_gap_after(source, index)
}

pub fn paragraph_gap_after(source: &impl ReadingBlocks, index: usize) -> i16 {
    if source.paragraph_end(index) {
        paragraph_gap(
            source
                .block(index)
                .map(|record| record.role)
                .unwrap_or(TextRole::Body),
        )
    } else {
        0
    }
}

/// Count the pages the loaded blocks paginate into, using the same height
/// math as rendering.
pub fn paginate_block_pages(source: &impl ReadingBlocks) -> usize {
    let PageBox {
        top: page_top,
        bottom: page_bottom,
        ..
    } = source.page_box();
    let mut pages = 1u32;
    let mut y = page_top;

    for index in 0..source.block_count() {
        if source.page_break_before(index) && y > page_top {
            pages = pages.saturating_add(1);
            y = page_top;
        }
        let height = block_height(source, index);

        if y + block_ink_height(source, index) > page_bottom && y > page_top {
            pages = pages.saturating_add(1);
            y = page_top;
        }
        y += height;
    }

    pages.max(1) as usize
}

/// Walk the blocks until `page_index` and return its page record.
pub fn page_record_at(source: &impl ReadingBlocks, page_index: usize) -> PageRecord {
    let PageBox {
        top: page_top,
        bottom: page_bottom,
        ..
    } = source.page_box();
    let mut current = 0usize;
    let mut first_block = 0usize;
    let mut block_count = 0usize;
    let mut y = page_top;

    for index in 0..source.block_count() {
        let height = block_height(source, index);
        let new_page = (y + block_ink_height(source, index) > page_bottom
            || source.page_break_before(index))
            && y > page_top;
        if new_page {
            if current == page_index {
                return PageRecord {
                    first_block: first_block as u16,
                    block_count: block_count as u16,
                };
            }
            current += 1;
            first_block = index;
            block_count = 0;
            y = page_top;
        }
        block_count += 1;
        y += height;
    }

    PageRecord {
        first_block: first_block as u16,
        block_count: block_count as u16,
    }
}

/// Where an appended block lands in the page index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockPlacement {
    /// The block joins the page in progress (or opens the very first page).
    SamePage,
    /// The block opens a new page.
    NewPage,
}

/// Incremental page-index cursor: the running `(y, last block height)` of
/// the firmware's `rebuild_page_index` walk, advanced one block at a time
/// as the cache build appends lines — O(1) per line instead of a full
/// re-walk of every accumulated block.
///
/// Invariant: driving [`Self::place_next_block`] over blocks `0..n` and
/// mirroring each placement with [`apply_block_placement`] yields page
/// records bit-identical to the full rebuild walk. Page records persist
/// into section files, so any divergence is silent cache corruption; the
/// host tests below check the cursor against a naive full walk on every
/// step, including retroactive paragraph-end growth and capacity overflow.
#[derive(Clone, Copy, Debug)]
pub struct PageIndexCursor {
    y: i16,
    /// Height charged for the most recently placed block, kept so a
    /// retroactive height change can re-derive the pre-placement `y`.
    last_block_height: i16,
}

impl PageIndexCursor {
    /// A cursor over an empty block set laid into `page_box`.
    pub const fn start(page_box: PageBox) -> Self {
        Self {
            y: page_box.top,
            last_block_height: 0,
        }
    }

    /// Place the block just appended at `index` (which must be the last
    /// block, placed exactly once, in order). Uses the same decision as the
    /// full rebuild walk: a block starts a new page when its gapped height
    /// overflows the page or it demands a break, and the page already has
    /// content.
    pub fn place_next_block(
        &mut self,
        source: &impl ReadingBlocks,
        index: usize,
    ) -> BlockPlacement {
        let PageBox { top, bottom, .. } = source.page_box();
        let height = block_height(source, index);
        let new_page =
            (self.y + height > bottom || source.page_break_before(index)) && self.y > top;
        if new_page {
            self.y = top;
        }
        self.y += height;
        self.last_block_height = height;
        if new_page {
            BlockPlacement::NewPage
        } else {
            BlockPlacement::SamePage
        }
    }

    /// Re-place the most recently placed block after a retroactive height
    /// change — a trailing paragraph-end mark adds the paragraph gap after
    /// the block has already been placed. Returns `NewPage` when the grown
    /// block no longer fits the page it joined and a full walk would move it
    /// to a fresh page (the caller then mirrors the move with
    /// [`apply_last_block_move`]); a block that already opened its page can
    /// only grow in place. Heights never shrink here (the gap is
    /// non-negative), so a placed `NewPage` decision never reverts.
    pub fn replace_last_block(
        &mut self,
        source: &impl ReadingBlocks,
        index: usize,
    ) -> BlockPlacement {
        let PageBox { top, bottom, .. } = source.page_box();
        let y_before = self.y - self.last_block_height;
        let height = block_height(source, index);
        let new_page =
            (y_before + height > bottom || source.page_break_before(index)) && y_before > top;
        self.y = if new_page {
            top + height
        } else {
            y_before + height
        };
        self.last_block_height = height;
        if new_page {
            BlockPlacement::NewPage
        } else {
            BlockPlacement::SamePage
        }
    }
}

/// Mirror one [`PageIndexCursor`] placement into bounded page-record
/// arrays, replicating the full rebuild's behavior exactly — including the
/// silent drop of page records past `pages.len()` (`overflowed` then latches
/// so later same-page blocks don't grow an unrelated record).
pub fn apply_block_placement(
    placement: BlockPlacement,
    index: usize,
    spine: u16,
    pages: &mut [PageRecord],
    page_spine: &mut [u16],
    page_count: &mut usize,
    overflowed: &mut bool,
) {
    let open_first = *page_count == 0 && matches!(placement, BlockPlacement::SamePage);
    match placement {
        BlockPlacement::SamePage if !open_first => {
            if !*overflowed {
                pages[*page_count - 1].block_count += 1;
            }
        }
        _ => {
            if *overflowed || *page_count >= pages.len() {
                *overflowed = true;
                return;
            }
            pages[*page_count] = PageRecord {
                first_block: index as u16,
                block_count: 1,
            };
            page_spine[*page_count] = spine;
            *page_count += 1;
        }
    }
}

/// Mirror a [`PageIndexCursor::replace_last_block`] move: the grown block
/// leaves the tail of the page in progress and opens a new page. Follows
/// the same capacity rule as [`apply_block_placement`].
pub fn apply_last_block_move(
    index: usize,
    spine: u16,
    pages: &mut [PageRecord],
    page_spine: &mut [u16],
    page_count: &mut usize,
    overflowed: &mut bool,
) {
    if *overflowed || *page_count == 0 {
        // The block lives past the recorded pages (or nothing is recorded);
        // a full rebuild would not record its page either.
        return;
    }
    let last = *page_count - 1;
    pages[last].block_count = pages[last].block_count.saturating_sub(1);
    if *page_count >= pages.len() {
        *overflowed = true;
        return;
    }
    pages[*page_count] = PageRecord {
        first_block: index as u16,
        block_count: 1,
    };
    page_spine[*page_count] = spine;
    *page_count += 1;
}

pub fn for_each_drawable_block(
    source: &impl ReadingBlocks,
    page: PageRecord,
    mut visit: impl FnMut(ReaderDrawableBlock<'_>) -> bool,
) {
    let settings = source.type_settings();
    let page_box = source.page_box();
    let mut y = page_box.top;
    for offset in 0..page.block_count as usize {
        let index = page.first_block as usize + offset;
        let Some(record) = source.block(index) else {
            break;
        };
        let text = source.block_text(index);
        let advance = line_advance(settings, record.role);
        let style = source.block_style(index);
        let height = block_height(source, index);
        if y + block_ink_height(source, index) > page_box.bottom && y > page_box.top {
            break;
        }
        if !visit(ReaderDrawableBlock {
            record,
            text,
            y: y + advance,
            advance,
            style,
            font: body_font(settings, style),
            indent: block_first_line_indent(source, index),
        }) {
            break;
        }
        y += height;
    }
}

/// Draw one page of reading-body blocks: the single rendering of cached
/// reader content shared by firmware views and host tooling.
pub fn draw_reading_page_body(fb: &mut Framebuffer, source: &impl ReadingBlocks, page: PageRecord) {
    let settings = source.type_settings();
    let page_box = source.page_box();
    for_each_drawable_block(source, page, |block| {
        let role = block.record.role;
        match block.record.align {
            TextAlign::Left => {
                let x = page_box.x_for(role);
                if block.record.line_count == 1 {
                    draw_styled_line(
                        fb,
                        settings,
                        block.text,
                        x + block.indent,
                        block.y,
                        block.style,
                    );
                } else {
                    draw_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        x,
                        block.y,
                        page_box.right,
                        block.advance,
                        block.indent,
                    );
                }
            }
            TextAlign::Justify => {
                let x = page_box.x_for(role);
                if block.record.line_count == 1 {
                    draw_styled_line(
                        fb,
                        settings,
                        block.text,
                        x + block.indent,
                        block.y,
                        block.style,
                    );
                } else {
                    draw_justified_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        x,
                        block.y,
                        page_box.right,
                        block.advance,
                        block.indent,
                    );
                }
            }
            TextAlign::Center => {
                if block.record.line_count == 1 {
                    let width = styled_text_ink_width(block.text, settings, block.style)
                        .min(page_box.right - page_box.left);
                    let x = ((page_box.left + page_box.right - width) / 2).max(page_box.left);
                    draw_styled_line(fb, settings, block.text, x, block.y, block.style);
                } else {
                    draw_centered_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        block.y,
                        page_box.right - page_box.left,
                        block.advance,
                    );
                }
            }
        };
        true
    });
}

/// Draw the page-in-chapter counter that completes the reading surface.
/// Callers own the chapter-position calculation and formatting; this shared
/// seam owns the exact font, right inset, and panel-relative baseline.
pub fn draw_reading_page_counter(fb: &mut Framebuffer, label: &str) {
    draw_reading_page_counter_aligned(fb, label, false);
}

pub fn draw_reading_page_counter_aligned(fb: &mut Framebuffer, label: &str, left: bool) {
    // Frame-relative, not panel-relative: the portrait page's footer sits
    // at the bottom of the upright frame. Landscape frames keep the
    // historical panel numbers.
    let font = display::font::literata_small(FontStyle::Regular);
    let frame_right = fb.frame_width() as i16 - 8;
    let baseline = fb.frame_height() as i16 - 3;
    let x = if left {
        READER_LEFT_X + 16
    } else {
        let width = measure_text(font, label) as i16;
        frame_right - width - 16
    };
    draw_text(fb, font, label, x, baseline, false);
}

pub const READER_PAGE_TOP: i16 = 6;
/// Footer band top: 14 rows up from the panel's bottom edge. Panel-relative
/// so the X3's taller page pushes the footer to its own bottom edge; on the
/// X4 this is the historical 466.
pub const READER_FOOTER_TOP: i16 = display::HEIGHT as i16 - 14;
/// Page-counter text baseline: as low as it goes without clipping (the
/// slash inks 2 rows below its baseline). Used by `fw::views` so the
/// footer's exact panel-relative position lives in one place instead of
/// being kept in sync by hand across crates.
pub const READER_FOOTER_BASELINE_Y: i16 = display::HEIGHT as i16 - 3;
/// Last permissible baseline row for body ink. Derived, not tuned: the
/// page counter's '/' ink starts 12 rows up from its baseline
/// (`READER_FOOTER_BASELINE_Y`), and the deepest body glyph reaches 7 rows
/// below its baseline (comma-below diacritics), so bottom - 3 - 12 - 7 - 1
/// keeps every possible descender a clear row away from the counter. On the
/// X4 this is the historical 457.
pub const READER_PAGE_BOTTOM: i16 = display::HEIGHT as i16 - 23;
/// Rows the portrait reading sheet covers when summoned. The portrait
/// reader reserves no space for it: the page and its footer use the full
/// frame height, and the sheet draws on top of the footer and bottom text
/// lines while it is up. Shallow because the portrait key strip names keys
/// with single 24px icons centered 24 rows up.
pub const PORTRAIT_READING_SHEET_HEIGHT: i16 = 48;
pub const READER_LEFT_X: i16 = 8;
/// Right text margin: 8 rows in from the panel's right edge (the X4's
/// historical 792). Panel-relative so the X3's narrower panel keeps the
/// same inset rather than letting body ink touch the edge.
pub const READER_RIGHT_X: i16 = display::WIDTH as i16 - 8;
pub const READER_WRAP_SAFETY: i16 = 4;
/// Version of the wrap rules and page constants in this module, and of the
/// cached section content keyed off it. Bump when layout changes for
/// unchanged type settings, or when the cache encoding changes so stale
/// sections rebuild. v8: chapters no longer truncate at the text budget and
/// style markers are stored only on run change. v9: intermediate sections
/// end on a whole page (the half-finished page carries into the next
/// section) so chunk seams no longer leave a short, half-empty page. v10:
/// the full chapter list is written to its own TOC.BIN at build time and
/// read on demand for the overview, so a long book's TOC is no longer capped
/// at 128; existing caches rebuild to produce that file. v11: the TOC.BIN
/// chapter-title budget grew 44->60 bytes (record 48->64), so the colophon
/// shows longer chapter names; existing caches rebuild to widen their records.
/// v12: Body paragraphs take a book-style first-line indent instead of an
/// inter-paragraph gap. The indent narrows each paragraph's opening line, so
/// wrap points and the persisted paragraph-start flag change; existing caches
/// rebuild.
/// v13: the Type Weight setting joins the layout config. Heavier (SemiBold)
/// body glyphs are wider than Regular, so wrap points change with weight;
/// existing caches rebuild on a weight change.
/// v14: the Font setting joins the layout config. The second family's advances
/// differ from Literata's at every size, so wrap points change with family;
/// existing caches rebuild on a family change.
/// v15: the second family changed from Bookerly to Merriweather. Its advances
/// differ, but the family bit (1) does not, so a version bump is what retires
/// any pagination cached under the old face. v16: font advances moved to 12.4
/// fixed point and generated kerning tables now affect line widths. v17:
/// family widened from one bit to two bits so the Custom slot cannot collide
/// with the version field. v18: the page box joins the layout config as a
/// portrait bit — the upright frame wraps at the short axis, so portrait
/// and landscape pagination cache separately. v19: the generated glyph boxes
/// come from the monochrome rasterization mode instead of the antialiased
/// one, so they contain the bitmap that is actually stored rather than
/// clipping it. Advances are unchanged, but `x_offset + width` moves on 737
/// of 49,802 renders, which is a wrap input; existing caches rebuild.
const READER_LAYOUT_VERSION: u16 = 19;

/// Panel-geometry salt folded into the version bits: wrap points and page
/// heights depend on the page box, so pagination cached on one panel must
/// read as stale on the other (an SD card can move between an X4 and an
/// X3). Zero on the X4's 800x480 keeps every existing cache valid; other
/// geometries claim a disjoint version band well clear of routine bumps.
/// (128, not the former 256: the config field gained the portrait bit, so
/// version + salt must stay under 256 to fit the u16.)
const PANEL_LAYOUT_SALT: u16 = if display::WIDTH == 800 && display::HEIGHT == 480 {
    0
} else {
    128
};

const _: () = assert!(READER_LAYOUT_VERSION + PANEL_LAYOUT_SALT < 256);

/// Section cache layout config: the wrap-rule version plus the layout the
/// section was paginated under. Stored in cache headers; a mismatch on
/// load invalidates the cached pagination and rebuilds it. Bit layout:
/// spacing in bits 0-1 (a spacing change only re-walks heights, so the
/// load check masks these off), size in bits 2-3, weight in bit 4, family
/// in bits 5-6, portrait in bit 7, version above. Size, weight, family,
/// and the page box all change wrap points, so a change in any forces a
/// full rebuild.
pub fn reader_layout_config(settings: TypeSettings, portrait: bool) -> u16 {
    ((READER_LAYOUT_VERSION + PANEL_LAYOUT_SALT) << 8)
        | ((portrait as u16) << 7)
        | ((settings.family as u16) << 5)
        | ((settings.weight as u16) << 4)
        | ((settings.size as u16) << 2)
        | settings.spacing as u16
}

/// The part of the layout that names a stored pagination.
///
/// The wrap-point inputs only: size, weight, family, and the page box. Two
/// layouts differing in any of those break pages in different places, so the
/// cache names them apart and keeps both.
///
/// Line spacing stays out. A spacing change re-walks heights over the same
/// wrap points, so both spacings share one stored set and the header check
/// sorts them out. The wrap-rule version and panel salt stay out because a
/// bump must retire every layout, which each index's own header does by
/// rejecting itself; in the name it would strand a fresh set of files with no
/// reader left to delete the old one.
pub fn layout_key(settings: TypeSettings, portrait: bool) -> u8 {
    ((reader_layout_config(settings, portrait) >> 2) & 0x3F) as u8
}

/// The reading body face for the given settings and style run.
pub fn body_font(settings: TypeSettings, style: FontStyle) -> &'static BitmapFont {
    family_weighted(settings.family, settings.size, settings.weight, style)
}

/// Baseline-to-baseline advance. Body values per (size, spacing); H1/H2
/// carry extra lead. Medium/Normal runs 26 (130% leading) so the default
/// page grid closes at seventeen lines: 6 + 17*26 = 448 <= 457, where the
/// historical 27 left a dead row above the footer on every full page.
pub fn line_advance(settings: TypeSettings, role: TextRole) -> i16 {
    let body = match (settings.size, settings.spacing) {
        (FontSize::Small, LineSpacing::Compact) => 22,
        (FontSize::Small, LineSpacing::Normal) => 24,
        (FontSize::Small, LineSpacing::Relaxed) => 28,
        (FontSize::Medium, LineSpacing::Compact) => 25,
        (FontSize::Medium, LineSpacing::Normal) => 26,
        (FontSize::Medium, LineSpacing::Relaxed) => 31,
        (FontSize::Large, LineSpacing::Compact) => 29,
        (FontSize::Large, LineSpacing::Normal) => 32,
        (FontSize::Large, LineSpacing::Relaxed) => 36,
    };
    if matches!(role, TextRole::Heading1 | TextRole::Heading2) {
        body + 5
    } else {
        body
    }
}

pub fn paragraph_gap(role: TextRole) -> i16 {
    match role {
        TextRole::Heading1 | TextRole::Heading2 => 10,
        TextRole::Heading3 => 6,
        TextRole::BlockQuote => 6,
        // Body paragraphs separate by their first-line indent, book-style,
        // rather than an inter-paragraph gap: the two together read as
        // redundant. See [`paragraph_indent`].
        TextRole::Body => 0,
    }
}

/// First-line paragraph indent, in pixels, scaled to the body size. This is
/// the book-style paragraph cue that replaces the inter-paragraph gap for
/// Body text (see [`paragraph_gap`]). Roughly 1.3em at each size.
pub fn paragraph_indent(size: FontSize) -> i16 {
    match size {
        FontSize::Small => 24,
        FontSize::Medium => 28,
        FontSize::Large => 34,
    }
}

/// The first-line indent block `index` draws with, or 0 when it takes none:
/// non-Body roles, centered text, and continuation lines of a paragraph that
/// wrapped or carried across a section boundary all stay flush left.
pub fn block_first_line_indent(source: &impl ReadingBlocks, index: usize) -> i16 {
    let Some(record) = source.block(index) else {
        return 0;
    };
    if matches!(record.role, TextRole::Body)
        && matches!(record.align, TextAlign::Left | TextAlign::Justify)
        && source.paragraph_start(index)
    {
        paragraph_indent(source.type_settings().size)
    } else {
        0
    }
}

pub fn reader_x_for(role: TextRole) -> i16 {
    PageBox::LANDSCAPE.x_for(role)
}

/// Running ink measurement: pen advance plus the rightmost inked edge,
/// which can exceed the advance for glyphs whose bitmap overhangs their
/// advance width (italics, some punctuation).
#[derive(Clone, Copy, Debug, Default)]
pub struct InkCursor {
    advance_fp: i32,
    right: i16,
    previous: Option<u16>,
}

impl InkCursor {
    pub const fn new() -> Self {
        Self {
            advance_fp: 0,
            right: 0,
            previous: None,
        }
    }

    #[inline]
    pub fn push_char(&mut self, font: &BitmapFont, ch: char) {
        let codepoint = if ch as u32 > u16::MAX as u32 {
            b'?' as u16
        } else {
            ch as u16
        };
        let Some((metric, _)) = font.glyph(codepoint).or_else(|| font.glyph(b'?' as u16)) else {
            self.advance_fp += 8 << 4;
            self.right = self.right.max(fixed_ceil(self.advance_fp));
            self.previous = Some(b'?' as u16);
            return;
        };
        let drawn = if font.glyph(codepoint).is_some() {
            codepoint
        } else {
            b'?' as u16
        };
        if let Some(left) = self.previous {
            self.advance_fp += font.kerning_adjust_fp(left, drawn) as i32;
        }
        let advance = fixed_round(self.advance_fp);
        let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance_fp += metric.advance_fp as i32;
        self.previous = Some(drawn);
    }

    #[inline]
    pub fn reset_pair(&mut self) {
        self.previous = None;
    }

    pub fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}

pub fn text_ink_width(font: &'static BitmapFont, text: &str) -> i16 {
    let mut ink = InkCursor::new();
    for ch in text.chars() {
        ink.push_char(font, ch);
    }
    ink.width()
}

/// Incremental ink measurement over cached styled text: [`STYLE_MARKER`]
/// followed by a style digit switches the active font mid-stream, staying
/// inside one type size. `Copy`, so callers can checkpoint before a word
/// and roll back on overflow.
#[derive(Clone, Copy)]
pub struct StyledInkCursor {
    ink: InkCursor,
    settings: TypeSettings,
    font: &'static BitmapFont,
}

impl StyledInkCursor {
    pub fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        Self {
            ink: InkCursor::new(),
            settings,
            font: body_font(settings, default_style),
        }
    }

    /// A [`STYLE_MARKER`] and its code digit must arrive within one
    /// fragment; a marker split across fragments loses its style switch.
    pub fn push_str(&mut self, text: &str) {
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == STYLE_MARKER {
                if let Some(code) = chars.next() {
                    self.ink.reset_pair();
                    self.font = body_font(
                        self.settings,
                        style_from_marker_code(code).unwrap_or(FontStyle::Regular),
                    );
                }
                continue;
            }
            self.ink.push_char(self.font, ch);
        }
    }

    pub fn width(&self) -> i16 {
        self.ink.width()
    }
}

pub fn styled_text_ink_width(text: &str, settings: TypeSettings, default_style: FontStyle) -> i16 {
    let mut cursor = StyledInkCursor::new(settings, default_style);
    cursor.push_str(text);
    cursor.width()
}

pub fn first_styled_line_style(text: &str) -> Option<FontStyle> {
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == STYLE_MARKER {
            return chars.next().and_then(style_from_marker_code);
        }
    }
    None
}

/// Greedy word wrap step. Starting at `cursor` (skipping leading ASCII
/// whitespace), returns `(line_start, line_end, next_cursor)` for the
/// longest run of words whose ink width fits `x..max_x`, or a single
/// overlong word when nothing fits.
pub fn next_wrapped_line(
    text: &str,
    mut cursor: usize,
    font: &'static BitmapFont,
    x: i16,
    max_x: i16,
) -> Option<(usize, usize, usize)> {
    let bytes = text.as_bytes();
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }

    let mut ink = InkCursor::new();
    let mut measured_to = cursor;
    let mut scan = cursor;
    let mut best_end = cursor;
    let mut best_next = cursor;
    while scan < bytes.len() {
        let word_start = scan;
        while scan < bytes.len() && !bytes[scan].is_ascii_whitespace() {
            scan += 1;
        }
        let word_end = scan;
        for ch in text[measured_to..word_end].chars() {
            ink.push_char(font, ch);
        }
        measured_to = word_end;
        if x + ink.width() + READER_WRAP_SAFETY > max_x {
            if best_end == cursor {
                return Some((word_start, word_end, word_end));
            }
            return Some((cursor, best_end, best_next));
        }
        best_end = word_end;
        while scan < bytes.len() && bytes[scan].is_ascii_whitespace() {
            scan += 1;
        }
        best_next = scan;
    }

    Some((cursor, best_end, best_next.max(best_end)))
}

pub fn wrapped_line_count(
    font: &'static BitmapFont,
    text: &str,
    max_width: i16,
    first_indent: i16,
) -> u16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    let mut lines = 0u16;
    // The opening line starts `first_indent` in, so it holds fewer words;
    // every following line runs the full width.
    let mut x = first_indent;
    while let Some((_, _, next_cursor)) = next_wrapped_line(text, cursor, font, x, max_width) {
        lines = lines.saturating_add(1);
        x = 0;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }
    lines
}

#[allow(clippy::too_many_arguments)]
pub fn wrapped_block_height(
    font: &'static BitmapFont,
    text: &str,
    role: TextRole,
    align: TextAlign,
    line_advance: i16,
    first_indent: i16,
    page_box: PageBox,
) -> i16 {
    let max_width = page_box.right
        - if align == TextAlign::Center {
            page_box.left
        } else {
            page_box.x_for(role)
        };
    wrapped_line_count(font, text, max_width, first_indent).max(1) as i16 * line_advance
}

pub fn draw_styled_line(
    fb: &mut Framebuffer,
    settings: TypeSettings,
    text: &str,
    x: i16,
    baseline_y: i16,
    default_style: FontStyle,
) -> i16 {
    let mut cursor_x = x;
    let mut run_start = 0usize;
    let mut style = default_style;
    let mut iter = text.char_indices().peekable();
    while let Some((index, ch)) = iter.next() {
        if ch != STYLE_MARKER {
            continue;
        }
        if run_start < index {
            cursor_x = draw_text(
                fb,
                body_font(settings, style),
                &text[run_start..index],
                cursor_x,
                baseline_y,
                false,
            );
        }
        if let Some((code_index, code)) = iter.next() {
            style = style_from_marker_code(code).unwrap_or(style);
            run_start = code_index + code.len_utf8();
        } else {
            run_start = index + ch.len_utf8();
        }
    }
    if run_start < text.len() {
        cursor_x = draw_text(
            fb,
            body_font(settings, style),
            &text[run_start..],
            cursor_x,
            baseline_y,
            false,
        );
    }
    cursor_x
}

pub fn draw_centered_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    mut baseline_y: i16,
    max_width: i16,
    line_advance: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, 0, max_width)
    {
        let line = &text[line_start..line_end];
        let width = text_ink_width(font, line).min(max_width);
        let x = ((fb.frame_width() as i16 - width) / 2).max(20);
        draw_text(fb, font, line, x, baseline_y, false);
        baseline_y += line_advance;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

#[allow(clippy::too_many_arguments)]
pub fn draw_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    x: i16,
    mut baseline_y: i16,
    max_x: i16,
    line_advance: i16,
    first_indent: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    let mut line_x = x + first_indent;
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, line_x, max_x)
    {
        draw_text(
            fb,
            font,
            &text[line_start..line_end],
            line_x,
            baseline_y,
            false,
        );
        baseline_y += line_advance;
        line_x = x;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

#[allow(clippy::too_many_arguments)]
pub fn draw_justified_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    x: i16,
    mut baseline_y: i16,
    max_x: i16,
    line_advance: i16,
    first_indent: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    let mut line_x = x + first_indent;
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, line_x, max_x)
    {
        let is_last_line = next_cursor >= bytes.len();
        draw_justified_line(
            fb,
            font,
            &text[line_start..line_end],
            line_x,
            baseline_y,
            max_x,
            is_last_line,
        );
        baseline_y += line_advance;
        line_x = x;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

/// Hands out the pixels of a justified line's slack that do not divide evenly
/// between its gaps, one gap at a time.
///
/// Spending them on the first gaps -- which is what `remainder > 0 { gap += 1 }`
/// does -- makes every justified line left-heavy: ten gaps with six spare
/// pixels gave the first six gaps an extra pixel and the last four none, on
/// every line of every page. Carrying the shortfall the way a Bresenham line
/// carries its error spreads the wider gaps evenly instead.
///
/// The error starts half a gap in rather than at zero, which is the centred
/// form of the same idea. From zero the accumulator cannot reach `gap_count`
/// before the last call, so the final gap of every line took a pixel whenever
/// there was one to give -- left-heavy traded for right-heavy. Half a gap of
/// head start puts the lone wider gap of a remainder-1 line near the middle.
///
/// The total is unchanged either way: `next` returns 1 exactly `remainder`
/// times across `gap_count` calls, because `carry` gains `remainder` per call,
/// sheds `gap_count` per carry, and starts below `gap_count`, so the line
/// still ends where it did.
struct GapSlack {
    remainder: i16,
    gap_count: i16,
    carry: i16,
}

impl GapSlack {
    fn new(extra: i16, gap_count: usize) -> Self {
        let gap_count = gap_count.min(i16::MAX as usize) as i16;
        Self {
            remainder: if gap_count > 0 { extra % gap_count } else { 0 },
            gap_count,
            carry: gap_count / 2,
        }
    }

    fn next(&mut self) -> i16 {
        if self.gap_count <= 0 {
            return 0;
        }
        self.carry += self.remainder;
        if self.carry >= self.gap_count {
            self.carry -= self.gap_count;
            1
        } else {
            0
        }
    }
}

/// Whether the space at `index` closes an inter-word gap: the last space of
/// its run, with a word after it.
///
/// `next_wrapped_line` hands the line over with its internal whitespace
/// verbatim, so a run of spaces is one gap made of several bytes. Counting
/// the gaps and widening them have to agree on that or the line overruns its
/// measured width, which is why both go through this predicate.
fn closes_word_gap(bytes: &[u8], index: usize) -> bool {
    bytes[index] == b' ' && bytes.get(index + 1).is_some_and(|next| *next != b' ')
}

fn draw_justified_line(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    line: &str,
    x: i16,
    baseline_y: i16,
    max_x: i16,
    is_last_line: bool,
) {
    let gap_count = (0..line.len())
        .filter(|index| closes_word_gap(line.as_bytes(), *index))
        .count();
    if is_last_line || gap_count == 0 {
        draw_text(fb, font, line, x, baseline_y, false);
        return;
    }

    let text_width = text_ink_width(font, line);
    let extra = (max_x - x - READER_WRAP_SAFETY - text_width).max(0);
    let extra_per_gap = extra / gap_count as i16;
    // The pixels that do not divide evenly, spread across the line instead of
    // spent on the first gaps. Handing the remainder out front-to-back made
    // every justified line left-heavy: with ten gaps and six spare pixels the
    // first six gaps were a pixel wider than the last four, on every line of
    // every page, which reads as a slight leftward crowding. This carries the
    // shortfall the way a Bresenham line carries its error, so the wider gaps
    // land evenly. The total is identical either way -- exactly `remainder`
    // gaps get the extra pixel, so the line still ends where it did.
    let mut slack = GapSlack::new(extra, gap_count);
    let mut cursor_x = x;
    let mut word_start = None;

    for (index, byte) in line.bytes().enumerate() {
        if byte == b' ' {
            if let Some(start) = word_start.take() {
                let word = &line[start..index];
                cursor_x = draw_text(fb, font, word, cursor_x, baseline_y, false);
            }
            cursor_x += measure_text(font, " ") as i16;
            // The slack belongs to the gap, not to each space byte in it.
            // `extra` was measured against a `text_ink_width` that already
            // counts every space's own advance, so spending `extra_per_gap`
            // once per byte would widen a double-space line past `max_x` and
            // pull `slack` past its remainder.
            if closes_word_gap(line.as_bytes(), index) {
                cursor_x += extra_per_gap + slack.next();
            }
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }
    if let Some(start) = word_start {
        draw_text(fb, font, &line[start..], cursor_x, baseline_y, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slack a justified line cannot divide evenly must be spread across
    /// it, not spent on the first gaps.
    #[test]
    fn justified_slack_is_spread_not_front_loaded() {
        // Ten gaps, six spare pixels. Front-loading gave 1,1,1,1,1,1,0,0,0,0.
        let mut slack = GapSlack::new(26, 10);
        let handed: heapless::Vec<i16, 10> = (0..10).map(|_| slack.next()).collect();
        assert_eq!(
            handed.iter().sum::<i16>(),
            6,
            "total slack must be preserved"
        );
        // No run of extra pixels longer than the ratio demands: with 6 of 10
        // the widest run is two, and the first four gaps are not all wider.
        assert!(
            handed[..4].iter().sum::<i16>() < 4,
            "the leading gaps must not absorb the whole remainder: {handed:?}"
        );
        assert_eq!(handed.iter().filter(|v| **v == 1).count(), 6);
    }

    /// A lone spare pixel belongs near the middle. Zeroing the accumulator
    /// put it on the last gap of every line, which is front-loading pointed
    /// the other way.
    #[test]
    fn justified_slack_centres_a_single_spare_pixel() {
        let mut slack = GapSlack::new(21, 10);
        let handed: heapless::Vec<i16, 10> = (0..10).map(|_| slack.next()).collect();
        assert_eq!(
            handed.as_slice(),
            [0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
            "{handed:?}"
        );
    }

    #[test]
    fn justified_slack_spaces_two_spare_pixels_evenly() {
        let mut slack = GapSlack::new(22, 10);
        let handed: heapless::Vec<i16, 10> = (0..10).map(|_| slack.next()).collect();
        assert_eq!(
            handed.as_slice(),
            [0, 0, 1, 0, 0, 0, 0, 1, 0, 0],
            "{handed:?}"
        );
    }

    /// A run of spaces is one inter-word gap. The count and the draw loop
    /// share `closes_word_gap` so they cannot disagree about how many gaps
    /// the slack is divided between.
    #[test]
    fn justified_gaps_are_counted_once_per_whitespace_run() {
        let line = b"a  b  c";
        let closes: heapless::Vec<usize, 8> = (0..line.len())
            .filter(|index| closes_word_gap(line, *index))
            .collect();
        assert_eq!(closes.as_slice(), [2, 5], "{closes:?}");
    }

    /// Every pixel of the slack is spent, and spent once, however the spaces
    /// are clumped: `extra_per_gap` per gap plus `remainder` single pixels.
    #[test]
    fn justified_slack_totals_the_extra_over_clumped_spaces() {
        for line in [&b"a b c d"[..], b"a  b c  d", b"a   b   c   d"] {
            let gap_count = (0..line.len())
                .filter(|index| closes_word_gap(line, *index))
                .count();
            assert_eq!(gap_count, 3, "line {line:?}");
            let extra = 26i16;
            let mut slack = GapSlack::new(extra, gap_count);
            let spent: i16 = (0..line.len())
                .filter(|index| closes_word_gap(line, *index))
                .map(|_| extra / gap_count as i16 + slack.next())
                .sum();
            assert_eq!(spent, extra, "line {line:?}");
        }
    }

    #[test]
    fn justified_slack_that_divides_evenly_hands_out_nothing() {
        let mut slack = GapSlack::new(20, 10);
        assert_eq!((0..10).map(|_| slack.next()).sum::<i16>(), 0);
    }

    #[test]
    fn justified_slack_survives_a_line_with_no_gaps() {
        // `draw_justified_line` returns early here, but the type must not
        // divide by zero if that guard ever moves.
        let mut slack = GapSlack::new(7, 0);
        assert_eq!(slack.next(), 0);
    }

    use display::font::{style_marker_code, FontFamily, FontWeight};

    /// Minimal blocks for exercising the indent predicate: roles, aligns, and
    /// paragraph-end flags, with the default `paragraph_start` derivation.
    struct IndentBlocks<'a> {
        roles: &'a [TextRole],
        aligns: &'a [TextAlign],
        ends: &'a [bool],
    }

    impl ReadingBlocks for IndentBlocks<'_> {
        fn block_count(&self) -> usize {
            self.roles.len()
        }
        fn block(&self, index: usize) -> Option<BlockRecord> {
            Some(BlockRecord {
                text_offset: 0,
                text_len: 0,
                line_count: 1,
                role: *self.roles.get(index)?,
                style: proto::text::FontStyle::Regular,
                align: self.aligns[index],
            })
        }
        fn block_text(&self, _index: usize) -> &str {
            ""
        }
        fn block_style(&self, _index: usize) -> FontStyle {
            FontStyle::Regular
        }
        fn page_break_before(&self, _index: usize) -> bool {
            false
        }
        fn paragraph_end(&self, index: usize) -> bool {
            self.ends.get(index).copied().unwrap_or(true)
        }
    }

    #[test]
    fn portrait_sheet_overlays_the_footer_instead_of_reserving_space() {
        let frame_height = FbFrame::Portrait.height() as i16;
        let sheet_top = frame_height - PORTRAIT_READING_SHEET_HEIGHT;
        // The page counter baseline (frame_height - 3, drawn by
        // `draw_reading_page_counter_aligned`) falls inside the summoned
        // band: the sheet covers the footer while up rather than the page
        // keeping its furniture above a permanently reserved zone.
        assert!(frame_height - 3 > sheet_top);
        // The page box runs under the band too — body text keeps the
        // full-height page when the sheet is down.
        assert!(PageBox::PORTRAIT.bottom > sheet_top);
    }

    #[test]
    fn only_body_paragraph_openings_take_the_first_line_indent() {
        let blocks = IndentBlocks {
            //                0               1               2
            roles: &[
                TextRole::Body,
                TextRole::Body,
                TextRole::Body,
                TextRole::Heading1,
                TextRole::Body,
                TextRole::BlockQuote,
            ],
            aligns: &[
                TextAlign::Justify,
                TextAlign::Justify,
                TextAlign::Left,
                TextAlign::Center,
                TextAlign::Center,
                TextAlign::Left,
            ],
            // block 0 opens (index 0); block 1 continues it (0 did not end a
            // paragraph); block 2 opens (1 ended one); the rest each open.
            ends: &[false, true, true, true, true, true],
        };
        let indent = paragraph_indent(FontSize::Medium);
        assert_eq!(block_first_line_indent(&blocks, 0), indent, "opening line");
        assert_eq!(block_first_line_indent(&blocks, 1), 0, "continuation line");
        assert_eq!(block_first_line_indent(&blocks, 2), indent, "next opening");
        assert_eq!(block_first_line_indent(&blocks, 3), 0, "heading");
        assert_eq!(block_first_line_indent(&blocks, 4), 0, "centered body");
        assert_eq!(block_first_line_indent(&blocks, 5), 0, "blockquote");
    }

    #[test]
    fn first_line_indent_never_reduces_the_wrapped_line_count() {
        let font = body_font(TypeSettings::DEFAULT, FontStyle::Regular);
        let indent = paragraph_indent(FontSize::Medium);
        let max_width = READER_RIGHT_X - READER_LEFT_X;
        for sample in SAMPLES {
            let flush = wrapped_line_count(font, sample, max_width, 0);
            let indented = wrapped_line_count(font, sample, max_width, indent);
            assert!(
                indented >= flush,
                "indent must not drop lines: {sample:?} {flush} -> {indented}"
            );
        }
    }

    /// Reference implementation: measure the whole string from scratch,
    /// exactly as the pre-incremental firmware code did.
    fn naive_text_ink_width(font: &'static BitmapFont, text: &str) -> i16 {
        let mut advance_fp = 0i32;
        let mut right = 0i16;
        let mut previous = None;
        for ch in text.chars() {
            let codepoint = if ch as u32 > u16::MAX as u32 {
                b'?' as u16
            } else {
                ch as u16
            };
            let (drawn, metric) = if let Some((metric, _)) = font.glyph(codepoint) {
                (codepoint, metric)
            } else if let Some((metric, _)) = font.glyph(b'?' as u16) {
                (b'?' as u16, metric)
            } else {
                advance_fp += 8 << 4;
                right = right.max(fixed_ceil(advance_fp));
                continue;
            };
            if let Some(left) = previous {
                advance_fp += font.kerning_adjust_fp(left, drawn) as i32;
            }
            let advance = fixed_round(advance_fp);
            let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
            right = right.max(glyph_right);
            advance_fp += metric.advance_fp as i32;
            previous = Some(drawn);
        }
        right.max(fixed_ceil(advance_fp))
    }

    /// Reference wrap: re-measures every candidate line per word, exactly
    /// as the pre-incremental firmware code did.
    fn naive_next_wrapped_line(
        text: &str,
        mut cursor: usize,
        font: &'static BitmapFont,
        x: i16,
        max_x: i16,
    ) -> Option<(usize, usize, usize)> {
        let bytes = text.as_bytes();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }

        let mut scan = cursor;
        let mut best_end = cursor;
        let mut best_next = cursor;
        while scan < bytes.len() {
            let word_start = scan;
            while scan < bytes.len() && !bytes[scan].is_ascii_whitespace() {
                scan += 1;
            }
            let word_end = scan;
            let candidate = &text[cursor..word_end];
            if x + naive_text_ink_width(font, candidate) + READER_WRAP_SAFETY > max_x {
                if best_end == cursor {
                    return Some((word_start, word_end, word_end));
                }
                return Some((cursor, best_end, best_next));
            }
            best_end = word_end;
            while scan < bytes.len() && bytes[scan].is_ascii_whitespace() {
                scan += 1;
            }
            best_next = scan;
        }

        Some((cursor, best_end, best_next.max(best_end)))
    }

    const SAMPLES: &[&str] = &[
        "",
        " ",
        "word",
        "two words",
        "It was the best of times, it was the worst of times, it was the age of wisdom.",
        "  leading and   irregular   spacing between words  ",
        "Supercalifragilisticexpialidocious-antidisestablishmentarianism-longword",
        "short a b c d e f g h i j k l m n o p q r s t u v w x y z",
        "punctuation, everywhere! (parentheses) \"quotes\" -- dashes; colons: done.",
        "tabs\tand\nnewlines\ras whitespace",
        "non-latin \u{4e16}\u{754c} mixed with latin text and \u{20ac} symbols",
        "beyond bmp \u{1F600} falls back to question mark",
    ];

    const STYLES: [FontStyle; 4] = [
        FontStyle::Regular,
        FontStyle::Italic,
        FontStyle::Bold,
        FontStyle::BoldItalic,
    ];

    const ALL_SETTINGS: [TypeSettings; 54] = {
        let sizes = [FontSize::Small, FontSize::Medium, FontSize::Large];
        let spacings = [
            LineSpacing::Compact,
            LineSpacing::Normal,
            LineSpacing::Relaxed,
        ];
        let weights = [FontWeight::Normal, FontWeight::Heavy];
        let families = [
            FontFamily::Literata,
            FontFamily::Merriweather,
            FontFamily::Custom,
        ];
        let mut out = [TypeSettings::DEFAULT; 54];
        let mut i = 0;
        while i < 3 {
            let mut j = 0;
            while j < 3 {
                let mut k = 0;
                while k < 2 {
                    let mut l = 0;
                    while l < 3 {
                        out[((i * 3 + j) * 2 + k) * 3 + l] = TypeSettings {
                            size: sizes[i],
                            spacing: spacings[j],
                            weight: weights[k],
                            family: families[l],
                        };
                        l += 1;
                    }
                    k += 1;
                }
                j += 1;
            }
            i += 1;
        }
        out
    };

    fn fonts() -> [&'static BitmapFont; 4] {
        STYLES.map(|style| body_font(TypeSettings::DEFAULT, style))
    }

    #[test]
    fn ink_width_matches_naive_reference() {
        for font in fonts() {
            for sample in SAMPLES {
                assert_eq!(
                    text_ink_width(font, sample),
                    naive_text_ink_width(font, sample),
                    "sample {sample:?}"
                );
            }
        }
    }

    #[test]
    fn next_wrapped_line_matches_naive_reference() {
        for font in fonts() {
            for sample in SAMPLES {
                for max_x in [40i16, 120, 300, 784] {
                    for x in [0i16, 8, 32] {
                        let mut cursor = 0usize;
                        loop {
                            let fast = next_wrapped_line(sample, cursor, font, x, max_x);
                            let slow = naive_next_wrapped_line(sample, cursor, font, x, max_x);
                            assert_eq!(fast, slow, "sample {sample:?} x {x} max_x {max_x}");
                            let Some((_, _, next_cursor)) = fast else {
                                break;
                            };
                            assert!(next_cursor > cursor, "wrap must make progress");
                            cursor = next_cursor;
                            if cursor >= sample.len() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn styled_width_matches_unstyled_when_unmarked() {
        for settings in ALL_SETTINGS {
            for style in STYLES {
                for sample in SAMPLES {
                    assert_eq!(
                        styled_text_ink_width(sample, settings, style),
                        naive_text_ink_width(body_font(settings, style), sample),
                        "sample {sample:?} settings {settings:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn layout_configs_are_distinct_per_type_settings_and_page_box() {
        let mut seen = [0u16; 2 * ALL_SETTINGS.len()];
        let mut index = 0;
        for portrait in [false, true] {
            for settings in ALL_SETTINGS.iter() {
                let config = reader_layout_config(*settings, portrait);
                assert!(
                    !seen[..index].contains(&config),
                    "duplicate layout config {config} for {settings:?} portrait={portrait}"
                );
                seen[index] = config;
                index += 1;
            }
        }
    }

    #[test]
    fn line_advances_grow_with_size_and_spacing() {
        let settings = [
            TypeSettings {
                size: FontSize::Small,
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Small,
                spacing: LineSpacing::Normal,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Small,
                spacing: LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Medium,
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Medium,
                spacing: LineSpacing::Normal,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Medium,
                spacing: LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Large,
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Large,
                spacing: LineSpacing::Normal,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Large,
                spacing: LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
        ];
        for role in [TextRole::Body, TextRole::Heading1] {
            for window in settings.windows(2) {
                assert!(
                    line_advance(window[0], role) < line_advance(window[1], role)
                        || window[0].size != window[1].size,
                    "advance must grow with spacing within a size: {window:?}"
                );
            }
        }
        assert_eq!(line_advance(TypeSettings::DEFAULT, TextRole::Body), 26);
        assert_eq!(line_advance(TypeSettings::DEFAULT, TextRole::Heading1), 31);
    }

    #[test]
    fn styled_cursor_checkpoint_equals_full_measure() {
        let mut styled = heapless::String::<256>::new();
        let _ = styled.push_str("plain ");
        let _ = styled.push(STYLE_MARKER);
        let _ = styled.push(style_marker_code(FontStyle::Italic));
        let _ = styled.push_str("slanted words ");
        let _ = styled.push(STYLE_MARKER);
        let _ = styled.push(style_marker_code(FontStyle::Bold));
        let _ = styled.push_str("heavy end");

        // Push in arbitrary fragments; the running width must match a
        // one-shot measure at every fragment boundary, at every size.
        for settings in ALL_SETTINGS {
            let text = styled.as_str();
            let mut cursor = StyledInkCursor::new(settings, FontStyle::Regular);
            let mut consumed = 0usize;
            for chunk in [3usize, 1, 9, 2, 30, 200] {
                let end = (consumed + chunk).min(text.len());
                while !text.is_char_boundary(consumed.min(text.len())) {
                    consumed += 1;
                }
                let mut safe_end = end;
                while safe_end < text.len() && !text.is_char_boundary(safe_end) {
                    safe_end += 1;
                }
                cursor.push_str(&text[consumed..safe_end]);
                consumed = safe_end;
                assert_eq!(
                    cursor.width(),
                    styled_text_ink_width(&text[..consumed], settings, FontStyle::Regular),
                    "prefix {:?} settings {settings:?}",
                    &text[..consumed]
                );
                if consumed >= text.len() {
                    break;
                }
            }
        }
    }

    /// Growable fixture for the incremental page-index cursor: single-line
    /// blocks (the shape the streaming cache build produces) with per-block
    /// role, paragraph-end, and page-break flags, exposing only the first
    /// `len` blocks so one allocation serves an append-one-at-a-time walk.
    struct PageBlocks {
        roles: Vec<TextRole>,
        ends: Vec<bool>,
        breaks: Vec<bool>,
        settings: TypeSettings,
        page_box: PageBox,
        len: usize,
    }

    impl ReadingBlocks for PageBlocks {
        fn block_count(&self) -> usize {
            self.len
        }
        fn block(&self, index: usize) -> Option<BlockRecord> {
            (index < self.len).then(|| BlockRecord {
                text_offset: 0,
                text_len: 0,
                line_count: 1,
                role: self.roles[index],
                style: proto::text::FontStyle::Regular,
                align: TextAlign::Justify,
            })
        }
        fn block_text(&self, _index: usize) -> &str {
            ""
        }
        fn block_style(&self, _index: usize) -> FontStyle {
            FontStyle::Regular
        }
        fn page_break_before(&self, index: usize) -> bool {
            self.breaks[index]
        }
        fn paragraph_end(&self, index: usize) -> bool {
            self.ends[index]
        }
        fn type_settings(&self) -> TypeSettings {
            self.settings
        }
        fn page_box(&self) -> PageBox {
            self.page_box
        }
    }

    /// Reference implementation: the pre-incremental firmware
    /// `rebuild_page_index` walk, verbatim — full gapped height against the
    /// page edge, completed pages pushed at each boundary plus the trailing
    /// page, records silently dropped past `capacity`.
    fn naive_page_index(
        source: &impl ReadingBlocks,
        capacity: usize,
    ) -> (Vec<PageRecord>, Vec<u16>, usize) {
        let mut pages = vec![
            PageRecord {
                first_block: 0,
                block_count: 0
            };
            capacity
        ];
        let mut page_spine = vec![0u16; capacity];
        let mut page_count = 0usize;
        let push = |pages: &mut Vec<PageRecord>,
                    page_spine: &mut Vec<u16>,
                    page_count: &mut usize,
                    first_block: usize,
                    block_count: usize| {
            if block_count == 0 || *page_count >= capacity {
                return;
            }
            pages[*page_count] = PageRecord {
                first_block: first_block as u16,
                block_count: block_count as u16,
            };
            page_spine[*page_count] = spine_for(first_block);
            *page_count += 1;
        };
        if source.block_count() == 0 {
            return (pages, page_spine, 0);
        }
        let PageBox {
            top: page_top,
            bottom: page_bottom,
            ..
        } = source.page_box();
        let mut first_block = 0usize;
        let mut block_count = 0usize;
        let mut y = page_top;
        for index in 0..source.block_count() {
            let height = block_height(source, index);
            let new_page =
                (y + height > page_bottom || source.page_break_before(index)) && y > page_top;
            if new_page {
                push(
                    &mut pages,
                    &mut page_spine,
                    &mut page_count,
                    first_block,
                    block_count,
                );
                first_block = index;
                block_count = 0;
                y = page_top;
            }
            block_count += 1;
            y += height;
        }
        push(
            &mut pages,
            &mut page_spine,
            &mut page_count,
            first_block,
            block_count,
        );
        (pages, page_spine, page_count)
    }

    /// Deterministic per-block spine, so spine propagation into the page
    /// records is checked too.
    fn spine_for(index: usize) -> u16 {
        (index / 5) as u16
    }

    /// The incremental side under test: the shared cursor plus the shared
    /// array-application helpers, driven exactly as the firmware's
    /// `LibraryBlockSink` drives them.
    struct IncrementalIndex {
        cursor: PageIndexCursor,
        pages: Vec<PageRecord>,
        page_spine: Vec<u16>,
        page_count: usize,
        overflowed: bool,
    }

    impl IncrementalIndex {
        fn new(page_box: PageBox, capacity: usize) -> Self {
            Self {
                cursor: PageIndexCursor::start(page_box),
                pages: vec![
                    PageRecord {
                        first_block: 0,
                        block_count: 0
                    };
                    capacity
                ],
                page_spine: vec![0u16; capacity],
                page_count: 0,
                overflowed: false,
            }
        }

        fn append(&mut self, source: &impl ReadingBlocks, index: usize) {
            let placement = self.cursor.place_next_block(source, index);
            apply_block_placement(
                placement,
                index,
                spine_for(index),
                &mut self.pages,
                &mut self.page_spine,
                &mut self.page_count,
                &mut self.overflowed,
            );
        }

        fn mark_last_grew(&mut self, source: &impl ReadingBlocks, index: usize) {
            if self.cursor.replace_last_block(source, index) == BlockPlacement::NewPage {
                apply_last_block_move(
                    index,
                    spine_for(index),
                    &mut self.pages,
                    &mut self.page_spine,
                    &mut self.page_count,
                    &mut self.overflowed,
                );
            }
        }

        /// The firmware's carry-path fallback: full rebuild, adopting the
        /// walk's cursor for the appends that follow.
        fn rebuild(&mut self, source: &impl ReadingBlocks) {
            self.cursor = PageIndexCursor::start(source.page_box());
            self.page_count = 0;
            self.overflowed = false;
            for index in 0..source.block_count() {
                self.append(source, index);
            }
        }

        fn assert_matches(&self, source: &impl ReadingBlocks, context: &str) {
            let (pages, page_spine, page_count) = naive_page_index(source, self.pages.len());
            assert_eq!(self.page_count, page_count, "page_count: {context}");
            assert_eq!(
                &self.pages[..page_count],
                &pages[..page_count],
                "page records: {context}"
            );
            assert_eq!(
                &self.page_spine[..page_count],
                &page_spine[..page_count],
                "page spines: {context}"
            );
        }
    }

    /// Deterministic pseudo-random block sequences: varied roles (varied
    /// heights and paragraph gaps), paragraph ends, and page breaks.
    fn synth_blocks(
        seed: u32,
        count: usize,
        settings: TypeSettings,
        page_box: PageBox,
    ) -> PageBlocks {
        let mut state = seed;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state >> 16
        };
        let roles = [
            TextRole::Body,
            TextRole::Body,
            TextRole::Body,
            TextRole::BlockQuote,
            TextRole::Heading3,
            TextRole::Heading2,
            TextRole::Heading1,
        ];
        let mut fixture = PageBlocks {
            roles: Vec::new(),
            ends: Vec::new(),
            breaks: Vec::new(),
            settings,
            page_box,
            len: 0,
        };
        for _ in 0..count {
            fixture.roles.push(roles[next() as usize % roles.len()]);
            fixture.ends.push(next() % 3 != 0);
            fixture.breaks.push(next() % 11 == 0);
            fixture.len += 1;
        }
        fixture
    }

    #[test]
    fn incremental_page_cursor_matches_full_rebuild_at_every_append() {
        let boxes = [
            PageBox::LANDSCAPE,
            PageBox::PORTRAIT,
            // A short page forces frequent page turns and, with the small
            // capacities below, exercises the past-capacity drop path.
            PageBox {
                left: 8,
                right: 200,
                top: 6,
                bottom: 96,
            },
        ];
        let settings = [
            TypeSettings::DEFAULT,
            TypeSettings {
                size: FontSize::Small,
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Large,
                spacing: LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
        ];
        let mut overflow_hit = false;
        for page_box in boxes {
            for settings in settings {
                for (seed, capacity) in [(1u32, 96usize), (2, 96), (3, 4), (4, 2)] {
                    let mut fixture = synth_blocks(seed, 80, settings, page_box);
                    let total = fixture.len;
                    fixture.len = 0;
                    let mut incremental = IncrementalIndex::new(page_box, capacity);
                    for index in 0..total {
                        fixture.len = index + 1;
                        incremental.append(&fixture, index);
                        incremental.assert_matches(
                            &fixture,
                            &format!("seed {seed} capacity {capacity} block {index}"),
                        );
                    }
                    overflow_hit |= incremental.overflowed;
                }
            }
        }
        assert!(
            overflow_hit,
            "the grid must exercise the capacity-drop path"
        );
    }

    #[test]
    fn retroactive_paragraph_end_marks_match_full_rebuild() {
        // Mirror the build's mark_last_block_paragraph_end: blocks arrive
        // with paragraph_end=false, then an empty paragraph-end fragment
        // flips the flag on the last block only — a height-only change that
        // the cursor must absorb as a bounded one-block fix-up.
        let mut moved_hit = false;
        for page_box in [PageBox::LANDSCAPE, PageBox::PORTRAIT] {
            for seed in 5u32..9 {
                let mut fixture = synth_blocks(seed, 60, TypeSettings::DEFAULT, page_box);
                // Roles with a real trailing gap make the flip change height.
                let total = fixture.len;
                for flag in fixture.ends.iter_mut() {
                    *flag = false;
                }
                fixture.len = 0;
                let mut incremental = IncrementalIndex::new(page_box, 96);
                let mut state = seed;
                for index in 0..total {
                    fixture.len = index + 1;
                    incremental.append(&fixture, index);
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    if (state >> 16) % 2 == 0 {
                        let before = incremental.page_count;
                        fixture.ends[index] = true;
                        incremental.mark_last_grew(&fixture, index);
                        moved_hit |= incremental.page_count > before;
                    }
                    incremental.assert_matches(&fixture, &format!("seed {seed} block {index}"));
                }
            }
        }
        assert!(
            moved_hit,
            "the sweep must include a mark that moves the block to a new page"
        );
    }

    #[test]
    fn paragraph_end_growth_moves_an_exactly_full_block_to_the_next_page() {
        // Constructed move: Medium/Normal body advance is 26; a page box of
        // 6..58 fits exactly two lines ungapped. Marking the second block
        // (BlockQuote, trailing gap 6) paragraph-end grows it past the edge,
        // so a full rebuild moves it to a new page — the cursor must agree.
        let advance = line_advance(TypeSettings::DEFAULT, TextRole::Body);
        let page_box = PageBox {
            left: 8,
            right: 200,
            top: 6,
            bottom: 6 + 2 * advance,
        };
        let mut fixture = PageBlocks {
            roles: vec![TextRole::Body, TextRole::BlockQuote],
            ends: vec![true, false],
            breaks: vec![false, false],
            settings: TypeSettings::DEFAULT,
            page_box,
            len: 0,
        };
        let mut incremental = IncrementalIndex::new(page_box, 96);
        fixture.len = 1;
        incremental.append(&fixture, 0);
        fixture.len = 2;
        incremental.append(&fixture, 1);
        assert_eq!(incremental.page_count, 1, "both blocks share the page");

        fixture.ends[1] = true;
        incremental.mark_last_grew(&fixture, 1);
        assert_eq!(incremental.page_count, 2, "the grown block moved");
        incremental.assert_matches(&fixture, "constructed move");
    }

    #[test]
    fn cursor_adopted_from_a_carry_rebuild_stays_in_agreement() {
        // The carry path drops the flushed whole pages, rebases the
        // half-finished page's blocks to the front, and full-rebuilds; the
        // appends that follow continue incrementally from the walk's cursor.
        for seed in 10u32..14 {
            let full = synth_blocks(seed, 70, TypeSettings::DEFAULT, PageBox::LANDSCAPE);
            let (pages, _, page_count) = naive_page_index(&full, 96);
            if page_count < 2 {
                continue;
            }
            // Simulate carrying the last (half-finished) page: keep only its
            // blocks, as carry_last_page rebases them to index 0.
            let cut = pages[page_count - 1].first_block as usize;
            let mut carried = PageBlocks {
                roles: full.roles[cut..].to_vec(),
                ends: full.ends[cut..].to_vec(),
                breaks: full.breaks[cut..].to_vec(),
                settings: full.settings,
                page_box: full.page_box,
                len: full.len - cut,
            };
            let mut incremental = IncrementalIndex::new(carried.page_box, 96);
            incremental.rebuild(&carried);
            incremental.assert_matches(&carried, &format!("seed {seed} post-carry rebuild"));

            // Keep appending after the rebuild.
            let more = synth_blocks(seed ^ 0xa5a5, 30, full.settings, full.page_box);
            for index in 0..more.len {
                carried.roles.push(more.roles[index]);
                carried.ends.push(more.ends[index]);
                carried.breaks.push(more.breaks[index]);
                carried.len += 1;
                incremental.append(&carried, carried.len - 1);
                incremental.assert_matches(
                    &carried,
                    &format!("seed {seed} appended block {index} after carry"),
                );
            }
        }
    }
}
