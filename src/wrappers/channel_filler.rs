use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{ImageMetadata, LookupTable};
use crate::common::reader::FormatReader;
use crate::wrappers::reader_wrapper::ReaderWrapper;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelFillerSnapshot {
    pub inner: Box<crate::snapshot::ReaderSnapshot>,
    pub metadata: ImageMetadata,
    pub filled: Option<bool>,
    pub lut_components: u32,
}

/// Expands indexed color pixels into direct RGB samples when a true-colour LUT is present.
pub struct ChannelFiller {
    reader: ReaderWrapper,
    metadata: ImageMetadata,
    filled: Option<bool>,
    lut_components: u32,
}

impl ChannelFiller {
    pub fn new<R: FormatReader + 'static>(reader: R) -> Self {
        let wrapper = ReaderWrapper::new(reader);
        let lut_components = Self::lookup_table_components(wrapper.lookup_table());
        let metadata = Self::filled_metadata(wrapper.metadata(), lut_components, None);
        Self {
            reader: wrapper,
            metadata,
            filled: None,
            lut_components,
        }
    }

    pub fn is_filled(&self) -> bool {
        if !self.reader.is_indexed() || self.lut_components == 0 {
            return false;
        }
        self.filled.unwrap_or(!self.reader.is_false_color())
    }

    pub fn set_filled(&mut self, filled: bool) {
        self.filled = Some(filled);
        self.metadata =
            Self::filled_metadata(self.reader.metadata(), self.lut_components, self.filled);
    }

    fn lookup_table_components(table: Option<&LookupTable>) -> u32 {
        table
            .map(|lut| {
                let mut count = 0;
                if !lut.red.is_empty() {
                    count += 1;
                }
                if !lut.green.is_empty() {
                    count += 1;
                }
                if !lut.blue.is_empty() {
                    count += 1;
                }
                count
            })
            .unwrap_or(0)
    }

    fn filled_metadata(
        source: &ImageMetadata,
        lut_components: u32,
        filled: Option<bool>,
    ) -> ImageMetadata {
        let should_fill =
            source.is_indexed && lut_components > 0 && filled.unwrap_or(!source.is_false_color);
        if !should_fill {
            return source.clone();
        }

        let mut metadata = source.clone();
        metadata.size_c = source.size_c.saturating_mul(lut_components);
        metadata.samples_per_pixel = lut_components;
        metadata.is_indexed = false;
        metadata.is_rgb = lut_components > 1;
        metadata.lookup_table = None;
        metadata
    }

    fn refresh_metadata(&mut self) {
        self.lut_components = Self::lookup_table_components(self.reader.lookup_table());
        self.metadata =
            Self::filled_metadata(self.reader.metadata(), self.lut_components, self.filled);
    }

    fn expand_indices(
        &self,
        indices: &[u8],
        table: &LookupTable,
        little_endian: bool,
        interleaved: bool,
        bytes_per_sample: usize,
    ) -> Result<Vec<u8>> {
        let pixel_count = if bytes_per_sample == 0 {
            0
        } else {
            indices.len() / bytes_per_sample
        };
        let channels = self.lut_components as usize;
        let mut out = vec![0u8; pixel_count * channels * bytes_per_sample];

        for pixel in 0..pixel_count {
            let table_index = match bytes_per_sample {
                1 => indices[pixel] as usize,
                2 => {
                    let offset = pixel * 2;
                    let raw = [indices[offset], indices[offset + 1]];
                    if little_endian {
                        u16::from_le_bytes(raw) as usize
                    } else {
                        u16::from_be_bytes(raw) as usize
                    }
                }
                other => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "ChannelFiller does not support {}-byte indexed samples",
                        other
                    )));
                }
            };

            let values = [
                table.red.get(table_index).copied().unwrap_or(0),
                table.green.get(table_index).copied().unwrap_or(0),
                table.blue.get(table_index).copied().unwrap_or(0),
            ];

            for channel in 0..channels {
                let dst_index = if interleaved {
                    (pixel * channels + channel) * bytes_per_sample
                } else {
                    (channel * pixel_count + pixel) * bytes_per_sample
                };
                match bytes_per_sample {
                    1 => out[dst_index] = values[channel] as u8,
                    2 => {
                        let bytes = if little_endian {
                            values[channel].to_le_bytes()
                        } else {
                            values[channel].to_be_bytes()
                        };
                        out[dst_index..dst_index + 2].copy_from_slice(&bytes);
                    }
                    _ => unreachable!(),
                }
            }
        }

        Ok(out)
    }

    pub fn from_snapshot(snapshot: ChannelFillerSnapshot) -> Result<Self> {
        Ok(Self {
            reader: ReaderWrapper::with_box(snapshot.inner.into_reader()?),
            metadata: snapshot.metadata,
            filled: snapshot.filled,
            lut_components: snapshot.lut_components,
        })
    }
}

impl FormatReader for ChannelFiller {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.reader.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.reader.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reader.set_id(path)?;
        self.refresh_metadata();
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
        self.refresh_metadata();
        Ok(())
    }

    fn series(&self) -> usize {
        self.reader.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }

    fn current_file(&self) -> Option<&Path> {
        self.reader.current_file()
    }

    fn used_files(&self) -> Vec<PathBuf> {
        self.reader.used_files()
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
        if !self.is_filled() {
            return self.reader.open_bytes_region(plane_index, x, y, w, h);
        }

        if plane_index >= self.image_count() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let indices = self.reader.open_bytes_region(plane_index, x, y, w, h)?;
        let table = self
            .reader
            .lookup_table()
            .ok_or_else(|| BioFormatsError::InvalidData("indexed plane missing LUT".into()))?;
        self.expand_indices(
            &indices,
            table,
            self.reader.is_little_endian(),
            self.reader.is_interleaved(),
            self.reader.metadata().pixel_type.bytes_per_sample(),
        )
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

    fn set_flattened_resolutions(&mut self, flattened: bool) -> Result<()> {
        self.reader.set_flattened_resolutions(flattened)
    }

    fn flattened_resolutions(&self) -> bool {
        self.reader.flattened_resolutions()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.reader.set_resolution(level)?;
        self.refresh_metadata();
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.reader.resolution()
    }

    fn snapshot(&self) -> Result<crate::snapshot::ReaderSnapshot> {
        Ok(crate::snapshot::ReaderSnapshot::ChannelFiller(
            ChannelFillerSnapshot {
                inner: Box::new(self.reader.inner().snapshot()?),
                metadata: self.metadata.clone(),
                filled: self.filled,
                lut_components: self.lut_components,
            },
        ))
    }
}
