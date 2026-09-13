#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressRecord {
    pub book_id: u32,
    pub page: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppStateRecord {
    pub book_id: u32,
    pub chapter: u16,
    pub screen: u32,
    pub shell_orientation: u8,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
    pub font_size: u8,
    pub line_spacing: u8,
    pub font_weight: u8,
    pub font_family: u8,
    pub front_buttons: u8,
    pub source_hash: u32,
    pub source_size: u32,
    /// Which rule produced `source_hash`: set when the record predates the
    /// catalog-v8 re-key, whose identity was FNV over the display path
    /// rather than the root and locator. The two hash domains can collide
    /// on the same 32 bits, so a reader must resolve an identity under the
    /// rule that wrote it, not try one and fall back to the other. Derived
    /// at decode from the version byte and not persisted: saves derive a
    /// fresh identity from the active entry, so every record this firmware
    /// writes carries the current interpretation.
    pub legacy_source_identity: bool,
}

impl AppStateRecord {
    pub const ENCODED_LEN: usize = 36;
    const V3_ENCODED_LEN: usize = 32;
    const V1_ENCODED_LEN: usize = 24;
    const MAGIC: u32 = 0x5834_4F53;
    /// V5 marks the catalog-v8 identity reinterpretation: `source_hash` in
    /// a v5 record derives from the root and locator, while v4 and older
    /// records carry the display-path hash. The 36-byte layout is identical
    /// to v4; the version byte is what says which rule wrote the identity.
    const VERSION: u8 = 5;
    const V4_VERSION: u8 = 4;
    const V3_VERSION: u8 = 3;
    const V2_VERSION: u8 = 2;
    const V1_VERSION: u8 = 1;
    /// FontSize::Medium / LineSpacing::Normal / FontWeight::Normal as u8 in
    /// app-core.
    const DEFAULT_FONT_SIZE: u8 = 1;
    const DEFAULT_LINE_SPACING: u8 = 1;
    const DEFAULT_FONT_WEIGHT: u8 = 0;
    const DEFAULT_FONT_FAMILY: u8 = 0;

    pub const fn new(book_id: u32) -> Self {
        Self {
            book_id,
            chapter: 0,
            screen: 0,
            shell_orientation: 3,
            reading_orientation: 0,
            refresh_policy: 1,
            font_size: Self::DEFAULT_FONT_SIZE,
            line_spacing: Self::DEFAULT_LINE_SPACING,
            font_weight: Self::DEFAULT_FONT_WEIGHT,
            font_family: Self::DEFAULT_FONT_FAMILY,
            front_buttons: 0,
            source_hash: 0,
            source_size: 0,
            legacy_source_identity: false,
        }
    }

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        write_u32(&mut out, 0, Self::MAGIC);
        out[4] = Self::VERSION;
        out[5] = self.shell_orientation;
        out[6] = self.reading_orientation;
        out[7] = self.refresh_policy;
        write_u32(&mut out, 8, self.book_id);
        write_u16(&mut out, 12, self.chapter);
        write_u32(&mut out, 14, self.screen);
        write_u32(&mut out, 18, self.source_hash);
        write_u32(&mut out, 22, self.source_size);
        out[26] = self.font_size;
        out[27] = self.line_spacing;
        // V4 adds the type weight at byte 28; the checksum span covers the
        // reserved tail. The font family later took reserved byte 29 and the
        // front-button layout byte 30: records written before either carry
        // zero there, which is the respective default (Literata, pages
        // right), so no version bump was needed. Byte 31 stays reserved zero.
        out[28] = self.font_weight;
        out[29] = self.font_family;
        out[30] = self.front_buttons;
        let checksum = checksum(&out[..32]);
        write_u32(&mut out, 32, checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::V1_ENCODED_LEN {
            return None;
        }
        if read_u32(bytes, 0) != Self::MAGIC {
            return None;
        }
        match bytes[4] {
            // V4 shares the v5 layout byte for byte; only the identity
            // interpretation differs, and the flag carries that.
            Self::VERSION | Self::V4_VERSION => {
                if bytes.len() < Self::ENCODED_LEN {
                    return None;
                }
                let expected = read_u32(bytes, 32);
                if checksum(&bytes[..32]) != expected {
                    return None;
                }
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size: bytes[26],
                    line_spacing: bytes[27],
                    font_weight: bytes[28],
                    font_family: bytes[29],
                    front_buttons: bytes[30],
                    source_hash: read_u32(bytes, 18),
                    source_size: read_u32(bytes, 22),
                    legacy_source_identity: bytes[4] == Self::V4_VERSION,
                })
            }
            Self::V3_VERSION | Self::V2_VERSION => {
                if bytes.len() < Self::V3_ENCODED_LEN {
                    return None;
                }
                let expected = read_u32(bytes, 28);
                if checksum(&bytes[..28]) != expected {
                    return None;
                }
                let (font_size, line_spacing) = if bytes[4] == Self::V3_VERSION {
                    (bytes[26], bytes[27])
                } else {
                    (Self::DEFAULT_FONT_SIZE, Self::DEFAULT_LINE_SPACING)
                };
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size,
                    line_spacing,
                    font_weight: Self::DEFAULT_FONT_WEIGHT,
                    font_family: Self::DEFAULT_FONT_FAMILY,
                    front_buttons: 0,
                    source_hash: read_u32(bytes, 18),
                    source_size: read_u32(bytes, 22),
                    legacy_source_identity: true,
                })
            }
            Self::V1_VERSION => {
                let expected = read_u32(bytes, 20);
                if checksum(&bytes[..20]) != expected {
                    return None;
                }
                Some(Self {
                    book_id: read_u32(bytes, 8),
                    chapter: read_u16(bytes, 12),
                    screen: read_u32(bytes, 14),
                    shell_orientation: bytes[5],
                    reading_orientation: bytes[6],
                    refresh_policy: bytes[7],
                    font_size: Self::DEFAULT_FONT_SIZE,
                    line_spacing: Self::DEFAULT_LINE_SPACING,
                    font_weight: Self::DEFAULT_FONT_WEIGHT,
                    font_family: Self::DEFAULT_FONT_FAMILY,
                    front_buttons: 0,
                    source_hash: 0,
                    source_size: 0,
                    // Predates the re-key, though with no identity stored
                    // there is nothing to interpret.
                    legacy_source_identity: true,
                })
            }
            _ => None,
        }
    }
}

pub trait ProgressStore {
    type Error;

    fn load(&mut self) -> Result<Option<ProgressRecord>, Self::Error>;
    fn store(&mut self, record: ProgressRecord) -> Result<(), Self::Error>;
}

pub trait AppStateStore {
    type Error;

    fn load_app_state(&mut self) -> Result<Option<AppStateRecord>, Self::Error>;
    fn store_app_state(&mut self, record: AppStateRecord) -> Result<(), Self::Error>;
}

/// Station credentials at `/READER/WIFI.BIN`, written by the onboarding
/// portal and read back ahead of every sync session. Same envelope as
/// `AppStateRecord`: magic, version, payload, checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiCredentialsRecord {
    pub ssid: [u8; 32],
    pub ssid_len: u8,
    pub password: [u8; 64],
    pub password_len: u8,
}

impl WifiCredentialsRecord {
    pub const ENCODED_LEN: usize = 4 + 1 + 1 + 1 + 32 + 64 + 4;
    const MAGIC: u32 = 0x5834_5746; // "X4WF"
    const VERSION: u8 = 1;

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        write_u32(&mut out, 0, Self::MAGIC);
        out[4] = Self::VERSION;
        out[5] = self.ssid_len.min(32);
        out[6] = self.password_len.min(64);
        out[7..39].copy_from_slice(&self.ssid);
        out[39..103].copy_from_slice(&self.password);
        let checksum = checksum(&out[..103]);
        write_u32(&mut out, 103, checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN
            || read_u32(bytes, 0) != Self::MAGIC
            || bytes[4] != Self::VERSION
            || read_u32(bytes, 103) != checksum(&bytes[..103])
        {
            return None;
        }
        let mut record = Self {
            ssid: [0; 32],
            ssid_len: bytes[5].min(32),
            password: [0; 64],
            password_len: bytes[6].min(64),
        };
        record.ssid.copy_from_slice(&bytes[7..39]);
        record.password.copy_from_slice(&bytes[39..103]);
        if record.ssid_len == 0 {
            return None;
        }
        Some(record)
    }
}

/// Where the station last associated: the AP a directed join should try
/// first, so a repeat session skips the all-channel scan.
///
/// Deliberately **not** part of [`WifiCredentialsRecord`]. The hint is a pure
/// accelerator — wrong, stale or missing, the join falls back to the scan and
/// nothing is lost — while the credentials are the one thing a user would
/// hate to lose. Widening that record to carry this would couple them: the
/// durable layer checks payload length exactly, so a longer record reads as
/// absent to any firmware expecting the old one, and a rollback would take
/// the saved password with it. A separate file costs one small read per
/// wireless session and keeps the accelerator disposable.
///
/// `ssid_hash` is what makes a stale hint safe: a hint learned on one network
/// must not steer a join to another, and the hash is enough to tell them
/// apart without storing the SSID twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiApHintRecord {
    pub ssid_hash: u32,
    pub bssid: [u8; 6],
    /// 1-14. Zero is not a valid Wi-Fi channel and never decodes.
    pub channel: u8,
}

impl WifiApHintRecord {
    pub const ENCODED_LEN: usize = 4 + 1 + 4 + 6 + 1 + 4;
    const MAGIC: u32 = 0x5834_4148; // "X4AH"
    const VERSION: u8 = 1;
    /// Above this, the value is not a channel number and the record is a
    /// decode failure rather than something to clamp.
    const MAX_CHANNEL: u8 = 14;

    /// Hash an SSID the same way both writers and readers must.
    pub fn hash_ssid(ssid: &[u8]) -> u32 {
        checksum(ssid)
    }

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        write_u32(&mut out, 0, Self::MAGIC);
        out[4] = Self::VERSION;
        write_u32(&mut out, 5, self.ssid_hash);
        out[9..15].copy_from_slice(&self.bssid);
        out[15] = self.channel;
        let checksum = checksum(&out[..16]);
        write_u32(&mut out, 16, checksum);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN
            || read_u32(bytes, 0) != Self::MAGIC
            || bytes[4] != Self::VERSION
            || read_u32(bytes, 16) != checksum(&bytes[..16])
        {
            return None;
        }
        let mut bssid = [0u8; 6];
        bssid.copy_from_slice(&bytes[9..15]);
        let channel = bytes[15];
        // A hint that names no channel steers nothing, and an out-of-range
        // one would be handed straight to the radio.
        if channel == 0 || channel > Self::MAX_CHANNEL || bssid == [0u8; 6] {
            return None;
        }
        Some(Self {
            ssid_hash: read_u32(bytes, 5),
            bssid,
            channel,
        })
    }

    /// Whether this hint was learned for `ssid`, and so may steer its join.
    pub fn matches_ssid(&self, ssid: &[u8]) -> bool {
        self.ssid_hash == Self::hash_ssid(ssid)
    }
}

/// Per-book reading position, stored as POS.BIN beside that book's cache.
///
/// The authoritative record of where the reader is in a book: the global
/// [`AppStateRecord`] carries a copy, but only as a mirror for readers that
/// expect to find position there.
///
/// `salt` is mixed into the checksum by the caller rather than fixed here. The
/// stored screen is a page index under one panel's pagination, so a card moved
/// between panels of different sizes must fail validation and resume at the
/// book's start instead of a page that does not exist. The geometry that
/// decides the salt lives above this crate, and a salt of zero leaves the
/// checksum byte-identical to an unsalted one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionRecord {
    pub chapter: u16,
    pub screen: u32,
}

impl PositionRecord {
    pub const ENCODED_LEN: usize = 15;
    const MAGIC: &'static [u8; 4] = b"X4PS";
    const VERSION: u8 = 1;
    /// The checksum spans everything before it.
    const CHECKSUM_AT: usize = 11;

    pub fn encode(self, salt: u32) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..4].copy_from_slice(Self::MAGIC);
        out[4] = Self::VERSION;
        out[5..7].copy_from_slice(&self.chapter.to_le_bytes());
        out[7..11].copy_from_slice(&self.screen.to_le_bytes());
        let sum = Self::checksum(&out[..Self::CHECKSUM_AT], salt);
        out[Self::CHECKSUM_AT..].copy_from_slice(&sum.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8], salt: u32) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN
            || &bytes[..4] != Self::MAGIC
            || bytes[4] != Self::VERSION
        {
            return None;
        }
        let sum = Self::checksum(&bytes[..Self::CHECKSUM_AT], salt);
        if bytes[Self::CHECKSUM_AT..Self::ENCODED_LEN] != sum.to_le_bytes() {
            return None;
        }
        Some(Self {
            chapter: u16::from_le_bytes([bytes[5], bytes[6]]),
            screen: u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]),
        })
    }

    /// Byte sum shifted by the panel salt. Deliberately not the FNV hash the
    /// other records use: this envelope is shared with MarigoldOS and has to
    /// stay byte-identical.
    fn checksum(bytes: &[u8], salt: u32) -> u32 {
        bytes
            .iter()
            .map(|byte| *byte as u32)
            .sum::<u32>()
            .wrapping_add(salt)
    }
}

/// What a place was written against, for deciding whether its anchor still
/// describes the book.
///
/// Content, not location. A move changes where a copy sits and changes
/// nothing about what it holds, and the anchor is the thing that is supposed
/// to survive a move, so anything path-derived here would throw away exactly
/// the case the record exists for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaceSource {
    /// The file's length. Move-invariant and cheap, and a replacement of a
    /// different length is caught by it alone.
    pub byte_size: u32,
    /// The hash recorded for this copy, when one has been read. Absent for a
    /// copy nobody has read yet, which the library identity PRD's R4 accepts:
    /// a same-sized replacement of a copy with no recorded bytes cannot be
    /// told from the original by anything the device holds.
    pub digest: Option<[u8; 32]>,
}

impl PlaceSource {
    /// Whether a place written against `self` still describes `now`.
    ///
    /// Length first, since it settles most replacements and costs nothing.
    /// Then the recorded hashes, which settle the same-length replacement
    /// when both sides have been read. Two absent hashes read as the same
    /// source, by the same rule that lets a copy be adopted without reading
    /// it.
    pub fn describes(&self, now: &Self) -> bool {
        if self.byte_size != now.byte_size {
            return false;
        }
        match (self.digest, now.digest) {
            (Some(was), Some(is)) => was == is,
            _ => true,
        }
    }
}

/// A reader's place in one library copy, addressed by the copy rather than by
/// where its file sits.
///
/// Replaces [`PositionRecord`], which names a page under one pagination and
/// lives in a directory keyed by where the file sits, so a settings change or
/// a move loses it.
///
/// No panel salt, unlike that record. A page index means nothing on a
/// differently sized screen and an anchor means the same thing on every one,
/// so salting this would invent a reason to discard it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceRecord {
    /// The copy this place belongs to. Stored in full even though the file
    /// sits in a directory named from it, so a directory-name collision is
    /// caught rather than silently handing one book another's place.
    pub id: crate::identity::BookId,
    pub anchor: crate::anchor::ContentAnchor,
    /// What the anchor was resolved against, so a source change is detectable
    /// without making the source own the place.
    pub source: PlaceSource,
    /// How far through the book the reader was, as a fraction of `u16::MAX`,
    /// or `None` while no complete pagination has said how long the book is.
    ///
    /// Fallback, not authority: read only when the source changed under the
    /// copy. `None` rather than a fraction of a half-built book, because a
    /// page total from a partial index is a floor and dividing by it would
    /// call page 10 of an eventual 200 the halfway mark.
    pub progression: Option<u16>,
}

impl PlaceRecord {
    pub const ENCODED_LEN: usize = 75;
    const MAGIC: &'static [u8; 4] = b"X4PL";
    const VERSION: u8 = 1;
    const CHECKSUM_AT: usize = 71;

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..4].copy_from_slice(Self::MAGIC);
        out[4] = Self::VERSION;
        out[5] = crate::anchor::CONTENT_STREAM_VERSION;
        out[6..22].copy_from_slice(&self.id.to_bytes());
        let mut anchor = [0u8; crate::anchor::CONTENT_ANCHOR_BYTES];
        self.anchor.encode(&mut anchor);
        out[22..28].copy_from_slice(&anchor);
        out[28..32].copy_from_slice(&self.source.byte_size.to_le_bytes());
        if let Some(digest) = self.source.digest {
            out[32] = 1;
            out[33..65].copy_from_slice(&digest);
        }
        if let Some(progression) = self.progression {
            out[65] = 1;
            out[66..68].copy_from_slice(&progression.to_le_bytes());
        }
        let sum = checksum(&out[..Self::CHECKSUM_AT]);
        out[Self::CHECKSUM_AT..].copy_from_slice(&sum.to_le_bytes());
        out
    }

    /// Whether this place was written against the bytes a reader is holding
    /// now. False demotes the anchor to a guess.
    pub fn describes(&self, source: &PlaceSource) -> bool {
        self.source.describes(source)
    }

    /// `None` for anything this build cannot read as a place: another magic,
    /// another record version, a content stream this build does not index the
    /// same way, or a checksum that fails. Each of those is a place that
    /// cannot be trusted to name content, and a wrong place is worse than
    /// opening at the start.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::ENCODED_LEN
            || &bytes[..4] != Self::MAGIC
            || bytes[4] != Self::VERSION
            || bytes[5] != crate::anchor::CONTENT_STREAM_VERSION
        {
            return None;
        }
        let sum = checksum(&bytes[..Self::CHECKSUM_AT]);
        if bytes[Self::CHECKSUM_AT..Self::ENCODED_LEN] != sum.to_le_bytes() {
            return None;
        }
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes[6..22]);
        let mut anchor = [0u8; crate::anchor::CONTENT_ANCHOR_BYTES];
        anchor.copy_from_slice(&bytes[22..28]);
        let digest = (bytes[32] == 1).then(|| {
            let mut sha = [0u8; 32];
            sha.copy_from_slice(&bytes[33..65]);
            sha
        });
        Some(Self {
            id: crate::identity::BookId::from_bytes(id)?,
            anchor: crate::anchor::ContentAnchor::decode(&anchor),
            source: PlaceSource {
                byte_size: u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
                digest,
            },
            progression: (bytes[65] == 1).then(|| u16::from_le_bytes([bytes[66], bytes[67]])),
        })
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut hash = 0x811C_9DC5u32;
    for byte in bytes {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset] = value as u8;
    out[offset + 1] = (value >> 8) as u8;
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset] = value as u8;
    out[offset + 1] = (value >> 8) as u8;
    out[offset + 2] = (value >> 16) as u8;
    out[offset + 3] = (value >> 24) as u8;
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    bytes[offset] as u16 | ((bytes[offset + 1] as u16) << 8)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    bytes[offset] as u32
        | ((bytes[offset + 1] as u32) << 8)
        | ((bytes[offset + 2] as u32) << 16)
        | ((bytes[offset + 3] as u32) << 24)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ap_hint_round_trips_and_rejects_nonsense() {
        let hint = WifiApHintRecord {
            ssid_hash: WifiApHintRecord::hash_ssid(b"home-network"),
            bssid: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01],
            channel: 11,
        };
        let bytes = hint.encode();
        assert_eq!(WifiApHintRecord::decode(&bytes), Some(hint));
        assert!(hint.matches_ssid(b"home-network"));
        // The hash is the whole guard against a hint steering the wrong
        // network's join.
        assert!(!hint.matches_ssid(b"cafe-wifi"));

        let mut corrupt = bytes;
        corrupt[0] ^= 0xFF;
        assert_eq!(WifiApHintRecord::decode(&corrupt), None);
        corrupt = bytes;
        corrupt[4] = WifiApHintRecord::VERSION + 1;
        assert_eq!(WifiApHintRecord::decode(&corrupt), None);
        // A flipped payload byte must fail the checksum, not ride through.
        corrupt = bytes;
        corrupt[10] ^= 0x01;
        assert_eq!(WifiApHintRecord::decode(&corrupt), None);
        assert_eq!(
            WifiApHintRecord::decode(&bytes[..WifiApHintRecord::ENCODED_LEN - 1]),
            None
        );
    }

    #[test]
    fn ap_hint_rejects_values_that_would_steer_a_join_nowhere() {
        // Both of these are what a zeroed or half-written record looks like,
        // and both would be handed straight to the radio.
        for (bssid, channel) in [
            ([0u8; 6], 6u8),
            ([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01], 0),
            ([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01], 15),
            ([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01], 255),
        ] {
            let bytes = WifiApHintRecord {
                ssid_hash: 1,
                bssid,
                channel,
            }
            .encode();
            assert_eq!(
                WifiApHintRecord::decode(&bytes),
                None,
                "bssid {bssid:?} channel {channel} must not decode"
            );
        }
    }

    fn record() -> AppStateRecord {
        AppStateRecord {
            book_id: 7,
            chapter: 3,
            screen: 41,
            shell_orientation: 2,
            reading_orientation: 1,
            refresh_policy: 2,
            font_size: 2,
            line_spacing: 0,
            font_weight: 1,
            font_family: 1,
            front_buttons: 1,
            source_hash: 0xDEAD_BEEF,
            source_size: 123_456,
            legacy_source_identity: false,
        }
    }

    /// The exact bytes `record()` encodes to, computed independently of this
    /// implementation.
    ///
    /// The other tests here prove old records still decode; this one proves the
    /// layout itself has not moved. Both files this crate describes are shared
    /// byte-for-byte with MarigoldOS so cards carry reading state between the
    /// two firmwares, and every compatibility test in this module would still
    /// pass if the whole envelope shifted underneath them in lockstep.
    const STATE_GOLDEN: [u8; AppStateRecord::ENCODED_LEN] = [
        0x53, 0x4f, 0x34, 0x58, 0x05, 0x02, 0x01, 0x02, 0x07, 0x00, 0x00, 0x00, 0x03, 0x00, 0x29,
        0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x40, 0xe2, 0x01, 0x00, 0x02, 0x00, 0x01, 0x01,
        0x01, 0x00, 0x1e, 0x1a, 0xaa, 0xd7,
    ];

    /// The same state as `record()`, as v4 firmware wrote it: version byte 4
    /// and its checksum, identical layout otherwise.
    const STATE_GOLDEN_V4: [u8; AppStateRecord::ENCODED_LEN] = [
        0x53, 0x4f, 0x34, 0x58, 0x04, 0x02, 0x01, 0x02, 0x07, 0x00, 0x00, 0x00, 0x03, 0x00, 0x29,
        0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x40, 0xe2, 0x01, 0x00, 0x02, 0x00, 0x01, 0x01,
        0x01, 0x00, 0xa7, 0x76, 0x1e, 0x60,
    ];

    #[test]
    fn app_state_encodes_to_the_agreed_bytes() {
        assert_eq!(record().encode(), STATE_GOLDEN);
        assert_eq!(AppStateRecord::decode(&STATE_GOLDEN), Some(record()));
    }

    /// A record pre-v8 firmware wrote decodes unchanged, and its identity is
    /// marked for the legacy interpretation. The two hash domains can
    /// collide on the same 32 bits, so the version byte is the only thing
    /// keeping a pre-v8 identity from resolving as an unrelated v8 book.
    #[test]
    fn v4_records_decode_with_the_legacy_identity_interpretation() {
        let expected = AppStateRecord {
            legacy_source_identity: true,
            ..record()
        };
        assert_eq!(AppStateRecord::decode(&STATE_GOLDEN_V4), Some(expected));
    }

    /// `PositionRecord { chapter: 3, screen: 41 }` at an unsalted checksum.
    const POSITION_GOLDEN: [u8; PositionRecord::ENCODED_LEN] = [
        0x58, 0x34, 0x50, 0x53, 0x01, 0x03, 0x00, 0x29, 0x00, 0x00, 0x00, 0x5c, 0x01, 0x00, 0x00,
    ];

    fn position() -> PositionRecord {
        PositionRecord {
            chapter: 3,
            screen: 41,
        }
    }

    #[test]
    fn position_encodes_to_the_agreed_bytes() {
        assert_eq!(position().encode(0), POSITION_GOLDEN);
        assert_eq!(
            PositionRecord::decode(&POSITION_GOLDEN, 0),
            Some(position())
        );
    }

    #[test]
    fn position_round_trips_under_any_salt() {
        for salt in [0, 1, 0x0100_0193, u32::MAX] {
            let encoded = position().encode(salt);
            assert_eq!(
                PositionRecord::decode(&encoded, salt),
                Some(position()),
                "salt {salt:#x} must round trip"
            );
        }
    }

    #[test]
    fn a_position_from_another_panel_is_refused() {
        // The stored screen is a page index under one pagination. Reading it
        // back under a different geometry has to fail rather than resume at a
        // page that does not exist in this one.
        let written_on_another_panel = position().encode(0x0011_0022);
        assert_eq!(PositionRecord::decode(&written_on_another_panel, 0), None);
    }

    #[test]
    fn a_corrupt_position_is_refused() {
        for byte in [0, 4, 5, 11] {
            let mut encoded = position().encode(0);
            encoded[byte] ^= 0xFF;
            assert_eq!(
                PositionRecord::decode(&encoded, 0),
                None,
                "a flipped byte {byte} must not decode"
            );
        }
        assert_eq!(
            PositionRecord::decode(&position().encode(0)[..14], 0),
            None,
            "a truncated record must not decode"
        );
    }

    #[test]
    fn app_state_round_trips_with_type_settings() {
        let encoded = record().encode();
        assert_eq!(AppStateRecord::decode(&encoded), Some(record()));
    }

    #[test]
    fn v3_records_decode_with_default_weight() {
        // A V3 record keeps its size/spacing but predates the weight byte, so
        // it must decode as the default weight. Rebuild the record as a 32-byte
        // V3 image: version 3 with the checksum over the first 28 bytes.
        let mut encoded = record().encode();
        encoded[4] = AppStateRecord::V3_VERSION;
        let checksum = checksum(&encoded[..28]);
        write_u32(&mut encoded, 28, checksum);

        let decoded =
            AppStateRecord::decode(&encoded[..AppStateRecord::V3_ENCODED_LEN]).expect("v3 decodes");
        assert_eq!(decoded.font_size, 2);
        assert_eq!(decoded.line_spacing, 0);
        assert_eq!(decoded.font_weight, AppStateRecord::DEFAULT_FONT_WEIGHT);
        assert_eq!(decoded.book_id, 7);
        assert!(decoded.legacy_source_identity);
    }

    #[test]
    fn v2_records_decode_with_default_type_settings() {
        // A V2 record zeroes the type bytes; size, spacing, and weight all
        // fall back to defaults. The checksum spans the first 28 bytes.
        let mut encoded = record().encode();
        encoded[4] = AppStateRecord::V2_VERSION;
        encoded[26] = 0;
        encoded[27] = 0;
        let checksum = checksum(&encoded[..28]);
        write_u32(&mut encoded, 28, checksum);

        let decoded =
            AppStateRecord::decode(&encoded[..AppStateRecord::V3_ENCODED_LEN]).expect("v2 decodes");
        assert_eq!(decoded.font_size, AppStateRecord::DEFAULT_FONT_SIZE);
        assert_eq!(decoded.line_spacing, AppStateRecord::DEFAULT_LINE_SPACING);
        assert_eq!(decoded.font_weight, AppStateRecord::DEFAULT_FONT_WEIGHT);
        assert_eq!(decoded.book_id, 7);
        assert_eq!(decoded.source_hash, 0xDEAD_BEEF);
        assert!(decoded.legacy_source_identity);
    }

    #[test]
    fn pre_family_v4_records_decode_as_literata() {
        // V4 records written before the Font setting carry the reserved zero
        // at byte 29; that must decode as the default (Literata) family.
        let mut encoded = record().encode();
        encoded[4] = AppStateRecord::V4_VERSION;
        encoded[29] = 0;
        let checksum = checksum(&encoded[..32]);
        write_u32(&mut encoded, 32, checksum);

        let decoded = AppStateRecord::decode(&encoded).expect("pre-family v4 decodes");
        assert_eq!(decoded.font_family, AppStateRecord::DEFAULT_FONT_FAMILY);
        assert_eq!(decoded.font_weight, 1);
    }

    #[test]
    fn pre_front_buttons_v4_records_decode_as_pages_right() {
        // V4 records written before the Front buttons setting carry the
        // reserved zero at byte 30; that must decode as the default
        // (pages right) layout.
        let mut encoded = record().encode();
        encoded[4] = AppStateRecord::V4_VERSION;
        encoded[30] = 0;
        let checksum = checksum(&encoded[..32]);
        write_u32(&mut encoded, 32, checksum);

        let decoded = AppStateRecord::decode(&encoded).expect("pre-front-buttons v4 decodes");
        assert_eq!(decoded.front_buttons, 0);
        assert_eq!(decoded.font_family, 1);
    }

    #[test]
    fn corrupt_checksum_is_rejected() {
        let mut encoded = record().encode();
        encoded[26] ^= 0xFF;
        assert_eq!(AppStateRecord::decode(&encoded), None);
    }
}
