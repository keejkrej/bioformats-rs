use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::common::error::Result;
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::formats::czi::CziReaderSnapshot;
use crate::formats::nd2::Nd2ReaderSnapshot;
use crate::registry::ImageReaderSnapshot;
use crate::tiff::reader::TiffReaderSnapshot;
use crate::wrappers::{
    ChannelFillerSnapshot, ChannelMergerSnapshot, ChannelSeparatorSnapshot,
    DimensionSwapperSnapshot, FileStitcherSnapshot, MinMaxCalculatorSnapshot,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReaderSnapshot {
    ImageReader(ImageReaderSnapshot),
    TiffReader(TiffReaderSnapshot),
    Nd2Reader(Nd2ReaderSnapshot),
    CziReader(CziReaderSnapshot),
    ChannelSeparator(ChannelSeparatorSnapshot),
    ChannelMerger(ChannelMergerSnapshot),
    ChannelFiller(ChannelFillerSnapshot),
    DimensionSwapper(DimensionSwapperSnapshot),
    MinMaxCalculator(MinMaxCalculatorSnapshot),
    FileStitcher(FileStitcherSnapshot),
}

impl ReaderSnapshot {
    pub fn into_reader(self) -> Result<Box<dyn FormatReader>> {
        match self {
            ReaderSnapshot::ImageReader(snapshot) => Ok(Box::new(
                crate::registry::ImageReader::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::TiffReader(snapshot) => {
                Ok(Box::new(crate::tiff::TiffReader::from_snapshot(snapshot)?))
            }
            ReaderSnapshot::Nd2Reader(snapshot) => Ok(Box::new(
                crate::formats::nd2::Nd2Reader::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::CziReader(snapshot) => Ok(Box::new(
                crate::formats::czi::CziReader::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::ChannelSeparator(snapshot) => Ok(Box::new(
                crate::wrappers::ChannelSeparator::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::ChannelMerger(snapshot) => Ok(Box::new(
                crate::wrappers::ChannelMerger::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::ChannelFiller(snapshot) => Ok(Box::new(
                crate::wrappers::ChannelFiller::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::DimensionSwapper(snapshot) => Ok(Box::new(
                crate::wrappers::DimensionSwapper::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::MinMaxCalculator(snapshot) => Ok(Box::new(
                crate::wrappers::MinMaxCalculator::from_snapshot(snapshot)?,
            )),
            ReaderSnapshot::FileStitcher(snapshot) => Ok(Box::new(
                crate::wrappers::FileStitcher::from_snapshot(snapshot)?,
            )),
        }
    }

    pub fn retarget_path(&mut self, path: &std::path::Path) {
        match self {
            ReaderSnapshot::ImageReader(snapshot) => {
                snapshot.current_path = path.to_path_buf();
                snapshot.inner.retarget_path(path);
            }
            ReaderSnapshot::TiffReader(snapshot) => snapshot.retarget_primary_path(path),
            ReaderSnapshot::Nd2Reader(snapshot) => snapshot.path = path.to_path_buf(),
            ReaderSnapshot::CziReader(snapshot) => snapshot.path = path.to_path_buf(),
            ReaderSnapshot::ChannelSeparator(snapshot) => snapshot.inner.retarget_path(path),
            ReaderSnapshot::ChannelMerger(snapshot) => snapshot.inner.retarget_path(path),
            ReaderSnapshot::ChannelFiller(snapshot) => snapshot.inner.retarget_path(path),
            ReaderSnapshot::DimensionSwapper(snapshot) => snapshot.inner.retarget_path(path),
            ReaderSnapshot::MinMaxCalculator(snapshot) => snapshot.inner.retarget_path(path),
            ReaderSnapshot::FileStitcher(snapshot) => {
                snapshot.current_path = path.to_path_buf();
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFingerprint {
    pub path: PathBuf,
    pub file_size: u64,
    pub modified_unix_seconds: Option<u64>,
    pub modified_unix_nanos: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoFilePayload {
    pub version: u32,
    pub source: SourceFingerprint,
    pub snapshot: ReaderSnapshot,
}

pub fn capture_fingerprint(path: &std::path::Path) -> Result<SourceFingerprint> {
    let metadata = std::fs::metadata(path)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
    Ok(SourceFingerprint {
        path: path.to_path_buf(),
        file_size: metadata.len(),
        modified_unix_seconds: modified.as_ref().map(|value| value.as_secs()),
        modified_unix_nanos: modified.map(|value| value.subsec_nanos()),
    })
}

pub fn fingerprint_matches(path: &std::path::Path, expected: &SourceFingerprint) -> bool {
    let Ok(current) = capture_fingerprint(path) else {
        return false;
    };
    current.file_size == expected.file_size
        && current.modified_unix_seconds == expected.modified_unix_seconds
        && current.modified_unix_nanos == expected.modified_unix_nanos
}

pub fn metadata_matches(left: &ImageMetadata, right: &ImageMetadata) -> bool {
    left.size_x == right.size_x
        && left.size_y == right.size_y
        && left.size_z == right.size_z
        && left.size_c == right.size_c
        && left.size_t == right.size_t
        && left.pixel_type == right.pixel_type
        && left.bits_per_pixel == right.bits_per_pixel
        && left.samples_per_pixel == right.samples_per_pixel
        && left.image_count == right.image_count
        && left.dimension_order == right.dimension_order
        && left.is_rgb == right.is_rgb
        && left.is_interleaved == right.is_interleaved
        && left.is_indexed == right.is_indexed
        && left.is_false_color == right.is_false_color
        && left.is_little_endian == right.is_little_endian
        && left.resolution_count == right.resolution_count
}
