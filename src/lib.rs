//! `bioformats-rs` — native Rust readers ported against Java Bio-Formats.
//!
//! Supported formats in this crate:
//! - TIFF / BigTIFF
//! - Nikon ND2
//! - Zeiss CZI
//! - NRRD
//! - MRC
//! - Hamamatsu DCIMG
//!
//! ```no_run
//! use bioformats_rs::{open, PlaneCoordinates, ReadRequest};
//!
//! let dataset = open("image.tif").unwrap();
//! let plane = dataset
//!     .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)))
//!     .unwrap();
//! assert!(!plane.bytes().is_empty());
//! ```

pub mod common;
pub mod dataset;
pub mod error;
pub mod formats;
pub mod metadata;
pub mod pattern;
pub mod pixel;
pub mod reader;
pub mod registry;
pub mod snapshot;
pub mod source;
pub mod tiff;
pub mod wrappers;

pub use dataset::{
    open, open_source, Dataset, PixelLayout, Plane, PlaneCoordinates, PlaneInfo, ReadRequest, Rect,
    Region, Resolution, Series,
};
pub use error::{BioFormatsError, Result};
pub use metadata::{
    ChannelMetadata, DimensionOrder, ImageMetadata, LookupTable, MetadataValue, PlaneMetadata,
};
pub use pattern::{AxisGuesser, AxisType, FilePattern, FilePatternBlock};
pub use pixel::PixelType;
pub use reader::FormatReader;
pub use registry::{FormatId, ImageReader, SUPPORTED_FORMATS};
pub use snapshot::ReaderSnapshot;
pub use source::{
    CompanionReference, CompanionResolver, RandomAccessSource, SourceError, SourceId, SourceInfo,
    SourceInput, SourceResult,
};
pub use tiff::TiffReader;
pub use wrappers::{
    ChannelFiller, ChannelMerger, ChannelSeparator, DimensionSwapper, FileStitcher,
    FileStitcherSnapshot, Memoizer, MinMaxCalculator, MinMaxStore, ReaderWrapper,
};
