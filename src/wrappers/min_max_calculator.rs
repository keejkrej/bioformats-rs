use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::wrappers::reader_wrapper::ReaderWrapper;

pub trait MinMaxStore: Send + Sync {
    fn set_channel_global_min_max(
        &mut self,
        channel: usize,
        minimum: f64,
        maximum: f64,
        series: usize,
    );
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SeriesStats {
    channel_min: Vec<Option<f64>>,
    channel_max: Vec<Option<f64>>,
    plane_min: Vec<Vec<Option<f64>>>,
    plane_max: Vec<Vec<Option<f64>>>,
    plane_complete: Vec<bool>,
    store_notified: bool,
}

impl SeriesStats {
    fn new(metadata: &ImageMetadata) -> Self {
        let channels = metadata.rgb_channel_count().max(1) as usize;
        let plane_count = metadata.image_count as usize;
        Self {
            channel_min: vec![None; channels],
            channel_max: vec![None; channels],
            plane_min: vec![vec![None; channels]; plane_count],
            plane_max: vec![vec![None; channels]; plane_count],
            plane_complete: vec![false; plane_count],
            store_notified: false,
        }
    }
}

/// Tracks per-plane and per-channel minima/maxima as planes are read.
pub struct MinMaxCalculator {
    reader: ReaderWrapper,
    series_stats: HashMap<usize, SeriesStats>,
    min_max_store: Option<Box<dyn MinMaxStore>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinMaxCalculatorSnapshot {
    pub inner: Box<crate::snapshot::ReaderSnapshot>,
    pub series_stats: HashMap<usize, SeriesStats>,
}

impl MinMaxCalculator {
    pub fn new<R: FormatReader + 'static>(reader: R) -> Self {
        let wrapper = ReaderWrapper::new(reader);
        let mut series_stats = HashMap::new();
        series_stats.insert(wrapper.series(), SeriesStats::new(wrapper.metadata()));
        Self {
            reader: wrapper,
            series_stats,
            min_max_store: None,
        }
    }

    pub fn from_snapshot(snapshot: MinMaxCalculatorSnapshot) -> Result<Self> {
        Ok(Self {
            reader: ReaderWrapper::with_box(snapshot.inner.into_reader()?),
            series_stats: snapshot.series_stats,
            min_max_store: None,
        })
    }

    pub fn set_min_max_store(&mut self, store: Box<dyn MinMaxStore>) {
        self.min_max_store = Some(store);
    }

    fn stats(&self) -> &SeriesStats {
        self.series_stats
            .get(&self.reader.series())
            .expect("series stats missing")
    }

    fn stats_mut(&mut self) -> &mut SeriesStats {
        let series = self.reader.series();
        self.series_stats
            .entry(series)
            .or_insert_with(|| SeriesStats::new(self.reader.metadata()))
    }

    fn refresh_series_state(&mut self) {
        let series = self.reader.series();
        self.series_stats
            .entry(series)
            .or_insert_with(|| SeriesStats::new(self.reader.metadata()));
    }

    pub fn get_channel_global_minimum(&self, channel: usize) -> Result<Option<f64>> {
        if channel >= self.metadata().rgb_channel_count().max(1) as usize {
            return Err(BioFormatsError::InvalidData(format!(
                "Invalid channel index: {}",
                channel
            )));
        }
        if !self.is_min_max_populated() {
            return Ok(None);
        }
        Ok(self.stats().channel_min[channel])
    }

    pub fn get_channel_global_maximum(&self, channel: usize) -> Result<Option<f64>> {
        if channel >= self.metadata().rgb_channel_count().max(1) as usize {
            return Err(BioFormatsError::InvalidData(format!(
                "Invalid channel index: {}",
                channel
            )));
        }
        if !self.is_min_max_populated() {
            return Ok(None);
        }
        Ok(self.stats().channel_max[channel])
    }

    pub fn get_channel_known_minimum(&self, channel: usize) -> Result<Option<f64>> {
        if channel >= self.metadata().rgb_channel_count().max(1) as usize {
            return Err(BioFormatsError::InvalidData(format!(
                "Invalid channel index: {}",
                channel
            )));
        }
        Ok(self.stats().channel_min[channel])
    }

    pub fn get_channel_known_maximum(&self, channel: usize) -> Result<Option<f64>> {
        if channel >= self.metadata().rgb_channel_count().max(1) as usize {
            return Err(BioFormatsError::InvalidData(format!(
                "Invalid channel index: {}",
                channel
            )));
        }
        Ok(self.stats().channel_max[channel])
    }

    pub fn get_plane_minimum(&self, plane_index: u32) -> Result<Option<Vec<f64>>> {
        let plane_index = plane_index as usize;
        let stats = self.stats();
        if plane_index >= stats.plane_min.len() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index as u32));
        }
        if stats.plane_min[plane_index].iter().all(Option::is_none) {
            return Ok(None);
        }
        Ok(Some(
            stats.plane_min[plane_index]
                .iter()
                .map(|value| value.unwrap_or(f64::NAN))
                .collect(),
        ))
    }

    pub fn get_plane_maximum(&self, plane_index: u32) -> Result<Option<Vec<f64>>> {
        let plane_index = plane_index as usize;
        let stats = self.stats();
        if plane_index >= stats.plane_max.len() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index as u32));
        }
        if stats.plane_max[plane_index].iter().all(Option::is_none) {
            return Ok(None);
        }
        Ok(Some(
            stats.plane_max[plane_index]
                .iter()
                .map(|value| value.unwrap_or(f64::NAN))
                .collect(),
        ))
    }

    pub fn is_min_max_populated(&self) -> bool {
        self.stats().plane_complete.iter().all(|done| *done)
    }

    fn read_sample(bytes: &[u8], pixel_type: PixelType, little_endian: bool) -> f64 {
        match pixel_type {
            PixelType::Int8 => (bytes[0] as i8) as f64,
            PixelType::Uint8 => bytes[0] as f64,
            PixelType::Int16 => {
                let raw = [bytes[0], bytes[1]];
                if little_endian {
                    i16::from_le_bytes(raw) as f64
                } else {
                    i16::from_be_bytes(raw) as f64
                }
            }
            PixelType::Uint16 => {
                let raw = [bytes[0], bytes[1]];
                if little_endian {
                    u16::from_le_bytes(raw) as f64
                } else {
                    u16::from_be_bytes(raw) as f64
                }
            }
            PixelType::Int32 => {
                let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
                if little_endian {
                    i32::from_le_bytes(raw) as f64
                } else {
                    i32::from_be_bytes(raw) as f64
                }
            }
            PixelType::Uint32 => {
                let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
                if little_endian {
                    u32::from_le_bytes(raw) as f64
                } else {
                    u32::from_be_bytes(raw) as f64
                }
            }
            PixelType::Float32 => {
                let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
                if little_endian {
                    f32::from_le_bytes(raw) as f64
                } else {
                    f32::from_be_bytes(raw) as f64
                }
            }
            PixelType::Float64 => {
                let raw = [
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ];
                if little_endian {
                    f64::from_le_bytes(raw)
                } else {
                    f64::from_be_bytes(raw)
                }
            }
            PixelType::Bit => (bytes[0] & 1) as f64,
        }
    }

    fn update_min_max(&mut self, plane_index: u32, bytes: &[u8], full_plane: bool) -> Result<()> {
        let metadata = self.metadata().clone();
        let bytes_per_sample = metadata.pixel_type.bytes_per_sample();
        if bytes_per_sample == 0 || bytes.is_empty() {
            return Ok(());
        }

        let channel_count = metadata.rgb_channel_count().max(1) as usize;
        let sample_count = bytes.len() / bytes_per_sample;
        let pixels_per_channel = sample_count / channel_count.max(1);
        let interleaved = metadata.is_rgb && metadata.is_interleaved;

        let mut local_min = vec![f64::INFINITY; channel_count];
        let mut local_max = vec![f64::NEG_INFINITY; channel_count];

        for channel in 0..channel_count {
            for pixel in 0..pixels_per_channel {
                let sample_index = if interleaved {
                    pixel * channel_count + channel
                } else {
                    channel * pixels_per_channel + pixel
                };
                let start = sample_index * bytes_per_sample;
                let value = Self::read_sample(
                    &bytes[start..start + bytes_per_sample],
                    metadata.pixel_type,
                    metadata.is_little_endian,
                );
                if value < local_min[channel] {
                    local_min[channel] = value;
                }
                if value > local_max[channel] {
                    local_max[channel] = value;
                }
            }
        }

        let series = self.reader.series();
        let plane_index = plane_index as usize;
        let mut notify_payload = None;
        {
            let stats = self.stats_mut();
            if plane_index >= stats.plane_min.len() {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index as u32));
            }

            if full_plane && stats.plane_complete[plane_index] {
                return Ok(());
            }

            for channel in 0..channel_count {
                let min_slot = &mut stats.plane_min[plane_index][channel];
                *min_slot = Some(
                    min_slot.map_or(local_min[channel], |value| value.min(local_min[channel])),
                );
                let max_slot = &mut stats.plane_max[plane_index][channel];
                *max_slot = Some(
                    max_slot.map_or(local_max[channel], |value| value.max(local_max[channel])),
                );

                let channel_min = &mut stats.channel_min[channel];
                *channel_min = Some(
                    channel_min.map_or(local_min[channel], |value| value.min(local_min[channel])),
                );
                let channel_max = &mut stats.channel_max[channel];
                *channel_max = Some(
                    channel_max.map_or(local_max[channel], |value| value.max(local_max[channel])),
                );
            }

            if full_plane {
                stats.plane_complete[plane_index] = true;
            }

            if stats.plane_complete.iter().all(|done| *done) && !stats.store_notified {
                notify_payload = Some(
                    stats
                        .channel_min
                        .iter()
                        .zip(stats.channel_max.iter())
                        .enumerate()
                        .filter_map(|(channel, (minimum, maximum))| {
                            minimum
                                .zip(*maximum)
                                .map(|(minimum, maximum)| (channel, minimum, maximum))
                        })
                        .collect::<Vec<_>>(),
                );
                stats.store_notified = true;
            }
        }

        if let (Some(store), Some(payload)) = (self.min_max_store.as_mut(), notify_payload) {
            for (channel, minimum, maximum) in payload {
                store.set_channel_global_min_max(channel, minimum, maximum, series);
            }
        }

        Ok(())
    }
}

impl FormatReader for MinMaxCalculator {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.reader.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.reader.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reader.set_id(path)?;
        self.series_stats.clear();
        self.refresh_series_state();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.reader.close()?;
        self.series_stats.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.reader.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.reader.set_series(series)?;
        self.refresh_series_state();
        Ok(())
    }

    fn series(&self) -> usize {
        self.reader.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.reader.metadata()
    }

    fn current_file(&self) -> Option<&Path> {
        self.reader.current_file()
    }

    fn used_files(&self) -> Vec<PathBuf> {
        self.reader.used_files()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let bytes = self.reader.open_bytes(plane_index)?;
        self.update_min_max(plane_index, &bytes, true)?;
        Ok(bytes)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let bytes = self.reader.open_bytes_region(plane_index, x, y, w, h)?;
        let full_plane = x == 0 && y == 0 && w == self.size_x() && h == self.size_y();
        self.update_min_max(plane_index, &bytes, full_plane)?;
        Ok(bytes)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.reader.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.reader.resolution_count()
    }

    fn set_flattened_resolutions(&mut self, flattened: bool) -> Result<()> {
        self.reader.set_flattened_resolutions(flattened)?;
        self.refresh_series_state();
        Ok(())
    }

    fn flattened_resolutions(&self) -> bool {
        self.reader.flattened_resolutions()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.reader.set_resolution(level)?;
        self.refresh_series_state();
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.reader.resolution()
    }

    fn snapshot(&self) -> Result<crate::snapshot::ReaderSnapshot> {
        Ok(crate::snapshot::ReaderSnapshot::MinMaxCalculator(
            MinMaxCalculatorSnapshot {
                inner: Box::new(self.reader.inner().snapshot()?),
                series_stats: self.series_stats.clone(),
            },
        ))
    }
}
