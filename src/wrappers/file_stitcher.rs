use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::pattern::{AxisGuesser, AxisType, FilePattern};
use crate::registry::ImageReader;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStitcherSnapshot {
    pub prototype: Box<crate::snapshot::ReaderSnapshot>,
    pub current_path: PathBuf,
    pub file_pattern: FilePattern,
    pub axis_guesser: AxisGuesser,
    pub file_paths: Vec<PathBuf>,
    pub underlying_readers: Vec<crate::snapshot::ReaderSnapshot>,
    pub metadata: ImageMetadata,
}

/// Stitches together files that differ across Z, C, or T filename blocks.
pub struct FileStitcher {
    prototype: Box<dyn FormatReader>,
    current_path: Option<PathBuf>,
    file_pattern: Option<FilePattern>,
    axis_guesser: Option<AxisGuesser>,
    file_paths: Vec<PathBuf>,
    underlying_readers: Vec<Box<dyn FormatReader>>,
    metadata: Option<ImageMetadata>,
}

impl FileStitcher {
    pub fn new() -> Self {
        Self::with_box(Box::new(ImageReader::new()))
    }

    pub fn with_reader<R: FormatReader + 'static>(reader: R) -> Self {
        Self::with_box(Box::new(reader))
    }

    pub fn with_box(reader: Box<dyn FormatReader>) -> Self {
        Self {
            prototype: reader,
            current_path: None,
            file_pattern: None,
            axis_guesser: None,
            file_paths: Vec::new(),
            underlying_readers: Vec::new(),
            metadata: None,
        }
    }

    pub fn from_snapshot(snapshot: FileStitcherSnapshot) -> Result<Self> {
        Ok(Self {
            prototype: snapshot.prototype.into_reader()?,
            current_path: Some(snapshot.current_path),
            file_pattern: Some(snapshot.file_pattern),
            axis_guesser: Some(snapshot.axis_guesser),
            file_paths: snapshot.file_paths,
            underlying_readers: snapshot
                .underlying_readers
                .into_iter()
                .map(crate::snapshot::ReaderSnapshot::into_reader)
                .collect::<Result<Vec<_>>>()?,
            metadata: Some(snapshot.metadata),
        })
    }

    pub fn file_pattern(&self) -> Option<&FilePattern> {
        self.file_pattern.as_ref()
    }

    pub fn axis_guesser(&self) -> Option<&AxisGuesser> {
        self.axis_guesser.as_ref()
    }

    pub fn set_axis_types(&mut self, axis_types: Vec<AxisType>) -> Result<()> {
        let guesser = self
            .axis_guesser
            .as_mut()
            .ok_or(BioFormatsError::NotInitialized)?;
        guesser.set_axis_types(axis_types);
        self.recompute_metadata()
    }

    pub fn used_files(&self) -> &[PathBuf] {
        &self.file_paths
    }

    pub fn underlying_readers(&self) -> &[Box<dyn FormatReader>] {
        &self.underlying_readers
    }

    fn recompute_metadata(&mut self) -> Result<()> {
        let guesser = self
            .axis_guesser
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let base = self
            .underlying_readers
            .first()
            .ok_or(BioFormatsError::NotInitialized)?
            .metadata()
            .clone();
        let block_lengths: Vec<u32> = guesser
            .pattern()
            .blocks()
            .iter()
            .map(|block| block.len() as u32)
            .collect();

        let mut z_scale = 1u32;
        let mut c_scale = 1u32;
        let mut t_scale = 1u32;
        for (axis, length) in guesser.axis_types().iter().zip(block_lengths.iter()) {
            match axis {
                AxisType::Z => z_scale = z_scale.saturating_mul(*length),
                AxisType::C => c_scale = c_scale.saturating_mul(*length),
                AxisType::T => t_scale = t_scale.saturating_mul(*length),
                _ => {}
            }
        }

        let logical_c = base.effective_size_c().max(1);
        let rgb = base.rgb_channel_count().max(1);
        let stitched_logical_c = logical_c.saturating_mul(c_scale);
        let mut metadata = base.clone();
        metadata.size_z = base.size_z.saturating_mul(z_scale);
        metadata.size_t = base.size_t.saturating_mul(t_scale);
        metadata.size_c = stitched_logical_c.saturating_mul(rgb);
        metadata.image_count = metadata
            .size_z
            .saturating_mul(stitched_logical_c)
            .saturating_mul(metadata.size_t);
        self.metadata = Some(metadata);
        Ok(())
    }

    fn metadata_ref(&self) -> Result<&ImageMetadata> {
        self.metadata
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)
    }

    fn decode_axis_digits(total: u32, lengths: &[u32]) -> Vec<u32> {
        if lengths.is_empty() {
            return Vec::new();
        }
        let mut digits = Vec::with_capacity(lengths.len());
        let mut remainder = total;
        for index in 0..lengths.len() {
            let suffix = lengths[index + 1..]
                .iter()
                .fold(1u32, |acc, value| acc.saturating_mul(*value));
            let digit = if suffix == 0 { 0 } else { remainder / suffix };
            digits.push(digit % lengths[index].max(1));
            remainder %= suffix.max(1);
        }
        digits
    }

    fn block_digits(&self, z_file: u32, c_file: u32, t_file: u32) -> Result<Vec<u32>> {
        let guesser = self
            .axis_guesser
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let lengths: Vec<u32> = guesser
            .pattern()
            .blocks()
            .iter()
            .map(|block| block.len() as u32)
            .collect();
        let z_lengths: Vec<u32> = guesser
            .axis_types()
            .iter()
            .zip(lengths.iter())
            .filter_map(|(axis, length)| (*axis == AxisType::Z).then_some(*length))
            .collect();
        let c_lengths: Vec<u32> = guesser
            .axis_types()
            .iter()
            .zip(lengths.iter())
            .filter_map(|(axis, length)| (*axis == AxisType::C).then_some(*length))
            .collect();
        let t_lengths: Vec<u32> = guesser
            .axis_types()
            .iter()
            .zip(lengths.iter())
            .filter_map(|(axis, length)| (*axis == AxisType::T).then_some(*length))
            .collect();
        let z_digits = Self::decode_axis_digits(z_file, &z_lengths);
        let c_digits = Self::decode_axis_digits(c_file, &c_lengths);
        let t_digits = Self::decode_axis_digits(t_file, &t_lengths);

        let mut z_cursor = 0usize;
        let mut c_cursor = 0usize;
        let mut t_cursor = 0usize;
        let mut digits = Vec::with_capacity(guesser.axis_types().len());
        for axis in guesser.axis_types() {
            let digit = match axis {
                AxisType::Z => {
                    let value = z_digits[z_cursor];
                    z_cursor += 1;
                    value
                }
                AxisType::C => {
                    let value = c_digits[c_cursor];
                    c_cursor += 1;
                    value
                }
                AxisType::T => {
                    let value = t_digits[t_cursor];
                    t_cursor += 1;
                    value
                }
                _ => 0,
            };
            digits.push(digit);
        }
        Ok(digits)
    }

    fn file_index_from_digits(&self, digits: &[u32]) -> Result<usize> {
        let pattern = self
            .file_pattern
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let lengths: Vec<usize> = pattern.blocks().iter().map(|block| block.len()).collect();
        let mut index = 0usize;
        for (position, digit) in digits.iter().enumerate() {
            let suffix = lengths[position + 1..]
                .iter()
                .fold(1usize, |acc, value| acc.saturating_mul(*value));
            index += *digit as usize * suffix;
        }
        Ok(index)
    }

    fn plane_mapping(&self, plane_index: u32) -> Result<(usize, u32)> {
        let metadata = self.metadata_ref()?;
        if plane_index >= metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let base = self
            .underlying_readers
            .first()
            .ok_or(BioFormatsError::NotInitialized)?
            .metadata()
            .clone();

        let (z, c, t) = metadata.get_zct_coords(plane_index);
        let base_logical_c = base.effective_size_c().max(1);
        let inner_z = z % base.size_z.max(1);
        let inner_c = c % base_logical_c;
        let inner_t = t % base.size_t.max(1);
        let z_file = z / base.size_z.max(1);
        let c_file = c / base_logical_c.max(1);
        let t_file = t / base.size_t.max(1);
        let digits = self.block_digits(z_file, c_file, t_file)?;
        let file_index = self.file_index_from_digits(&digits)?;
        let inner_index = base.get_index(inner_z, inner_c, inner_t);
        Ok((file_index, inner_index))
    }
}

impl Default for FileStitcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FileStitcher {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.prototype.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.prototype.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let pattern = if path.to_string_lossy().contains('<') {
            FilePattern::parse(path.to_string_lossy().to_string())
                .ok_or_else(|| BioFormatsError::InvalidData("invalid file pattern".into()))?
        } else {
            FilePattern::parse(FilePattern::find_pattern(path))
                .unwrap_or_else(|| FilePattern::parse(path.display().to_string()).unwrap())
        };
        let file_paths = pattern.files();
        if file_paths.is_empty() {
            return Err(BioFormatsError::InvalidData(
                "file pattern expanded to zero files".into(),
            ));
        }

        self.underlying_readers.clear();
        for file_path in &file_paths {
            let mut reader = self.prototype.clone_boxed()?;
            reader.set_id(file_path)?;
            self.underlying_readers.push(reader);
        }

        let base = self
            .underlying_readers
            .first()
            .ok_or(BioFormatsError::NotInitialized)?
            .metadata()
            .clone();
        let axis_guesser = AxisGuesser::new(
            pattern.clone(),
            base.size_z,
            base.size_t,
            base.effective_size_c(),
        );
        self.current_path = Some(path.to_path_buf());
        self.file_paths = file_paths;
        self.file_pattern = Some(pattern);
        self.axis_guesser = Some(axis_guesser);
        self.recompute_metadata()?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        for reader in &mut self.underlying_readers {
            reader.close()?;
        }
        self.current_path = None;
        self.file_pattern = None;
        self.axis_guesser = None;
        self.file_paths.clear();
        self.underlying_readers.clear();
        self.metadata = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(series))
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metadata.as_ref().expect("set_id not called")
    }

    fn current_file(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (file_index, inner_index) = self.plane_mapping(plane_index)?;
        self.underlying_readers[file_index].open_bytes(inner_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let (file_index, inner_index) = self.plane_mapping(plane_index)?;
        self.underlying_readers[file_index].open_bytes_region(inner_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (file_index, inner_index) = self.plane_mapping(plane_index)?;
        self.underlying_readers[file_index].open_thumb_bytes(inner_index)
    }

    fn snapshot(&self) -> Result<crate::snapshot::ReaderSnapshot> {
        Ok(crate::snapshot::ReaderSnapshot::FileStitcher(
            FileStitcherSnapshot {
                prototype: Box::new(self.prototype.snapshot()?),
                current_path: self
                    .current_path
                    .clone()
                    .ok_or(BioFormatsError::NotInitialized)?,
                file_pattern: self
                    .file_pattern
                    .clone()
                    .ok_or(BioFormatsError::NotInitialized)?,
                axis_guesser: self
                    .axis_guesser
                    .clone()
                    .ok_or(BioFormatsError::NotInitialized)?,
                file_paths: self.file_paths.clone(),
                underlying_readers: self
                    .underlying_readers
                    .iter()
                    .map(|reader| reader.snapshot())
                    .collect::<Result<Vec<_>>>()?,
                metadata: self
                    .metadata
                    .clone()
                    .ok_or(BioFormatsError::NotInitialized)?,
            },
        ))
    }
}
