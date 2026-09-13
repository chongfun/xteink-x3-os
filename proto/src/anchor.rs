//! The coordinate a reading position is stored as.
//!
//! A page number means something only to the layout that produced it, so it
//! cannot be durable state. What is durable is a place in the book's content,
//! and this is the coordinate for one: a spine item, and how far into that
//! item's logical content stream the place sits.
//!
//! # The logical content stream
//!
//! The stream is a coordinate space, not a buffer. Nothing materializes it and
//! no file holds it. It is defined by what the XHTML block parser emits for
//! one spine item:
//!
//! - blocks count in the order the parser emits them, which is document order;
//! - a block occupies `text.len() + 1` bytes of the space, the extra byte
//!   standing for the boundary after it;
//! - a block's offset is the sum of the sizes of every block before it in the
//!   same spine item, so the first block of an item sits at 0.
//!
//! The extra byte per block is what lets content with no text hold a place of
//! its own. An image block emits no characters, and without it two images in a
//! row would share one offset and a reader could not be returned to the
//! second.
//!
//! # What the stream does not depend on
//!
//! The parser takes XHTML and CSS and nothing else. No font, no viewport, no
//! margin and no line spacing reaches it, so the same bytes always produce the
//! same stream. That is the point: a place in it survives every layout change,
//! which a page number does not.
//!
//! # Versioning
//!
//! The rules above are persistence ABI, so [`CONTENT_STREAM_VERSION`] covers
//! them. Change what the parser emits, how it normalizes text, or how a
//! block's size is counted, and every stored anchor moves. The version is what
//! a reader checks before believing one.

/// The rules that define the logical content stream an offset indexes.
///
/// Bump this when the parser's emitted blocks, its text normalization, or the
/// size a block occupies changes. A reader that finds a version it does not
/// know cannot interpret the offset, and falls back under the position
/// format's own rules rather than guessing.
pub const CONTENT_STREAM_VERSION: u8 = 1;

/// A place in a book, independent of how the book is laid out.
///
/// Ordered the way a reader moves through one: by spine item, then by offset
/// within it. The derived `Ord` gives exactly that, which turns "the last page
/// starting at or before this anchor" into a comparison rather than a search
/// through content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentAnchor {
    /// Index of the spine item, in the order the OPF lists it.
    pub spine: u16,
    /// Bytes into that item's logical content stream.
    pub offset: u32,
}

/// Bytes one anchor occupies on the card.
pub const CONTENT_ANCHOR_BYTES: usize = 6;

impl ContentAnchor {
    /// The beginning of the book: the first spine item, before its first
    /// block. Where a book with no stored position opens.
    pub const START: Self = Self {
        spine: 0,
        offset: 0,
    };

    /// The anchor for a place, named rather than built positionally, since
    /// `(spine, offset)` and `(offset, spine)` are both plausible readings of
    /// a bare two-number constructor.
    pub const fn at(spine: u16, offset: u32) -> Self {
        Self { spine, offset }
    }

    /// Whether this anchor falls in `[start, end)`.
    ///
    /// Half-open, and the convention the whole feature keeps: a page owns its
    /// start and not its end, so one layout's page boundaries tile the book
    /// with no place belonging to two pages and none belonging to none.
    pub fn is_within(self, start: Self, end: Self) -> bool {
        self >= start && self < end
    }

    /// Little-endian, spine then offset. Fixed width, so an array of anchors
    /// indexes without a scan.
    pub fn encode(self, out: &mut [u8; CONTENT_ANCHOR_BYTES]) {
        out[0..2].copy_from_slice(&self.spine.to_le_bytes());
        out[2..6].copy_from_slice(&self.offset.to_le_bytes());
    }

    /// The inverse of [`encode`](Self::encode). Total: every six-byte pattern
    /// is a legal anchor, so a torn read yields a wrong place rather than a
    /// parse failure, and the caller's own integrity check catches it.
    pub fn decode(input: &[u8; CONTENT_ANCHOR_BYTES]) -> Self {
        Self {
            spine: u16::from_le_bytes([input[0], input[1]]),
            offset: u32::from_le_bytes([input[2], input[3], input[4], input[5]]),
        }
    }
}

/// How much of the logical stream a block holding this much text occupies.
///
/// The one place the rule lives, so the parser that assigns offsets and any
/// reader that walks them cannot disagree about it.
pub const fn block_stream_len(text_len: usize) -> u32 {
    // Saturating rather than wrapping: a block longer than u32 cannot exist,
    // since the parser's own block buffer is 384 bytes, and a silent wrap
    // would put a later place before an earlier one.
    (text_len as u32).saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_order_by_spine_then_offset() {
        let first = ContentAnchor::at(0, 100);
        let later_in_item = ContentAnchor::at(0, 200);
        let next_item = ContentAnchor::at(1, 0);
        assert!(first < later_in_item);
        assert!(later_in_item < next_item);
        assert!(ContentAnchor::START < first);
    }

    #[test]
    fn a_round_trip_keeps_the_place() {
        for anchor in [
            ContentAnchor::START,
            ContentAnchor::at(1, 0),
            ContentAnchor::at(0, u32::MAX),
            ContentAnchor::at(u16::MAX, u32::MAX),
            ContentAnchor::at(7, 4_099),
        ] {
            let mut bytes = [0u8; CONTENT_ANCHOR_BYTES];
            anchor.encode(&mut bytes);
            assert_eq!(ContentAnchor::decode(&bytes), anchor);
        }
    }

    #[test]
    fn a_page_owns_its_start_and_not_its_end() {
        let start = ContentAnchor::at(2, 40);
        let end = ContentAnchor::at(2, 90);
        assert!(start.is_within(start, end), "the start is inside");
        assert!(ContentAnchor::at(2, 89).is_within(start, end));
        assert!(
            !end.is_within(start, end),
            "the end belongs to the next page"
        );
        assert!(!ContentAnchor::at(2, 39).is_within(start, end));
        assert!(
            !ContentAnchor::at(3, 0).is_within(start, end),
            "and a later spine item is outside whatever its offset"
        );
    }

    #[test]
    fn content_with_no_text_still_takes_a_place_of_its_own() {
        // Two images in a row: no characters between them, and the reader
        // still has to be able to come back to the second rather than the
        // first.
        let first = 0;
        let second = first + block_stream_len(0);
        assert_ne!(first, second);
        assert_eq!(block_stream_len(0), 1);
        assert_eq!(block_stream_len(383), 384);
    }
}
