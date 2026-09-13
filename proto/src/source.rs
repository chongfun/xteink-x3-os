//! Identity for the bytes of an EPUB, independent of where the file sits.
//!
//! A [`SourceDigest`] answers "which bytes is this", and nothing else. It is
//! deliberately not the identity of a library entry: two byte-identical files
//! share a digest while remaining separate books with separate reading
//! positions, so user state hangs from a library identity rather than from
//! this. What belongs here is derived and rebuildable, such as a parsed
//! structure, a cover, or decoded images, which identical copies may share.
//!
//! The digest covers the whole stream rather than a sample, since the
//! question it answers is decided by the bytes themselves. It is streaming so
//! the upload path can hash what it is already receiving. The length rides
//! alongside the hash because a disagreeing length is an easier diagnosis
//! than a hash that quietly does not match.

use sha2::{Digest, Sha256};

/// Bytes in a SHA-256 digest.
pub const SHA256_BYTES: usize = 32;

/// The authoritative identity of an EPUB's bytes.
///
/// Equality covers both fields, so a difference in either denotes a different
/// digest. Whether a digest still describes the file it was stored beside is
/// a separate question, and the answer to that one goes stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceDigest {
    byte_len: u64,
    sha256: [u8; SHA256_BYTES],
}

impl SourceDigest {
    /// Rebuild a digest from parts.
    ///
    /// Crate-private on purpose: every public way to obtain one reads bytes,
    /// so stored fields cannot be assembled into a fact from outside. Inside
    /// the crate a claim's bytes are reassembled here and then wrapped in
    /// [`CachedSourceDigest`], which is the shape that stays evidence.
    pub(crate) const fn from_parts(byte_len: u64, sha256: [u8; SHA256_BYTES]) -> Self {
        Self { byte_len, sha256 }
    }

    /// Length of the stream this digest was taken over.
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }

    /// The raw SHA-256.
    pub const fn sha256(&self) -> &[u8; SHA256_BYTES] {
        &self.sha256
    }
}

/// Streaming hasher: feed the bytes as they arrive, then [`SourceHasher::finish`].
///
/// Chunking is the caller's business. A digest depends on the byte stream and
/// not on how it was cut up, so a socket's arbitrary read sizes and a file
/// reader's fixed buffer produce the same answer.
pub struct SourceHasher {
    sha: Sha256,
    byte_len: u64,
}

impl SourceHasher {
    pub fn new() -> Self {
        Self {
            sha: Sha256::new(),
            byte_len: 0,
        }
    }

    /// Add the next bytes of the stream.
    pub fn update(&mut self, bytes: &[u8]) {
        self.sha.update(bytes);
        // Saturating rather than wrapping: a length that stopped counting is
        // wrong, and a length that wrapped to a small number looks right.
        self.byte_len = self.byte_len.saturating_add(bytes.len() as u64);
    }

    /// Finish the stream and take its identity.
    pub fn finish(self) -> SourceDigest {
        SourceDigest {
            byte_len: self.byte_len,
            sha256: self.sha.finalize().into(),
        }
    }
}

impl Default for SourceHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// The digest of a slice already in memory.
pub fn digest_of(bytes: &[u8]) -> SourceDigest {
    let mut hasher = SourceHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// A digest read back off the card, rather than computed from the bytes now
/// on it.
///
/// A computer can replace a file with a same-sized edition, or delete one so
/// its alias goes to another book. The record survives both intact, describing
/// a book that is gone, so it stays in its own type. Narrowing candidates is a
/// fine use. Claiming two files hold the same bytes is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CachedSourceDigest(SourceDigest);

impl CachedSourceDigest {
    pub const fn new(digest: SourceDigest) -> Self {
        Self(digest)
    }

    /// The length when this was recorded. Cheap to compare, worth nothing as
    /// proof: a same-sized replacement matches it.
    pub const fn byte_len(&self) -> u64 {
        self.0.byte_len()
    }

    /// The recorded hash, for narrowing candidates before paying for a read.
    pub const fn sha256(&self) -> &[u8; SHA256_BYTES] {
        self.0.sha256()
    }

    /// Whether this still describes the file, given a digest just computed
    /// from it.
    ///
    /// The only way past the boundary, and it costs a read. There is no
    /// accessor handing back a [`SourceDigest`], because holding one entitles
    /// a caller to claim two files match.
    pub fn agrees_with(&self, current: &SourceDigest) -> bool {
        self.0 == *current
    }
}

/// On-card record for a digest that has already been computed.
///
/// ```text
/// magic[4] | version u8 | byte_len u64 | sha256[32] | checksum u32
/// ```
///
/// Little-endian, FNV-1a over everything before the checksum. That checksum
/// turns a torn write into a miss rather than a lie: a record failing it is
/// discarded and the digest recomputed, costing a read.
pub const SOURCE_RECORD_MAGIC: [u8; 4] = *b"CSRC";
/// Bumped when the layout below changes. An unknown version reads as absent,
/// so an older build's record is recomputed rather than misread.
pub const SOURCE_RECORD_VERSION: u8 = 1;
/// Bytes in an encoded record.
pub const SOURCE_RECORD_BYTES: usize = 49;

const OFF_VERSION: usize = 4;
const OFF_BYTE_LEN: usize = 5;
const OFF_SHA256: usize = 13;
const OFF_CHECKSUM: usize = 45;

fn record_checksum(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in bytes {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    hash
}

/// Encode a digest for storage beside the book it describes.
pub fn encode_record(digest: &SourceDigest) -> [u8; SOURCE_RECORD_BYTES] {
    let mut out = [0u8; SOURCE_RECORD_BYTES];
    out[..4].copy_from_slice(&SOURCE_RECORD_MAGIC);
    out[OFF_VERSION] = SOURCE_RECORD_VERSION;
    out[OFF_BYTE_LEN..OFF_SHA256].copy_from_slice(&digest.byte_len().to_le_bytes());
    out[OFF_SHA256..OFF_CHECKSUM].copy_from_slice(digest.sha256());
    let checksum = record_checksum(&out[..OFF_CHECKSUM]);
    out[OFF_CHECKSUM..].copy_from_slice(&checksum.to_le_bytes());
    out
}

/// Encode a digest that was already read off the card.
///
/// For a caller carrying evidence through memory of its own between two
/// reads, which is what the scan's move search does with the digests of the
/// copies that have gone missing. The boundary holds: what goes in is a
/// record of what the bytes were, what comes back out of [`parse_record`]
/// is the same, and nothing here hands anyone a [`SourceDigest`] to claim
/// two files match with.
pub fn encode_cached_record(digest: &CachedSourceDigest) -> [u8; SOURCE_RECORD_BYTES] {
    encode_record(&SourceDigest::from_parts(
        digest.byte_len(),
        *digest.sha256(),
    ))
}

/// Read a record back, or `None` for anything this build cannot trust.
///
/// Short, foreign, from an unknown version, or failing its checksum all read
/// the same way, because the answer to each is to hash the file again.
pub fn parse_record(bytes: &[u8]) -> Option<CachedSourceDigest> {
    if bytes.len() < SOURCE_RECORD_BYTES || bytes[..4] != SOURCE_RECORD_MAGIC {
        return None;
    }
    if bytes[OFF_VERSION] != SOURCE_RECORD_VERSION {
        return None;
    }
    let stored = u32::from_le_bytes(bytes[OFF_CHECKSUM..SOURCE_RECORD_BYTES].try_into().ok()?);
    if stored != record_checksum(&bytes[..OFF_CHECKSUM]) {
        return None;
    }
    let byte_len = u64::from_le_bytes(bytes[OFF_BYTE_LEN..OFF_SHA256].try_into().ok()?);
    let sha256: [u8; SHA256_BYTES] = bytes[OFF_SHA256..OFF_CHECKSUM].try_into().ok()?;
    Some(CachedSourceDigest::new(SourceDigest::from_parts(
        byte_len, sha256,
    )))
}

/// Which physical file a digest belongs to: the 8.3 alias and the directory
/// it sits in.
///
/// The directory is part of the key rather than context a caller is trusted to
/// remember, since two books can wear the same alias in different places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKey {
    in_books: bool,
    alias: heapless::String<12>,
}

impl FileKey {
    /// The 8.3 alias, which is also how the file is opened. Whoever reads it
    /// takes the name from here, so a name and a key cannot disagree.
    pub fn alias(&self) -> &str {
        self.alias.as_str()
    }

    /// Whether it sits in `/BOOKS` rather than the card root.
    pub const fn in_books(&self) -> bool {
        self.in_books
    }

    /// `None` for a name too long to be an 8.3 alias.
    ///
    /// Fallible because the alternative is worse: `push_str` is all or
    /// nothing, so a long name would silently produce an empty key, and every
    /// long name would produce the same one.
    pub fn new(in_books: bool, alias: &str) -> Option<Self> {
        let mut owned = heapless::String::new();
        owned.push_str(alias).ok()?;
        Some(Self {
            in_books,
            alias: owned,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of the empty string, from the standard test vectors. Pins the
    /// construction to the algorithm rather than to itself.
    const EMPTY_SHA256: [u8; SHA256_BYTES] = [
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9,
        0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52,
        0xb8, 0x55,
    ];

    /// SHA-256 of "abc", the other vector everyone knows.
    const ABC_SHA256: [u8; SHA256_BYTES] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];

    #[test]
    fn empty_input_matches_the_published_vector() {
        let digest = digest_of(&[]);
        assert_eq!(digest.sha256(), &EMPTY_SHA256);
        assert_eq!(digest.byte_len(), 0);
    }

    #[test]
    fn known_vector_matches() {
        assert_eq!(digest_of(b"abc").sha256(), &ABC_SHA256);
    }

    #[test]
    fn identical_bytes_have_one_identity() {
        assert_eq!(digest_of(b"the same book"), digest_of(b"the same book"));
    }

    #[test]
    fn one_byte_apart_is_a_different_identity() {
        assert_ne!(digest_of(b"the same book"), digest_of(b"the same bood"));
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_answer() {
        // The upload path is fed by a socket and the sideload path by a fixed
        // buffer. They must agree.
        let body: [u8; 300] = core::array::from_fn(|i| (i % 251) as u8);
        let whole = digest_of(&body);
        for cut in [1usize, 2, 63, 64, 65, 127, 128, 129, 299] {
            let mut hasher = SourceHasher::new();
            for chunk in body.chunks(cut) {
                hasher.update(chunk);
            }
            assert_eq!(hasher.finish(), whole, "chunked by {cut}");
        }
    }

    #[test]
    fn empty_updates_do_not_disturb_the_stream() {
        let mut hasher = SourceHasher::new();
        hasher.update(b"ab");
        hasher.update(&[]);
        hasher.update(b"c");
        assert_eq!(hasher.finish(), digest_of(b"abc"));
    }

    #[test]
    fn length_counts_the_whole_stream() {
        let mut hasher = SourceHasher::new();
        hasher.update(&[0u8; 100]);
        hasher.update(&[0u8; 56]);
        assert_eq!(hasher.finish().byte_len(), 156);
    }

    #[test]
    fn persisted_parts_round_trip() {
        let digest = digest_of(b"abc");
        let rebuilt = SourceDigest::from_parts(digest.byte_len(), *digest.sha256());
        assert_eq!(rebuilt, digest);
    }

    #[test]
    fn evidence_only_becomes_a_fact_by_agreeing_with_a_fresh_digest() {
        let digest = digest_of(b"abc");
        let evidence = CachedSourceDigest::new(digest);
        assert!(evidence.agrees_with(&digest));
        assert!(!evidence.agrees_with(&digest_of(b"abd")));
        // And the only way back to a SourceDigest is to have hashed one.
        assert_eq!(evidence.sha256(), digest.sha256());
    }

    #[test]
    fn a_key_refuses_a_name_that_cannot_fit_an_alias() {
        assert!(FileKey::new(true, "DUNE~1.EPU").is_some());
        assert!(
            FileKey::new(true, "ABCDEFGH.EPU").is_some(),
            "a full 8.3 fits"
        );
        assert_eq!(
            FileKey::new(true, "Dune.epub is a long name"),
            None,
            "silently emptying this would make every longer name one key",
        );
        // Fitting is the whole check. This one is not a valid 8.3 name, is
        // accepted here, and fails at the driver, staying distinct meanwhile.
        assert!(FileKey::new(true, "123456789").is_some());
    }

    #[test]
    fn a_record_round_trips() {
        let digest = digest_of(b"abc");
        let parsed = parse_record(&encode_record(&digest)).expect("round trip");
        assert!(parsed.agrees_with(&digest));
        assert_eq!(parsed.byte_len(), digest.byte_len());
    }

    #[test]
    fn a_torn_record_reads_as_absent() {
        let encoded = encode_record(&digest_of(b"abc"));
        for cut in [0usize, 1, 20, SOURCE_RECORD_BYTES - 1] {
            assert_eq!(parse_record(&encoded[..cut]), None, "truncated to {cut}");
        }
    }

    #[test]
    fn a_flipped_bit_anywhere_reads_as_absent() {
        let encoded = encode_record(&digest_of(b"abc"));
        for at in 0..SOURCE_RECORD_BYTES {
            let mut damaged = encoded;
            damaged[at] ^= 0x01;
            assert_eq!(parse_record(&damaged), None, "bit flipped at byte {at}");
        }
    }

    #[test]
    fn another_builds_version_reads_as_absent() {
        let mut encoded = encode_record(&digest_of(b"abc"));
        encoded[OFF_VERSION] = SOURCE_RECORD_VERSION.wrapping_add(1);
        // Rechecksummed, so this is a whole record from a build that is not
        // this one rather than a damaged record from this one.
        let checksum = record_checksum(&encoded[..OFF_CHECKSUM]);
        encoded[OFF_CHECKSUM..].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(parse_record(&encoded), None);
    }

    #[test]
    fn something_else_entirely_reads_as_absent() {
        assert_eq!(parse_record(&[0u8; SOURCE_RECORD_BYTES]), None);
        assert_eq!(parse_record(b"a label, not a record"), None);
    }

    #[test]
    fn length_participates_in_equality() {
        let digest = digest_of(b"abc");
        let other = SourceDigest::from_parts(digest.byte_len() + 1, *digest.sha256());
        assert_ne!(other, digest);
    }
}
