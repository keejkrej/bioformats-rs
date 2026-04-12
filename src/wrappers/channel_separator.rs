use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::reader::FormatReader;
use crate::wrappers::reader_wrapper::ReaderWrapper;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSeparatorSnapshot {
    pub inner: Box<crate::snapshot::ReaderSnapshot>,
    pub metadata: ImageMetadata,
}

/// Splits RGB planes into separate per-channel planes.
pub struct ChannelSeparator {
    reader: ReaderWrapper,
    metadata: ImageMetadata,
}

impl ChannelSeparator {
    pub fn new<R: FormatReader + 'static>(reader: R) -> Self {
        let wrapper = ReaderWrapper::new(reader);
        let metadata = Self::separated_metadata(wrapper.metadata());
        Self {
            reader: wrapper,
            metadata,
        }
    }

    fn should_separate(reader: &dyn FormatReader) -> bool {
        reader.is_rgb() && !reader.is_indexed()
    }

    fn separated_metadata(source: &ImageMetadata) -> ImageMetadata {
        if !source.is_rgb || source.is_indexed {
            return source.clone();
        }

        let mut metadata = source.clone();
        metadata.image_count = source.image_count * source.rgb_channel_count();
        metadata.is_rgb = false;
        metadata.is_interleaved = false;
        metadata.dimension_order = match source.dimension_order {
            DimensionOrder::XYCTZ | DimensionOrder::XYCZT => source.dimension_order,
            DimensionOrder::XYTCZ | DimensionOrder::XYTZC => source.dimension_order,
            DimensionOrder::XYZCT | DimensionOrder::XYZTC => DimensionOrder::XYCZT,
        };
        metadata
    }

    pub fn get_original_index(&self, no: u32) -> u32 {
        if !Self::should_separate(self.reader.inner()) {
            return no;
        }

        let (z, c, t) = self.metadata.get_zct_coords(no);
        let source_c = c / self.reader.rgb_channel_count();
        self.reader.get_index(z, source_c, t)
    }

    pub fn from_snapshot(snapshot: ChannelSeparatorSnapshot) -> Result<Self> {
        Ok(Self {
            reader: ReaderWrapper::with_box(snapshot.inner.into_reader()?),
            metadata: snapshot.metadata,
        })
    }
}

impl FormatReader for ChannelSeparator {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.reader.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.reader.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reader.set_id(path)?;
        self.metadata = Self::separated_metadata(self.reader.metadata());
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
        self.metadata = Self::separated_metadata(self.reader.metadata());
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
        if !Self::should_separate(self.reader.inner()) {
            return self.reader.open_bytes_region(plane_index, x, y, w, h);
        }

        if plane_index >= self.image_count() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let original_index = self.get_original_index(plane_index);
        let split_channel = self.get_zct_coords(plane_index).1 % self.reader.rgb_channel_count();
        let full = self.reader.open_bytes_region(original_index, x, y, w, h)?;
        let bpp = self.metadata.pixel_type.bytes_per_sample();
        let rgb = self.reader.rgb_channel_count() as usize;
        let pixel_count = (w * h) as usize;
        let mut out = vec![0u8; pixel_count * bpp];
        let channel = split_channel as usize;

        for pixel in 0..pixel_count {
            let src = pixel * rgb * bpp + channel * bpp;
            let dst = pixel * bpp;
            out[dst..dst + bpp].copy_from_slice(&full[src..src + bpp]);
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

    fn snapshot(&self) -> Result<crate::snapshot::ReaderSnapshot> {
        Ok(crate::snapshot::ReaderSnapshot::ChannelSeparator(
            ChannelSeparatorSnapshot {
                inner: Box::new(self.reader.inner().snapshot()?),
                metadata: self.metadata.clone(),
            },
        ))
    }
}
