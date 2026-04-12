use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::reader::FormatReader;
use crate::wrappers::reader_wrapper::ReaderWrapper;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionSwapperSnapshot {
    pub inner: Box<crate::snapshot::ReaderSnapshot>,
    pub metadata: ImageMetadata,
    pub input_order: DimensionOrder,
    pub output_order: DimensionOrder,
}

/// Reinterprets source plane order and exposes a different output order.
pub struct DimensionSwapper {
    reader: ReaderWrapper,
    metadata: ImageMetadata,
    input_order: DimensionOrder,
    output_order: DimensionOrder,
}

impl DimensionSwapper {
    pub fn new<R: FormatReader + 'static>(reader: R) -> Self {
        let wrapper = ReaderWrapper::new(reader);
        let meta = wrapper.metadata().clone();
        Self {
            reader: wrapper,
            metadata: meta.clone(),
            input_order: meta.dimension_order,
            output_order: meta.dimension_order,
        }
    }

    pub fn input_order(&self) -> DimensionOrder {
        self.input_order
    }

    pub fn output_order(&self) -> DimensionOrder {
        self.output_order
    }

    pub fn swap_dimensions(&mut self, order: DimensionOrder) {
        let old = self.input_order;
        if old == order {
            return;
        }

        let old_meta = self.metadata.clone();
        let dims = [
            old_meta.size_x,
            old_meta.size_y,
            old_meta.size_z,
            old_meta.size_c,
            old_meta.size_t,
        ];

        let old_chars = old.as_str().as_bytes();
        let new_chars = order.as_str().as_bytes();

        let index_of = |chars: &[u8], axis: u8| chars.iter().position(|c| *c == axis).unwrap();

        self.metadata.size_x = dims[index_of(old_chars, new_chars[0])];
        self.metadata.size_y = dims[index_of(old_chars, new_chars[1])];
        self.metadata.size_z = dims[index_of(old_chars, new_chars[2])];
        self.metadata.size_c = dims[index_of(old_chars, new_chars[3])];
        self.metadata.size_t = dims[index_of(old_chars, new_chars[4])];
        self.input_order = order;
    }

    pub fn set_output_order(&mut self, order: DimensionOrder) {
        self.output_order = order;
        self.metadata.dimension_order = order;
    }

    fn reordered_index(&self, no: u32) -> u32 {
        let output_coords = self.metadata.get_zct_coords(no);
        let mut source_meta = self.metadata.clone();
        source_meta.dimension_order = self.input_order;
        source_meta.get_index(output_coords.0, output_coords.1, output_coords.2)
    }

    pub fn from_snapshot(snapshot: DimensionSwapperSnapshot) -> Result<Self> {
        Ok(Self {
            reader: ReaderWrapper::with_box(snapshot.inner.into_reader()?),
            metadata: snapshot.metadata,
            input_order: snapshot.input_order,
            output_order: snapshot.output_order,
        })
    }
}

impl FormatReader for DimensionSwapper {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.reader.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.reader.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reader.set_id(path)?;
        self.metadata = self.reader.metadata().clone();
        self.input_order = self.metadata.dimension_order;
        self.output_order = self.metadata.dimension_order;
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
        self.metadata = self.reader.metadata().clone();
        self.input_order = self.metadata.dimension_order;
        self.output_order = self.metadata.dimension_order;
        Ok(())
    }

    fn series(&self) -> usize {
        self.reader.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if plane_index >= self.image_count() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.reader.open_bytes(self.reordered_index(plane_index))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if plane_index >= self.image_count() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.reader
            .open_bytes_region(self.reordered_index(plane_index), x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if plane_index >= self.image_count() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.reader
            .open_thumb_bytes(self.reordered_index(plane_index))
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
        Ok(crate::snapshot::ReaderSnapshot::DimensionSwapper(
            DimensionSwapperSnapshot {
                inner: Box::new(self.reader.inner().snapshot()?),
                metadata: self.metadata.clone(),
                input_order: self.input_order,
                output_order: self.output_order,
            },
        ))
    }
}
