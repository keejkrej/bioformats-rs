use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::peek_header;
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

/// Auto-detecting image reader for the supported MVP formats.
pub struct ImageReader {
    inner: Option<Box<dyn FormatReader>>,
    current_path: Option<PathBuf>,
}

fn all_readers() -> Vec<Box<dyn FormatReader>> {
    vec![
        Box::new(crate::tiff::TiffReader::new()),
        Box::new(crate::formats::czi::CziReader::new()),
        Box::new(crate::formats::nd2::Nd2Reader::new()),
    ]
}

impl Default for ImageReader {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageReader {
    pub fn new() -> Self {
        Self {
            inner: None,
            current_path: None,
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut reader = Self::new();
        reader.set_id(path)?;
        Ok(reader)
    }

    fn inner(&self) -> Result<&(dyn FormatReader + '_)> {
        match self.inner.as_ref() {
            Some(inner) => Ok(inner.as_ref()),
            None => Err(BioFormatsError::NotInitialized),
        }
    }

    fn inner_mut(&mut self) -> Result<&mut (dyn FormatReader + '_)> {
        match self.inner.as_mut() {
            Some(inner) => Ok(inner.as_mut()),
            None => Err(BioFormatsError::NotInitialized),
        }
    }

    pub fn metadata(&self) -> &ImageMetadata {
        self.inner()
            .expect("ImageReader not initialized")
            .metadata()
    }

    pub fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes(plane_index)
    }

    pub fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes_region(plane_index, x, y, w, h)
    }

    pub fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_thumb_bytes(plane_index)
    }

    pub fn series_count(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .series_count()
    }

    pub fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner_mut()?.set_series(series)
    }

    pub fn series(&self) -> usize {
        self.inner().expect("ImageReader not initialized").series()
    }

    pub fn resolution_count(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .resolution_count()
    }

    pub fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner_mut()?.set_resolution(level)
    }

    pub fn resolution(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .resolution()
    }

    pub fn close(&mut self) -> Result<()> {
        if let Some(inner) = self.inner.as_mut() {
            inner.close()?;
        }
        self.inner = None;
        self.current_path = None;
        Ok(())
    }
}

impl FormatReader for ImageReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        all_readers()
            .into_iter()
            .any(|reader| reader.is_this_type_by_name(path))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        all_readers()
            .into_iter()
            .any(|reader| reader.is_this_type_by_bytes(header))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let header = peek_header(path, 512).unwrap_or_default();

        for mut reader in all_readers() {
            if reader.is_this_type_by_bytes(&header) {
                reader.set_id(path)?;
                self.inner = Some(reader);
                self.current_path = Some(path.to_path_buf());
                return Ok(());
            }
        }

        for mut reader in all_readers() {
            if reader.is_this_type_by_name(path) {
                reader.set_id(path)?;
                self.inner = Some(reader);
                self.current_path = Some(path.to_path_buf());
                return Ok(());
            }
        }

        Err(BioFormatsError::UnsupportedFormat(
            path.display().to_string(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        ImageReader::close(self)
    }

    fn series_count(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner_mut()?.set_series(series)
    }

    fn series(&self) -> usize {
        self.inner().expect("ImageReader not initialized").series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner()
            .expect("ImageReader not initialized")
            .metadata()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner_mut()?.set_resolution(level)
    }

    fn resolution(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .resolution()
    }
}
