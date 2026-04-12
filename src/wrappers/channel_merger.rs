use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::wrappers::reader_wrapper::ReaderWrapper;

/// Merges non-RGB channel planes into a single multi-channel plane.
pub struct ChannelMerger {
    reader: ReaderWrapper,
    metadata: ImageMetadata,
}

impl ChannelMerger {
    pub fn new<R: FormatReader + 'static>(reader: R) -> Self {
        let wrapper = ReaderWrapper::new(reader);
        let metadata = Self::merged_metadata(wrapper.metadata());
        Self {
            reader: wrapper,
            metadata,
        }
    }

    pub fn can_merge(reader: &dyn FormatReader) -> bool {
        let size_c = reader.size_c();
        size_c > 1 && size_c <= 4 && !reader.is_rgb()
    }

    fn merged_metadata(source: &ImageMetadata) -> ImageMetadata {
        if !Self::can_merge_from_metadata(source) {
            return source.clone();
        }

        let mut metadata = source.clone();
        metadata.image_count = source.image_count / source.size_c;
        metadata.is_rgb = true;
        metadata.is_interleaved = false;
        metadata
    }

    fn can_merge_from_metadata(source: &ImageMetadata) -> bool {
        source.size_c > 1 && source.size_c <= 4 && !source.is_rgb
    }
}

impl FormatReader for ChannelMerger {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.reader.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.reader.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reader.set_id(path)?;
        self.metadata = Self::merged_metadata(self.reader.metadata());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.reader.close()
    }

    fn series_count(&self) -> usize {
        self.reader.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.reader.set_series(series)?;
        self.metadata = Self::merged_metadata(self.reader.metadata());
        Ok(())
    }

    fn series(&self) -> usize {
        self.reader.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes_region(plane_index, 0, 0, self.size_x(), self.size_y())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if !Self::can_merge(self.reader.inner()) {
            return self.reader.open_bytes_region(plane_index, x, y, w, h);
        }

        if plane_index >= self.image_count() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let (z, _, t) = self.get_zct_coords(plane_index);
        let mut out = Vec::new();
        for c in 0..self.size_c() {
            let source_index = self.reader.get_index(z, c, t);
            let bytes = self.reader.open_bytes_region(source_index, x, y, w, h)?;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let tw = self.size_x().min(256);
        let th = self.size_y().min(256);
        let tx = (self.size_x() - tw) / 2;
        let ty = (self.size_y() - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
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
