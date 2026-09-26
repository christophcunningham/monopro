//! Stage 1: Sensor. The u16 CFA mosaic exactly as the file holds it, plus the
//! geometry and levels needed to interpret it. Nothing has been done to the data.

use crate::geometry::{CfaColor, CfaGeometry, Dims};
use crate::{Error, Result};
use rawler::decoders::RawDecodeParams;
use rawler::rawimage::{RawImageData, RawPhotometricInterpretation};
use rawler::rawsource::RawSource;

/// Shot metadata, read once at load.
///
/// Plumbing only at this point -- nothing in the pipeline consumes it. It exists
/// because the exposure module will need `exposure_bias` for "compensate camera
/// exposure", and the info panel needs the rest; reading it later would mean either
/// a second pass over the file or threading a decoder handle out of `load`.
///
/// Everything is optional because it genuinely is: the corpus has files missing
/// aperture (manual lenses) and files missing lens identification entirely.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub lens: Option<String>,
    pub iso: Option<u32>,
    /// Seconds. 1/500 is stored as 0.002, not as "500".
    pub shutter: Option<f32>,
    /// f-number.
    pub aperture: Option<f32>,
    /// Millimetres, as recorded (not 35mm-equivalent).
    pub focal_len: Option<f32>,
    /// EV. The exposure module's "compensate camera exposure" cancels this.
    pub exposure_bias: Option<f32>,
    pub date_time: Option<String>,
    /// How the body metered, as a word rather than as the EXIF code.
    ///
    /// `&'static str` because the set is closed by the spec: there is no metering
    /// mode a camera can invent. Mapped here, at the one place the tag enters the
    /// program, for the same reason [`measured`] filters here — a display site that
    /// has to remember what `2` means is a display site that will get it wrong.
    pub metering: Option<&'static str>,
    /// `Auto` or `Manual`. The EXIF tag has exactly these two values.
    ///
    /// **This is the camera's white balance, and this app does not use it as one.**
    /// Gain equalisation is the hinge of the design and runs on `wb_coeffs`
    /// regardless of what the body was set to. The row is shot data, like the
    /// metering mode beside it — not a control, and not a claim about the pipeline.
    pub white_balance: Option<&'static str>,
    /// Which way up the camera was held.
    ///
    /// **Not optional, unlike everything above it**, and the difference is real: an
    /// absent aperture is a fact the file does not record, while an absent
    /// orientation tag means upright — which is a value, not a gap. Making it an
    /// `Option` would push that decision out to every reader, and the app went eight
    /// months displaying two corpus files sideways because this field did not
    /// exist at all.
    pub orientation: crate::composition::Orientation,
}

/// EXIF `ApertureValue` is in **APEX**, not f-stops: `FNumber = 2^(Av/2)`.
///
/// Av 4.0 is f/4.0 and Av 1.0 is f/1.41, which is why the tag looks like an f-number
/// often enough to be mistaken for one.
fn apex_to_fnumber(av: f32) -> f32 {
    2.0f32.powf(av / 2.0)
}

/// EXIF `MeteringMode` (0x9207) as the word a photographer uses.
///
/// **`0` is "Unknown" in the spec and is treated as absent**, which is the same
/// distinction [`measured`] draws for the numeric tags: a body that wrote `0` did not
/// fail to meter, it declined to say how, and "Unknown" in a readout claims more than
/// that. `255` — "Other" — is a real answer and is kept.
///
/// `Center-wt` rather than the spec's "Center-weighted average": the Info panel's
/// value column is narrow and the full phrase wraps to two lines. This is the
/// prototype's abbreviation, and it is the one on the camera's own dial.
fn metering_label(code: u16) -> Option<&'static str> {
    Some(match code {
        1 => "Average",
        2 => "Center-wt",
        3 => "Spot",
        4 => "Multi-spot",
        5 => "Pattern",
        6 => "Partial",
        255 => "Other",
        _ => return None,
    })
}

/// EXIF `WhiteBalance` (0xA403). Two values, and that is the whole tag.
fn white_balance_label(code: u16) -> Option<&'static str> {
    match code {
        0 => Some("Auto"),
        1 => Some("Manual"),
        _ => None,
    }
}

/// A value the camera actually measured, or nothing.
///
/// **Present is not the same as meaningful.** A Fuji in the corpus writes `FNumber` as
/// `0/0`, which reads back as `NaN`; the Leica writes `FocalLength: 0` for an uncoded
/// lens. Both are "the body could not interrogate this", and printing them gave
/// `f/NaN · 0 mm` in the chrome.
///
/// Filtered **here**, at the one place the value enters the program, rather than at each
/// site that displays it — which was three places that all had to remember, and is how
/// the aperture fallback above could have quietly reintroduced `f/NaN`.
fn measured(v: f32) -> Option<f32> {
    (v.is_finite() && v > 0.0).then_some(v)
}

impl Metadata {
    /// Shutter as a photographer reads it: `1/500` below a second, `2.5"` above.
    pub fn shutter_label(&self) -> Option<String> {
        let s = self.shutter?;
        Some(if s >= 1.0 {
            format!("{s:.1}\"")
        } else if s > 0.0 {
            format!("1/{:.0}", 1.0 / s)
        } else {
            "—".into()
        })
    }
}

pub struct SensorImage {
    pub data: Vec<u16>,
    pub geom: CfaGeometry,
    /// Black level, indexed by position in a repeating tile -- NOT by CFA colour.
    /// rawler reports it as a `width x height x cpp` repeat pattern; the Fuji GFX
    /// reports [254, 254, 254, 255] over a 2x2 tile, which aligns with the Bayer
    /// tile but is conceptually a positional pattern.
    pub black: BlackPattern,
    /// Saturation point per CFA colour. Usually one scalar broadcast to all three.
    pub white: [f32; 3],
    /// Camera white balance coefficients, R/G/B, green normalised to 1.0.
    /// See `Gains` -- these are used as photosite equalisation, not white balance.
    pub wb_coeffs: [f32; 3],
    pub camera: String,
    pub meta: Metadata,
}

#[derive(Debug, Clone)]
pub struct BlackPattern {
    pub levels: Vec<f32>,
    pub w: usize,
    pub h: usize,
}

impl BlackPattern {
    #[inline]
    pub fn at(&self, y: usize, x: usize) -> f32 {
        self.levels[(y % self.h) * self.w + (x % self.w)]
    }
}

impl SensorImage {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        // Check existence first so a wrong path reports as a wrong path. rawler
        // surfaces a missing file as a generic "possibly corrupt image", which is
        // actively misleading.
        if !path.exists() {
            return Err(Error::NotFound(path.display().to_string()));
        }
        // Not `rawler::decode_file`: that returns only the RawImage, and EXIF lives
        // on a separate `raw_metadata` call. Going through the decoder directly
        // means the file is read and parsed once and serves both, rather than
        // opening it twice to fill in the info panel.
        let src = RawSource::new(path).map_err(|e| Error::Decode(e.to_string()))?;
        let decoder = rawler::get_decoder(&src).map_err(|e| Error::Decode(e.to_string()))?;
        let params = RawDecodeParams::default();
        let img = decoder
            .raw_image(&src, &params, false)
            .map_err(|e| Error::Decode(e.to_string()))?;
        // Metadata is best-effort: a file whose pixels decode but whose EXIF does
        // not is still perfectly usable, so this must never fail the load.
        let meta = decoder
            .raw_metadata(&src, &params)
            .map(|m| metadata_from(&m))
            .unwrap_or_default();

        let cfg = match &img.photometric {
            RawPhotometricInterpretation::Cfa(c) => c,
            other => {
                return Err(Error::NotCfa {
                    path: path.display().to_string(),
                    what: format!("{other:?}"),
                });
            }
        };
        if cfg.cfa.width != 2 || cfg.cfa.height != 2 {
            return Err(Error::UnsupportedCfa {
                name: cfg.cfa.name.clone(),
                w: cfg.cfa.width,
                h: cfg.cfa.height,
            });
        }

        let data = match &img.data {
            RawImageData::Integer(d) => d.clone(),
            RawImageData::Float(_) => {
                return Err(Error::Decode("f32 raw data not handled yet".into()));
            }
        };

        let mut pattern = [[CfaColor::Green; 2]; 2];
        for (r, row) in pattern.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = CfaColor::from_index(cfg.cfa.color_at(r, c)).ok_or_else(|| {
                    Error::UnsupportedCfa {
                        name: cfg.cfa.name.clone(),
                        w: 2,
                        h: 2,
                    }
                })?;
            }
        }
        // Every colour index was legal on its own; the quad still has to be Bayer.
        // See `CfaColor::is_bayer_quad` for what downstream would silently assume.
        if !CfaColor::is_bayer_quad(&pattern) {
            return Err(Error::UnsupportedCfa {
                name: cfg.cfa.name.clone(),
                w: 2,
                h: 2,
            });
        }

        let full = Dims {
            w: img.width,
            h: img.height,
        };
        let (cx, cy, cw, ch) = match img.crop_area {
            Some(r) => (r.p.x, r.p.y, r.d.w, r.d.h),
            None => (0, 0, img.width, img.height),
        };
        let geom = CfaGeometry::new(img.width, full, cx, cy, cw, ch, pattern);

        let bl = &img.blacklevel;
        let black = BlackPattern {
            levels: (0..bl.height)
                .flat_map(|r| (0..bl.width).map(move |c| (r, c)))
                .map(|(r, c)| bl.levels[(r * bl.width + c) * bl.cpp].as_f32())
                .collect(),
            w: bl.width,
            h: bl.height,
        };

        let wl = img.whitelevel.as_vec();
        let white = [
            *wl.first().unwrap_or(&65535.0),
            *wl.get(1).unwrap_or(&wl[0]),
            *wl.get(2).unwrap_or(&wl[0]),
        ];

        // wb_coeffs is RGBE; index 3 is NaN on every Bayer camera in the corpus.
        // A NaN reaching the gains would poison the whole frame, so it is dropped
        // here rather than guarded downstream.
        let raw = [img.wb_coeffs[0], img.wb_coeffs[1], img.wb_coeffs[2]];
        let wb_coeffs = if raw.iter().all(|v| v.is_finite() && *v > 0.0) {
            raw
        } else {
            [1.0, 1.0, 1.0]
        };

        Ok(Self {
            data,
            geom,
            black,
            white,
            wb_coeffs,
            camera: format!("{} {}", img.clean_make, img.clean_model),
            meta,
        })
    }
}

/// Photosite equalisation factors.
///
/// THESE ARE NOT WHITE BALANCE. They are numerically the camera's white balance
/// coefficients, but they are being used to equalise photosite response so the CFA
/// pattern becomes invisible in a monochrome render. This is the operation that
/// turns a CFA sensor into a monochrome sensor. Do not rename it to `white_balance`
/// on the grounds that it looks like white balance.
#[derive(Debug, Clone, Copy)]
pub struct Gains(pub [f32; 3]);

impl Gains {
    /// Normalise so the LEAST amplified channel is 1.0.
    ///
    /// The alternative -- dividing by the largest coefficient so everything lands
    /// at or below 1.0 -- was measured against the corpus and costs 0.87 to 1.73
    /// stops of range depending on the camera, for nothing. The least-amplified
    /// channel (green, on every Bayer camera measured) saturates first, so its
    /// saturation point IS the monochrome white point. The more-amplified channels
    /// legitimately read above 1.0 until they clip in their own turn.
    ///
    /// Measured consequence: scene values reach 3.52 on the Leica M10-R, 3.22 on
    /// the Sony RX100M4, 2.11 on the Canon EOS R. The headroom is `max/min` of the
    /// coefficients, roughly 1 to 2 stops. Nothing downstream may clamp it.
    pub fn equalizing(wb_coeffs: [f32; 3]) -> Self {
        let min = wb_coeffs.iter().cloned().fold(f32::INFINITY, f32::min);
        Self([wb_coeffs[0] / min, wb_coeffs[1] / min, wb_coeffs[2] / min])
    }

    #[inline]
    pub fn for_color(&self, c: CfaColor) -> f32 {
        self.0[c as usize]
    }

    /// Highest value the gain-equalised scene stage can produce.
    pub fn headroom(&self) -> f32 {
        self.0.iter().cloned().fold(0.0f32, f32::max)
    }
}

/// Build [`Metadata`] from what rawler parsed out of the file.
///
/// Split out of [`SensorImage::load`] so the EXIF panel can have the tags without
/// decoding a single pixel — `raw_metadata` parses IFDs, where `raw_image` unpacks
/// the sensor. Every tag is still interpreted in exactly one place, which is the
/// property the aperture fix was written to protect.
fn metadata_from(m: &rawler::decoders::RawMetadata) -> Metadata {
    let e = &m.exif;
    Metadata {
        lens: m
            .lens
            .as_ref()
            .map(|l| l.lens_model.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| e.lens_model.clone()),
        // `iso_speed_ratings` is the u16 tag and saturates at 65535 on
        // high-ISO bodies; `iso_speed` is the u32 replacement. Prefer
        // the wider one when the camera wrote it.
        iso: e.iso_speed.or(e.iso_speed_ratings.map(u32::from)),
        shutter: e.exposure_time.map(|r| r.as_f32()),
        // **`FNumber` is not the only place an aperture lives.** The Leica
        // M10-R writes no `FNumber` (0x829D) in ExifIFD at all; it records
        // `ApertureValue` (0x9202) there, plus an `FNumber` in its own
        // MakerNote and in XMP. Reading only the first tag reported `—` for
        // an aperture the file states plainly — which looked like "a manual
        // lens cannot be interrogated" and was really the wrong tag.
        //
        // `FNumber` stays preferred where both exist, because they can
        // disagree: the Canon EOS R in the corpus writes `FNumber 1.4` and
        // an `ApertureValue` of 1.4142, and 1.4 is the number the camera
        // meant.
        aperture: e
            .fnumber
            .map(|r| r.as_f32())
            .and_then(measured)
            .or_else(|| e.aperture_value.map(|r| apex_to_fnumber(r.as_f32())))
            .and_then(measured),
        // Genuinely absent on an uncoded lens, and left that way: this file
        // records `FocalLength: 0.0`. See `measured`.
        focal_len: e.focal_length.map(|r| r.as_f32()).and_then(measured),
        // SRational has no `as_f32` (unlike Rational), and a zero
        // denominator is how some bodies write "no bias".
        exposure_bias: e
            .exposure_bias
            .filter(|r| r.d != 0)
            .map(|r| r.n as f32 / r.d as f32),
        date_time: e
            .date_time_original
            .clone()
            .or_else(|| e.create_date.clone()),
        metering: e.metering_mode.and_then(metering_label),
        white_balance: e.white_balance.and_then(white_balance_label),
        // Read here rather than at the render, so the tag is
        // interpreted in exactly one place — which is what the
        // aperture fix should have suggested and what stopped
        // `f/NaN` reaching three separate readouts.
        orientation: e
            .orientation
            .map(crate::composition::Orientation::from_exif)
            .unwrap_or_default(),
    }
}

/// The camera's name and what it recorded, **without decoding a pixel**.
///
/// [`SensorImage::load`] gets the same metadata, but it unpacks the mosaic on the way
/// — hundreds of milliseconds and hundreds of megabytes to answer "what lens was
/// this". A panel that follows a selection around a grid cannot pay that.
///
/// It is not free either: `RawSource::new` mmaps with `populate()`, so this still
/// faults the whole file through the page cache. Cheap enough for one selected frame,
/// far too expensive per tile — the same line [`capture_time`] draws.
pub fn probe(path: &std::path::Path) -> Option<(String, Metadata)> {
    let src = RawSource::new(path).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let meta = decoder
        .raw_metadata(&src, &RawDecodeParams::default())
        .ok()?;
    let camera = format!("{} {}", meta.make, meta.model).trim().to_owned();
    Some((camera, metadata_from(&meta)))
}

/// The same shot facts, for a file rawler will not open.
///
/// **A second reader, not a second model.** Lightbox lists JPEGs, TIFFs and PNGs
/// beside raws, and every camera row in its Metadata pane came from [`probe`] — which
/// is rawler and therefore raw-only, so an ordinary picture showed its dimensions and
/// its file size and stopped. The rows are the same rows and the [`Metadata`] is the
/// same struct; only the source of the bytes differs.
///
/// It returns the camera as `"{Make} {Model}"` exactly as [`probe`] does, so the panel
/// cannot tell which reader answered — which is the point. `None` means the file
/// carries no EXIF at all, and that is a fact about the file rather than a failure:
/// a rendered export or a picture off the web often has none, and the panel shows the
/// same blank rows it shows for a tag a camera did not write.
///
/// Cheap in the way the panel needs: `read_from_container` reads the header and stops,
/// so nothing is decoded and a multi-hundred-megabyte TIFF costs no more than a JPEG.
pub fn probe_rendered(path: &std::path::Path) -> Option<(String, Metadata)> {
    use exif::{In, Tag, Value};

    fn ascii(exif: &exif::Exif, tag: Tag) -> Option<String> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            Value::Ascii(parts) => {
                let text = parts
                    .iter()
                    .map(|part| String::from_utf8_lossy(part))
                    .collect::<Vec<_>>()
                    .join(" ");
                let text = text.trim().to_owned();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        }
    }

    /// Rationals only. A camera writes these as a fraction and the denominator can
    /// be zero — 1/0 is how some bodies say "not recorded" — so this refuses rather
    /// than returning an infinity that would print as a plausible number.
    fn number(exif: &exif::Exif, tag: Tag) -> Option<f32> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            Value::Rational(r) => r.first().filter(|r| r.denom != 0).map(|r| r.to_f32()),
            Value::SRational(r) => r.first().filter(|r| r.denom != 0).map(|r| r.to_f32()),
            _ => None,
        }
    }

    fn integer(exif: &exif::Exif, tag: Tag) -> Option<u32> {
        match &exif.get_field(tag, In::PRIMARY)?.value {
            Value::Short(v) => v.first().map(|v| u32::from(*v)),
            Value::Long(v) => v.first().copied(),
            _ => None,
        }
    }

    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(&file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;

    let camera = [Tag::Make, Tag::Model]
        .iter()
        .filter_map(|tag| ascii(&exif, *tag))
        .collect::<Vec<_>>()
        .join(" ");

    let metadata = Metadata {
        lens: ascii(&exif, Tag::LensModel),
        // `PhotographicSensitivity` is EXIF 2.3's name for the tag rawler calls
        // `iso_speed_ratings`. The wider `StandardOutputSensitivity` is preferred
        // where a body wrote both, for the reason `probe` prefers `iso_speed`.
        iso: integer(&exif, Tag::StandardOutputSensitivity)
            .or_else(|| integer(&exif, Tag::PhotographicSensitivity)),
        shutter: number(&exif, Tag::ExposureTime),
        // The same two-tag fallback `probe` documents: a body that writes no
        // `FNumber` may still record `ApertureValue` in APEX.
        aperture: number(&exif, Tag::FNumber)
            .and_then(measured)
            .or_else(|| number(&exif, Tag::ApertureValue).map(apex_to_fnumber))
            .and_then(measured),
        focal_len: number(&exif, Tag::FocalLength).and_then(measured),
        exposure_bias: number(&exif, Tag::ExposureBiasValue),
        date_time: ascii(&exif, Tag::DateTimeOriginal).or_else(|| ascii(&exif, Tag::DateTime)),
        metering: integer(&exif, Tag::MeteringMode)
            .and_then(|code| u16::try_from(code).ok())
            .and_then(metering_label),
        white_balance: integer(&exif, Tag::WhiteBalance)
            .and_then(|code| u16::try_from(code).ok())
            .and_then(white_balance_label),
        orientation: integer(&exif, Tag::Orientation)
            .and_then(|code| u16::try_from(code).ok())
            .map(crate::composition::Orientation::from_exif)
            .unwrap_or_default(),
    };

    Some((camera, metadata))
}

/// A raw's photosite dimensions, without unpacking a single one.
///
/// **`dummy: true` is the whole trick.** rawler's `raw_image` takes a flag that builds
/// the image *structure* — geometry, crop, components per pixel — and leaves the pixel
/// buffer uninitialised. So the header work happens and the unpack does not, which is
/// the difference between a panel that can report a size and one that cannot afford to.
///
/// Reports the **cropped** area where the file declares one, because that is the
/// picture; the uncropped sensor includes masked borders the photographer never saw.
/// Falls back to the full frame when there is no crop.
///
/// Not free, for the reason [`probe`] is not: `RawSource::new` still faults the file
/// through the page cache. One selected frame, never per tile.
pub fn raw_dimensions(path: &std::path::Path) -> Option<(usize, usize)> {
    let src = RawSource::new(path).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let raw = decoder
        .raw_image(&src, &RawDecodeParams::default(), true)
        .ok()?;
    let (w, h) = raw
        .crop_area
        .map(|c| (c.width(), c.height()))
        .filter(|(w, h)| *w > 0 && *h > 0)
        .unwrap_or((raw.width, raw.height));
    (w > 0 && h > 0).then_some((w, h))
}

/// The file's own XMP packet, if it carries one, and the EXIF capture time as
/// ISO 8601 — **from one open of the file**, since the open is the expensive part
/// (see [`capture_time`]) and the Metadata pane wants both.
///
/// **The packet is where IPTC lives now.** The legacy IPTC-IIM block is a binary
/// record almost nothing writes any more; IPTC Core is XMP, in the same `dc:` and
/// `photoshop:` namespaces this app's own sidecar already speaks. So the packet comes
/// back as text and [`crate::sidecar::from_xml`] reads it — the same parser, which is
/// what stops this app having two ideas of what a creator field is.
///
/// No packet for most raws: rawler defaults `xpacket` to nothing and only some
/// decoders override it. A file with no packet is not an error; it is a file nobody
/// has captioned. A picture rawler does not decode has no packet from here, but can
/// still have a date: that comes from [`probe_rendered`], the reader the pane already
/// uses for it.
pub fn xmp_and_capture_date(path: &std::path::Path) -> (Option<String>, Option<String>) {
    if let Ok(src) = RawSource::new(path)
        && let Ok(decoder) = rawler::get_decoder(&src)
    {
        let params = RawDecodeParams::default();
        let packet = decoder
            .xpacket(&src, &params)
            .ok()
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok());
        let taken = decoder.raw_metadata(&src, &params).ok().and_then(|meta| {
            let e = meta.exif;
            let when = e.date_time_original.or(e.create_date)?;
            crate::sidecar::exif_date_to_iso(&when, e.offset_time_original.as_deref())
        });
        return (packet, taken);
    }
    let taken = probe_rendered(path)
        .and_then(|(_, meta)| meta.date_time)
        .and_then(|when| crate::sidecar::exif_date_to_iso(&when, None));
    (None, taken)
}

/// When the frame was taken, from EXIF `DateTimeOriginal`.
///
/// **This costs a full read of the file**, and the caller has to know that:
/// `RawSource::new` mmaps with `populate()` and `WillNeed`, so asking one raw for its
/// capture date faults every byte of it through the page cache. Over a folder that is
/// the whole folder. It is separate from [`SensorImage`] for exactly that reason —
/// there is no way to get this without paying, so the paying should be visible at the
/// call site rather than hidden in a field.
///
/// The tag's format is `YYYY:MM:DD HH:MM:SS` with no timezone, which is what every
/// body writes. **Treated as UTC**, which is wrong by up to a day's offset and does
/// not matter: this is only ever compared against other frames' values, so a constant
/// error orders identically. Inventing a timezone the file does not carry would be
/// the dishonest alternative.
pub fn capture_time(path: &std::path::Path) -> Option<std::time::SystemTime> {
    let src = RawSource::new(path).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let meta = decoder
        .raw_metadata(&src, &RawDecodeParams::default())
        .ok()?;
    parse_exif_datetime(meta.exif.date_time_original.as_deref()?)
}

/// `YYYY:MM:DD HH:MM:SS` to an instant, as UTC. See [`capture_time`].
///
/// Split out so it can be tested without a raw file, and written by hand because it
/// is one conversion that only has to be monotonic — a date library for this would be
/// a dependency carrying a timezone database to answer a question with no timezone in
/// it.
fn parse_exif_datetime(s: &str) -> Option<std::time::SystemTime> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, mi, sec) = (
        n(11, 13).unwrap_or(0),
        n(14, 16).unwrap_or(0),
        n(17, 19).unwrap_or(0),
    );

    // Days from civil, Howard Hinnant's algorithm: March-based years make the leap
    // day the last of the year and the month-length table collapse to one expression.
    let (y2, mo2) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (mo2 - 3) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    u64::try_from(secs)
        .ok()
        .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_dates_order_the_way_the_clock_does() {
        let t = |s: &str| parse_exif_datetime(s).expect(s);
        // The epoch itself, as the one value with an independently known answer.
        assert_eq!(t("1970:01:01 00:00:00"), std::time::UNIX_EPOCH);
        assert_eq!(
            t("1970:01:02 00:00:01"),
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(86401)
        );

        // Ordering is the only property the sort depends on, so it is the one
        // checked across the awkward boundaries: leap day, year end, midnight.
        let rising = [
            "2024:02:28 23:59:59",
            "2024:02:29 00:00:00", // a leap day that exists
            "2024:03:01 00:00:00",
            "2024:12:31 23:59:59",
            "2025:01:01 00:00:00",
            "2026:03:11 09:15:00",
        ];
        for pair in rising.windows(2) {
            assert!(
                t(pair[0]) < t(pair[1]),
                "{} did not precede {}",
                pair[0],
                pair[1]
            );
        }

        // Rubbish is `None` rather than a wrong instant — an unparseable date sorts
        // with the files that have none, which is where a file nobody can date belongs.
        assert!(parse_exif_datetime("").is_none());
        assert!(parse_exif_datetime("not a date at all").is_none());
        assert!(
            parse_exif_datetime("2024:13:01 00:00:00").is_none(),
            "month 13"
        );
        assert!(
            parse_exif_datetime("2024:00:10 00:00:00").is_none(),
            "month 0"
        );
    }

    #[test]
    fn the_two_readers_agree_about_one_camera() {
        // **The fixture is the GFX100S's own JPEG**, extracted from the corpus RAF with
        // `exiftool -b -PreviewImage`, so these are a camera's bytes rather than bytes
        // written to make a parser pass. That is what makes the cross-check worth
        // anything: `probe` reads the RAF through rawler and `probe_rendered` reads the
        // preview through a completely separate EXIF implementation, and where the two
        // describe the same exposure they have to agree.
        //
        // Skips when the corpus is not there, like every other test that reads it.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let (Some((raw_camera, raw)), Some((jpeg_camera, jpeg))) = (
            probe(&dir.join("fuji-reference.RAF")),
            probe_rendered(&dir.join("fuji-reference_camera-preview.jpg")),
        ) else {
            return;
        };

        // **The one thing they do not agree about is the name, and that is not a bug
        // to fix here.** rawler resolves make and model against a camera database and
        // reports `Fujifilm GFX 100S`; EXIF holds the body's own two strings and they
        // read `FUJIFILM GFX100S`. Normalising the second toward the first would mean
        // carrying that database, and inventing a tidier name than the file contains
        // is the opposite of what this panel is for. Pinned so a future reader finds
        // the difference explained rather than assumed to be a defect.
        assert_eq!(raw_camera, "Fujifilm GFX 100S");
        assert_eq!(jpeg_camera, "FUJIFILM GFX100S");
        assert_eq!(raw.iso, jpeg.iso);
        assert_eq!(jpeg.iso, Some(500));
        assert_eq!(raw.date_time, jpeg.date_time);
        assert_eq!(raw.metering, jpeg.metering);
        let (Some(a), Some(b)) = (raw.shutter, jpeg.shutter) else {
            panic!(
                "a shutter speed went missing: {:?} vs {:?}",
                raw.shutter, jpeg.shutter
            );
        };
        assert!((a - b).abs() < 1.0e-6, "shutter disagreed: {a} vs {b}");

        // The manual lens on this body writes `FNumber` as 0/0 and a focal length of
        // zero, and `measured` is what keeps that out of the panel. Both readers have
        // to draw the line in the same place or the rows would change meaning with the
        // file format.
        assert_eq!(jpeg.aperture, None, "an unmeasured aperture came through");
        assert_eq!(
            jpeg.focal_len, None,
            "an unmeasured focal length came through"
        );
        assert_eq!(raw.aperture, jpeg.aperture);
        assert_eq!(raw.focal_len, jpeg.focal_len);
    }

    #[test]
    fn a_picture_with_no_exif_says_so_rather_than_inventing_rows() {
        // A rendered export and a picture off the web are the common non-raw cases and
        // neither carries EXIF. `None` has to mean "this file records nothing", which
        // is what lets the panel draw the same blank rows it draws for a tag a camera
        // did not write — rather than a reader failure it would have to explain.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        for name in [
            "rendered-reference.jpg",
            "Sony-WM-EX910-Zoom-logo-ig-boxedwalkman-2228826821.jpg",
        ] {
            let path = dir.join(name);
            if !path.exists() {
                continue;
            }
            assert!(
                probe_rendered(&path).is_none(),
                "{name} reported metadata it does not carry"
            );
        }
        assert!(probe_rendered(&dir.join("no-such-file.jpg")).is_none());
    }

    #[test]
    fn the_corpus_dates_itself() {
        // Whether a body writes the tag is a property of the file, so this checks the
        // corpus rather than assuming. Skips when the raws are not there.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut dated = 0;
        for e in entries.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !["dng", "raf", "nef", "cr3", "arw", "rw2", "cr2"].contains(&ext.as_str()) {
                continue;
            }
            match capture_time(&p) {
                Some(t) => {
                    dated += 1;
                    assert!(
                        t > std::time::UNIX_EPOCH,
                        "{}: dated before 1970",
                        p.display()
                    );
                }
                None => eprintln!(
                    "{}: no capture date",
                    p.file_name().unwrap().to_string_lossy()
                ),
            }
        }
        eprintln!("{dated} of the corpus carry a capture date");
    }
}

#[cfg(test)]
mod sensor_tests {
    use super::*;

    #[test]
    fn metering_zero_is_absent_rather_than_the_word_unknown() {
        // The spec calls 0 "Unknown", and a readout that prints it claims the body
        // said something when it declined to. Same distinction `measured` draws for
        // the numeric tags — and the reason both live at the read rather than at the
        // display.
        assert_eq!(metering_label(0), None);
        assert_eq!(metering_label(2), Some("Center-wt"));
        // 255 is "Other", which *is* an answer.
        assert_eq!(metering_label(255), Some("Other"));
        // Anything outside the spec is a file we cannot interpret, not a guess.
        assert_eq!(metering_label(9), None);
    }

    #[test]
    fn white_balance_has_exactly_two_values() {
        assert_eq!(white_balance_label(0), Some("Auto"));
        assert_eq!(white_balance_label(1), Some("Manual"));
        assert_eq!(white_balance_label(2), None);
    }

    #[test]
    fn apex_aperture_values_convert_to_f_stops() {
        // The identities worth pinning, because the tag looks like an f-number often
        // enough to be used as one: Av 4 really is f/4, but Av 1 is f/1.41 and Av 2 is
        // f/2, so two of the three agree by coincidence.
        for (av, f) in [
            (0.0, 1.0),
            (1.0, std::f32::consts::SQRT_2),
            (2.0, 2.0),
            (4.0, 4.0),
            (8.0, 16.0),
        ] {
            let got = apex_to_fnumber(av);
            assert!(
                (got - f).abs() < 1.0e-4,
                "Av {av} gave f/{got}, expected f/{f}"
            );
        }
    }

    #[test]
    fn the_leica_aperture_is_read_from_aperture_value() {
        // the maintainer asked why his Leica f-stop was missing. It was not: the M10-R writes no
        // `FNumber` in ExifIFD, only `ApertureValue`, and we read the first tag only.
        // Checked against the real file rather than a fixture, because the whole point
        // is which tag this body actually wrote. Skips when the corpus is absent.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws/leica-reference.dng");
        if !path.exists() {
            return;
        }
        let img = SensorImage::load(&path).expect("the corpus Leica decodes");
        assert_eq!(
            img.meta.aperture,
            Some(4.0),
            "the Leica aperture came back {:?} — ApertureValue is not being read",
            img.meta.aperture
        );
        // And the focal length is genuinely absent on this uncoded lens, so the fix
        // must not have invented one.
        assert!(
            img.meta.focal_len.is_none_or(|f| f == 0.0),
            "a focal length appeared from nowhere: {:?}",
            img.meta.focal_len
        );
    }

    #[test]
    fn the_corpus_actually_carries_metering_and_white_balance() {
        // A tag that is read but never present is indistinguishable from a tag that
        // is not read at all, and both show as `—` in the panel. This is the check
        // that the two new EXIF rows have something to say on real files rather than
        // being two more dashes.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let (mut metering, mut wb, mut read) = (0, 0, 0);
        let mut seen = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            // The corpus holds sidecars and TIFFs beside the raws.
            if p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("xmp") || x.eq_ignore_ascii_case("tif"))
            {
                continue;
            }
            let Ok(img) = SensorImage::load(&p) else {
                continue;
            };
            read += 1;
            metering += u32::from(img.meta.metering.is_some());
            wb += u32::from(img.meta.white_balance.is_some());
            seen.push(format!(
                "{}: metering {:?}, wb {:?}",
                p.file_name().unwrap_or_default().to_string_lossy(),
                img.meta.metering,
                img.meta.white_balance
            ));
        }
        if read == 0 {
            return;
        }
        assert!(
            metering > 0,
            "no corpus file reported a metering mode:\n{}",
            seen.join("\n")
        );
        assert!(
            wb > 0,
            "no corpus file reported a white balance:\n{}",
            seen.join("\n")
        );
    }

    #[test]
    fn the_two_sideways_files_in_the_corpus_report_their_rotation() {
        // The defect Composition opened with, measured against the files that had
        // it. The app ignored `exif.orientation` for months and these two
        // frames displayed on their side the whole time — a portrait shot laid out
        // landscape, which is not a missing feature but a wrong picture.
        //
        // Checked against the real files rather than a fixture, because the whole
        // question is which tag these bodies actually wrote. Skips when the corpus
        // is absent, like the aperture test above.
        use crate::composition::Orientation;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        for (name, want) in [
            ("sony-reference.ARW", Orientation::Rotate270),
            ("canon_eos_r_54.cr3", Orientation::Rotate270),
            // And a landscape frame, so a bug that reported every file as rotated
            // would not pass by agreeing with the two that are.
            ("nikon-reference.nef", Orientation::Rotate0),
            ("leica-reference.dng", Orientation::Rotate0),
        ] {
            let path = dir.join(name);
            if !path.exists() {
                continue;
            }
            let img = SensorImage::load(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(img.meta.orientation, want, "{name}");
        }
    }

    #[test]
    fn no_corpus_file_reports_an_aperture_that_is_not_one() {
        // The fallback must not regress the bodies that write `FNumber` properly, and
        // must not start reporting nonsense for the ones that write neither. The Fuji
        // RAF in the corpus writes `FNumber` as `0/0`, which is `NaN` — it reached the
        // chrome as `f/NaN` once, and `measured` is what stops it here rather than at
        // each place that prints a number.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !["dng", "raf", "nef", "cr3", "arw", "rw2", "3fr"].contains(&ext.as_str()) {
                continue;
            }
            let Ok(img) = SensorImage::load(&p) else {
                continue;
            };
            if let Some(f) = img.meta.aperture {
                assert!(
                    f.is_finite() && (0.7..=90.0).contains(&f),
                    "{}: f/{f} is not an aperture",
                    p.display()
                );
            }
        }
    }
}
