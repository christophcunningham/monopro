//! Decode -> scene, for a CFA sensor read as a monochrome sensor.
//!
//! The app reads the CFA mosaic. Nothing here may reintroduce a
//! demosaic-then-convert path -- that is the failure the rewrite exists to escape.
//!
//! Invariants this crate enforces, each verified against real files in `docs/corpus.md`:
//!
//! - CFA colour is looked up in ABSOLUTE sensor coordinates, never crop-relative.
//!   The Fuji GFX 100S and Canon 400D both report an odd crop top, which flips the
//!   Bayer phase inside the crop.
//! - Black subtraction and white normalisation happen once, here. Nothing
//!   downstream knows the black level.
//! - `SceneImage` is f32 and unclamped above 1.0. Clamping discards real headroom in
//!   the gain-equalised red and blue photosites.
//! - The per-channel gains are photosite equalisation, not white balance.

pub mod atomic_file;
pub mod camera_exif;
pub mod colour;
pub mod composition;
pub mod curve;
pub mod demosaic;
pub mod display;
pub mod dodgeburn;
pub mod frame;
pub mod geometry;
pub mod grain;
pub mod icc;
pub mod okhsl;
pub mod output;
pub mod params;
pub mod preview;
pub mod resample;
pub mod scene;
pub mod sensor;
pub mod sharpen;
pub mod sidecar;
pub mod toning;
pub mod zone;

pub use colour::Primaries;
pub use composition::{
    CompositionParams, Frame, IRect, KeystoneCrop, KeystoneMode, KeystoneParams, Orientation,
    Point, Ratio, Rect,
};
pub use curve::{Curve, CurveInstance, CurveStack};
pub use dodgeburn::{Dab, DodgeBurnParams, Gesture, Instance, Sign, ZoneMask};
pub use frame::{FrameParams, Layout as ExportFrameLayout, LayoutError as FrameLayoutError};
pub use geometry::{CfaColor, CfaGeometry, Dims};
pub use grain::{GrainParams, GrainResult};
pub use output::{Axis, OutputParams, Resize, Unit};
pub use params::{
    AgxParams, ContrastMaskParams, Dirty, DisplayParams, ExposureParams, History, LuminanceParams,
    Params, ToneMap,
};
pub use resample::{Filter, resample};
pub use scene::{
    DecodeOptions, DemosaicAlgo, LumaImage, Sampling, SceneImage, Weighting, decode,
    derive_luminance,
};
pub use sensor::{Metadata, SensorImage};
pub use toning::{Applied, Process, ToningParams, Treatment};
pub use zone::{Basis, Proxy};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // A distinct variant for the missing-file case, because rawler wraps a plain
    // file-not-found as "Failed to decode image, possibly corrupt image", which
    // sends you looking for a decode bug when the path is simply wrong.
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("raw decode failed: {0}")]
    Decode(String),
    #[error("{path} is not a CFA image ({what}) -- monochrome sensors are not yet handled")]
    NotCfa { path: String, what: String },
    #[error("unsupported CFA pattern {name} ({w}x{h}); only 2x2 Bayer is handled")]
    UnsupportedCfa { name: String, w: usize, h: usize },
}

pub type Result<T> = std::result::Result<T, Error>;
