use std::path::Path;

use crate::common::error::Result;
use crate::common::metadata::ImageMetadata;

/// Core trait implemented by each format reader.
pub trait FormatReader: Send + Sync {
    fn is_this_type_by_name(&self, path: &Path) -> bool;
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool;
    fn set_id(&mut self, path: &Path) -> Result<()>;
    fn close(&mut self) -> Result<()>;
    fn series_count(&self) -> usize;
    fn set_series(&mut self, series: usize) -> Result<()>;
    fn series(&self) -> usize;
    fn metadata(&self) -> &ImageMetadata;
    fn image_count(&self) -> u32 {
        self.metadata().image_count
    }
    fn size_x(&self) -> u32 {
        self.metadata().size_x
    }
    fn size_y(&self) -> u32 {
        self.metadata().size_y
    }
    fn size_z(&self) -> u32 {
        self.metadata().size_z
    }
    fn size_c(&self) -> u32 {
        self.metadata().size_c
    }
    fn size_t(&self) -> u32 {
        self.metadata().size_t
    }
    fn dimension_order(&self) -> crate::common::metadata::DimensionOrder {
        self.metadata().dimension_order
    }
    fn is_rgb(&self) -> bool {
        self.metadata().is_rgb
    }
    fn is_interleaved(&self) -> bool {
        self.metadata().is_interleaved
    }
    fn is_indexed(&self) -> bool {
        self.metadata().is_indexed
    }
    fn is_false_color(&self) -> bool {
        self.metadata().is_false_color
    }
    fn is_little_endian(&self) -> bool {
        self.metadata().is_little_endian
    }
    fn effective_size_c(&self) -> u32 {
        self.metadata().effective_size_c()
    }
    fn rgb_channel_count(&self) -> u32 {
        self.metadata().rgb_channel_count()
    }
    fn get_index(&self, z: u32, c: u32, t: u32) -> u32 {
        self.metadata().get_index(z, c, t)
    }
    fn get_zct_coords(&self, index: u32) -> (u32, u32, u32) {
        self.metadata().get_zct_coords(index)
    }
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>>;
    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>>;
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>>;
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, _level: usize) -> Result<()> {
        Ok(())
    }
    fn resolution(&self) -> usize {
        0
    }
}
