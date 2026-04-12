//! `bioformats-rs` — direct Rust port of selected Bio-Formats readers.
//!
//! Supported formats in this crate:
//! - TIFF / BigTIFF
//! - Nikon ND2
//! - Zeiss CZI
//!
//! ```no_run
//! use bioformats_rs::ImageReader;
//! use std::path::Path;
//!
//! let mut reader = ImageReader::open(Path::new("image.tif")).unwrap();
//! let plane = reader.open_bytes(0).unwrap();
//! assert!(!plane.is_empty());
//! ```

pub mod common;
pub mod error;
pub mod formats;
pub mod metadata;
pub mod pixel;
pub mod reader;
pub mod registry;
pub mod tiff;

pub use error::{BioFormatsError, Result};
pub use metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
pub use pixel::PixelType;
pub use reader::FormatReader;
pub use registry::ImageReader;
pub use tiff::TiffReader;
