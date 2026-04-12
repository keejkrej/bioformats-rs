use std::path::Path;

use crate::common::error::Result;
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::registry::ImageReader;

/// Base delegating wrapper around another reader.
pub struct ReaderWrapper {
    reader: Box<dyn FormatReader>,
}

impl ReaderWrapper {
    pub fn new<R: FormatReader + 'static>(reader: R) -> Self {
        Self {
            reader: Box::new(reader),
        }
    }

    pub fn image_reader() -> Self {
        Self::new(ImageReader::new())
    }

    pub fn with_box(reader: Box<dyn FormatReader>) -> Self {
        Self { reader }
    }

    pub fn inner(&self) -> &dyn FormatReader {
        self.reader.as_ref()
    }

    pub fn inner_mut(&mut self) -> &mut dyn FormatReader {
        self.reader.as_mut()
    }

    pub fn unwrap(self) -> Box<dyn FormatReader> {
        self.reader
    }
}

impl FormatReader for ReaderWrapper {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.reader.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.reader.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reader.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.reader.close()
    }

    fn series_count(&self) -> usize {
        self.reader.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.reader.set_series(series)
    }

    fn series(&self) -> usize {
        self.reader.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.reader.metadata()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.reader.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.reader.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.reader.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.reader.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.reader.set_resolution(level)
    }

    fn resolution(&self) -> usize {
        self.reader.resolution()
    }
}
