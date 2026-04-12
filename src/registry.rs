use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::peek_header;
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

/// Auto-detecting image reader for the supported MVP formats.
pub struct ImageReader {
    inner: Box<dyn FormatReader>,
}

fn all_readers() -> Vec<Box<dyn FormatReader>> {
    vec![
        Box::new(crate::tiff::TiffReader::new()),
        Box::new(crate::formats::czi::CziReader::new()),
        Box::new(crate::formats::nd2::Nd2Reader::new()),
    ]
}

impl ImageReader {
    pub fn open(path: &Path) -> Result<Self> {
        let header = peek_header(path, 512).unwrap_or_default();

        for mut reader in all_readers() {
            if reader.is_this_type_by_bytes(&header) {
                reader.set_id(path)?;
                return Ok(Self { inner: reader });
            }
        }

        for mut reader in all_readers() {
            if reader.is_this_type_by_name(path) {
                reader.set_id(path)?;
                return Ok(Self { inner: reader });
            }
        }

        Err(BioFormatsError::UnsupportedFormat(
            path.display().to_string(),
        ))
    }

    pub fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    pub fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }

    pub fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    pub fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    pub fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    pub fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)
    }

    pub fn series(&self) -> usize {
        self.inner.series()
    }

    pub fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    pub fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }

    pub fn resolution(&self) -> usize {
        self.inner.resolution()
    }

    pub fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
}
