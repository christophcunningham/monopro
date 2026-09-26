//! The camera's own record of a frame — body, lens, exposure, when — carried from the
//! source into an exported file.
//!
//! # What travels, and what never does
//!
//! **A fixed list, not "all of the EXIF".** Copying the source's whole block would carry
//! things that are wrong for an export and things that are nobody else's business:
//!
//! - **Never location.** GPS is the privacy case the export setting exists around, and a
//!   print of a street is not a map of where the photographer lives.
//! - **Never serial numbers or the owner's name.** They identify a person's equipment
//!   across every picture they publish.
//! - **Never maker notes.** An undocumented binary block, several hundred kilobytes on
//!   some bodies, which other software rewrites wrongly more often than not.
//! - **Never orientation or dimensions.** An export is already the right way up and at
//!   its own size, so the source's values would describe a different picture — a
//!   rotated frame would be turned a second time by every viewer that believes it.
//! - **Not the color space, artist or copyright.** The export carries its own profile,
//!   and authorship is IPTC's, under its own setting.
//!
//! What is left is the photograph's technical record: what took it, through what, at
//! what settings, and when.
//!
//! # One list, two encodings
//!
//! [`CameraExif`] holds neutral [`Entry`] values so the two writers cannot disagree about
//! what the list is: [`CameraExif::tiff_block`] encodes it as the TIFF-structured block
//! JPEG's APP1 and PNG's `eXIf` carry, and a TIFF export writes the same entries into
//! its own IFD0 and Exif IFD.

use std::path::Path;

use rawler::decoders::RawDecodeParams;
use rawler::rawsource::RawSource;

/// One EXIF value, in the TIFF field types the list uses.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Ascii(String),
    Short(u16),
    Rational(u32, u32),
    SRational(i32, i32),
    Rationals(Vec<(u32, u32)>),
    Undefined(Vec<u8>),
}

/// One tag, and whether it belongs in the Exif IFD rather than IFD0.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub tag: u16,
    pub exif_ifd: bool,
    pub value: Value,
}

/// The tags that travel, in tag order within each IFD. See the module note for the ones
/// that do not.
mod tag {
    // IFD0
    pub const MAKE: u16 = 0x010F;
    pub const MODEL: u16 = 0x0110;
    // Exif IFD
    pub const EXPOSURE_TIME: u16 = 0x829A;
    pub const F_NUMBER: u16 = 0x829D;
    pub const EXPOSURE_PROGRAM: u16 = 0x8822;
    pub const ISO: u16 = 0x8827;
    pub const EXIF_VERSION: u16 = 0x9000;
    pub const DATE_TIME_ORIGINAL: u16 = 0x9003;
    pub const DATE_TIME_DIGITIZED: u16 = 0x9004;
    pub const OFFSET_TIME_ORIGINAL: u16 = 0x9011;
    pub const EXPOSURE_BIAS: u16 = 0x9204;
    pub const MAX_APERTURE: u16 = 0x9205;
    pub const METERING_MODE: u16 = 0x9207;
    pub const FLASH: u16 = 0x9209;
    pub const FOCAL_LENGTH: u16 = 0x920A;
    pub const SUB_SEC_TIME_ORIGINAL: u16 = 0x9291;
    pub const EXPOSURE_MODE: u16 = 0xA402;
    pub const WHITE_BALANCE: u16 = 0xA403;
    pub const LENS_SPECIFICATION: u16 = 0xA432;
    pub const LENS_MAKE: u16 = 0xA433;
    pub const LENS_MODEL: u16 = 0xA434;

    /// Every tag read from a rendered source, with the IFD it lives in. The raw path
    /// builds the same set from rawler's typed fields.
    pub const CARRIED: [(u16, bool); 20] = [
        (MAKE, false),
        (MODEL, false),
        (EXPOSURE_TIME, true),
        (F_NUMBER, true),
        (EXPOSURE_PROGRAM, true),
        (ISO, true),
        (DATE_TIME_ORIGINAL, true),
        (DATE_TIME_DIGITIZED, true),
        (OFFSET_TIME_ORIGINAL, true),
        (EXPOSURE_BIAS, true),
        (MAX_APERTURE, true),
        (METERING_MODE, true),
        (FLASH, true),
        (FOCAL_LENGTH, true),
        (SUB_SEC_TIME_ORIGINAL, true),
        (EXPOSURE_MODE, true),
        (WHITE_BALANCE, true),
        (LENS_SPECIFICATION, true),
        (LENS_MAKE, true),
        (LENS_MODEL, true),
    ];
}

/// The camera's record of one frame. Empty when the source carries none.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CameraExif {
    entries: Vec<Entry>,
}

impl CameraExif {
    /// Read the carried tags from `path`: through rawler for a raw, and through the
    /// EXIF reader Lightbox already uses for anything else. `None` when the file has
    /// nothing on the list.
    ///
    /// **This costs a full read of a raw**, like `sensor::capture_time`; call it once
    /// per export, not per frame drawn.
    pub fn read(path: &Path) -> Option<Self> {
        let found = read_raw(path).or_else(|| read_rendered(path))?;
        (!found.entries.is_empty()).then_some(found)
    }

    /// A record from entries already in hand, held to the same rules as one read from
    /// a file: blank text is dropped, the Exif IFD gets its version, and the order is
    /// TIFF's.
    pub fn from_entries(entries: impl IntoIterator<Item = Entry>) -> Self {
        let mut out = Self::default();
        for e in entries {
            if e.tag != tag::EXIF_VERSION {
                out.push(e.tag, e.exif_ifd, Some(e.value));
            }
        }
        out.sorted()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add one entry, skipping blank text. The first entry for the Exif IFD brings the
    /// ExifVersion that IFD is required to carry.
    fn push(&mut self, tag: u16, exif_ifd: bool, value: Option<Value>) {
        let Some(value) = value else {
            return;
        };
        if let Value::Ascii(text) = &value
            && (text.trim().is_empty() || text.contains('\0'))
        {
            return;
        }
        if exif_ifd && !self.entries.iter().any(|e| e.tag == tag::EXIF_VERSION) {
            self.entries.push(Entry {
                tag: tag::EXIF_VERSION,
                exif_ifd: true,
                value: Value::Undefined(b"0232".to_vec()),
            });
        }
        self.entries.push(Entry {
            tag,
            exif_ifd,
            value,
        });
    }

    /// Sort into tag order within each IFD, which is what TIFF requires.
    fn sorted(mut self) -> Self {
        self.entries.sort_by_key(|e| (e.exif_ifd, e.tag));
        self
    }

    /// The list as a TIFF-structured EXIF block: IFD0 and the Exif IFD it points to,
    /// little-endian. What JPEG's APP1 carries after `Exif\0\0`, and PNG's `eXIf` as is.
    pub fn tiff_block(&self) -> Vec<u8> {
        use exif::experimental::Writer;
        let fields: Vec<exif::Field> = self
            .entries
            .iter()
            .map(|e| {
                let context = if e.exif_ifd {
                    exif::Context::Exif
                } else {
                    exif::Context::Tiff
                };
                exif::Field {
                    tag: exif::Tag(context, e.tag),
                    ifd_num: exif::In::PRIMARY,
                    value: match &e.value {
                        Value::Ascii(s) => exif::Value::Ascii(vec![s.as_bytes().to_vec()]),
                        Value::Short(v) => exif::Value::Short(vec![*v]),
                        Value::Rational(n, d) => {
                            exif::Value::Rational(vec![exif::Rational { num: *n, denom: *d }])
                        }
                        Value::SRational(n, d) => {
                            exif::Value::SRational(vec![exif::SRational { num: *n, denom: *d }])
                        }
                        Value::Rationals(list) => exif::Value::Rational(
                            list.iter()
                                .map(|(n, d)| exif::Rational { num: *n, denom: *d })
                                .collect(),
                        ),
                        Value::Undefined(b) => exif::Value::Undefined(b.clone(), 0),
                    },
                }
            })
            .collect();
        let mut writer = Writer::new();
        for field in &fields {
            writer.push_field(field);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        // Writing to memory cannot fail on I/O, and every value above is one the
        // writer accepts; an empty block is the honest answer if that ever changes.
        if writer.write(&mut out, true).is_err() {
            return Vec::new();
        }
        out.into_inner()
    }
}

fn read_raw(path: &Path) -> Option<CameraExif> {
    let src = RawSource::new(path).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let meta = decoder
        .raw_metadata(&src, &RawDecodeParams::default())
        .ok()?;
    let e = meta.exif;
    let rational =
        |r: &rawler::formats::tiff::Rational| (r.d != 0).then_some(Value::Rational(r.n, r.d));
    let ascii = |s: Option<String>| s.map(|s| Value::Ascii(s.trim().to_owned()));

    let mut out = CameraExif::default();
    out.push(tag::MAKE, false, ascii(Some(meta.make)));
    out.push(tag::MODEL, false, ascii(Some(meta.model)));
    out.push(
        tag::EXPOSURE_TIME,
        true,
        e.exposure_time.as_ref().and_then(rational),
    );
    out.push(tag::F_NUMBER, true, e.fnumber.as_ref().and_then(rational));
    out.push(
        tag::EXPOSURE_PROGRAM,
        true,
        e.exposure_program.map(Value::Short),
    );
    // PhotographicSensitivity is a SHORT; a body past 65535 records the rest elsewhere,
    // and the short field saturates as the standard says it should.
    let iso = e
        .iso_speed_ratings
        .map(u32::from)
        .or(e.iso_speed)
        .or(e.recommended_exposure_index)
        .filter(|iso| *iso > 0)
        .map(|iso| Value::Short(iso.min(u32::from(u16::MAX)) as u16));
    out.push(tag::ISO, true, iso);
    out.push(tag::DATE_TIME_ORIGINAL, true, ascii(e.date_time_original));
    out.push(tag::DATE_TIME_DIGITIZED, true, ascii(e.create_date));
    out.push(
        tag::OFFSET_TIME_ORIGINAL,
        true,
        ascii(e.offset_time_original),
    );
    out.push(
        tag::EXPOSURE_BIAS,
        true,
        e.exposure_bias
            .filter(|r| r.d != 0)
            .map(|r| Value::SRational(r.n, r.d)),
    );
    out.push(
        tag::MAX_APERTURE,
        true,
        e.max_aperture_value.as_ref().and_then(rational),
    );
    out.push(tag::METERING_MODE, true, e.metering_mode.map(Value::Short));
    out.push(tag::FLASH, true, e.flash.map(Value::Short));
    out.push(
        tag::FOCAL_LENGTH,
        true,
        e.focal_length.as_ref().and_then(rational),
    );
    out.push(
        tag::SUB_SEC_TIME_ORIGINAL,
        true,
        ascii(e.sub_sec_time_original),
    );
    out.push(tag::EXPOSURE_MODE, true, e.exposure_mode.map(Value::Short));
    out.push(tag::WHITE_BALANCE, true, e.white_balance.map(Value::Short));
    out.push(
        tag::LENS_SPECIFICATION,
        true,
        e.lens_spec
            .filter(|spec| spec.iter().all(|r| r.d != 0))
            .map(|spec| Value::Rationals(spec.iter().map(|r| (r.n, r.d)).collect())),
    );
    out.push(tag::LENS_MAKE, true, ascii(e.lens_make));
    out.push(tag::LENS_MODEL, true, ascii(e.lens_model));
    Some(out.sorted())
}

fn read_rendered(path: &Path) -> Option<CameraExif> {
    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(file))
        .ok()?;
    let mut out = CameraExif::default();
    for (number, exif_ifd) in tag::CARRIED {
        let context = if exif_ifd {
            exif::Context::Exif
        } else {
            exif::Context::Tiff
        };
        let Some(field) = exif.get_field(exif::Tag(context, number), exif::In::PRIMARY) else {
            continue;
        };
        let value = match &field.value {
            exif::Value::Ascii(parts) => parts
                .first()
                .map(|p| Value::Ascii(String::from_utf8_lossy(p).trim().to_owned())),
            exif::Value::Short(v) => v.first().map(|v| Value::Short(*v)),
            exif::Value::Long(v) => v
                .first()
                .map(|v| Value::Short((*v).min(u32::from(u16::MAX)) as u16)),
            exif::Value::Rational(v) if number == tag::LENS_SPECIFICATION => (v.len() == 4
                && v.iter().all(|r| r.denom != 0))
            .then(|| Value::Rationals(v.iter().map(|r| (r.num, r.denom)).collect())),
            exif::Value::Rational(v) => v
                .first()
                .filter(|r| r.denom != 0)
                .map(|r| Value::Rational(r.num, r.denom)),
            exif::Value::SRational(v) => v
                .first()
                .filter(|r| r.denom != 0)
                .map(|r| Value::SRational(r.num, r.denom)),
            _ => None,
        };
        out.push(number, exif_ifd, value);
    }
    Some(out.sorted())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CameraExif {
        let mut c = CameraExif::default();
        c.push(tag::MAKE, false, Some(Value::Ascii("Leica".into())));
        c.push(tag::MODEL, false, Some(Value::Ascii("M10-R".into())));
        c.push(tag::EXPOSURE_TIME, true, Some(Value::Rational(1, 500)));
        c.push(tag::F_NUMBER, true, Some(Value::Rational(17, 10)));
        c.push(tag::ISO, true, Some(Value::Short(200)));
        c.push(
            tag::DATE_TIME_ORIGINAL,
            true,
            Some(Value::Ascii("2026:03:08 10:26:38".into())),
        );
        c.push(tag::EXPOSURE_BIAS, true, Some(Value::SRational(-1, 3)));
        c.push(
            tag::LENS_MODEL,
            true,
            Some(Value::Ascii("Summilux-M 50".into())),
        );
        c.sorted()
    }

    #[test]
    fn the_block_reads_back_as_the_camera_wrote_it() {
        let block = sample().tiff_block();
        let exif = exif::Reader::new()
            .read_raw(block)
            .expect("the block is valid EXIF");
        let get = |t: exif::Tag| {
            exif.get_field(t, exif::In::PRIMARY)
                .map(|f| f.display_value().to_string())
        };
        assert_eq!(get(exif::Tag::Make).as_deref(), Some("\"Leica\""));
        assert_eq!(get(exif::Tag::Model).as_deref(), Some("\"M10-R\""));
        assert_eq!(get(exif::Tag::ExposureTime).as_deref(), Some("1/500"));
        assert_eq!(
            get(exif::Tag::PhotographicSensitivity).as_deref(),
            Some("200")
        );
        assert_eq!(get(exif::Tag::ExifVersion).as_deref(), Some("2.32"));
        assert_eq!(
            get(exif::Tag::DateTimeOriginal).as_deref(),
            Some("2026-03-08 10:26:38")
        );
    }

    #[test]
    fn nothing_that_identifies_a_person_or_a_place_is_on_the_list() {
        let banned = [
            0x0112, // Orientation
            0x8825, // GPS IFD pointer
            0x927C, // MakerNote
            0x9286, // UserComment
            0xA430, // CameraOwnerName
            0xA431, // BodySerialNumber
            0xA435, // LensSerialNumber
            0x013B, // Artist
            0x8298, // Copyright
            0xA001, // ColorSpace
        ];
        for (number, _) in tag::CARRIED {
            assert!(!banned.contains(&number), "{number:#06x} must not travel");
        }
    }

    #[test]
    fn a_corpus_raw_carries_its_camera_and_exposure() {
        // The private corpus, beside the workspace or inside it (docs/private-fixtures.md).
        // Skipped rather than failed without it, like every other corpus test.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Some(raw) = [
            root.join("raws/L1000016.DNG"),
            root.join("../raws/L1000016.DNG"),
        ]
        .into_iter()
        .find(|p| p.is_file()) else {
            eprintln!("skipped: no corpus raw L1000016.DNG");
            return;
        };
        let found = CameraExif::read(&raw).expect("a Leica DNG records its camera");
        let has = |t: u16| found.entries().iter().any(|e| e.tag == t);
        for t in [
            tag::MAKE,
            tag::MODEL,
            tag::EXPOSURE_TIME,
            tag::ISO,
            tag::DATE_TIME_ORIGINAL,
        ] {
            assert!(has(t), "{t:#06x} missing from {:?}", found.entries());
        }
        assert!(
            exif::Reader::new().read_raw(found.tiff_block()).is_ok(),
            "the block built from a real raw does not parse"
        );
    }

    #[test]
    fn blank_and_nul_text_is_dropped_rather_than_written() {
        let mut c = CameraExif::default();
        c.push(tag::LENS_MODEL, true, Some(Value::Ascii("   ".into())));
        c.push(tag::MAKE, false, Some(Value::Ascii("Leica\0Camera".into())));
        assert!(c.is_empty(), "{:?}", c.entries);
    }
}
