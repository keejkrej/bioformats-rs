use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::peek_header;
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::snapshot::ReaderSnapshot;

/// Stable identifier for a built-in reader family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum FormatId {
    Tiff,
    Nd2,
    Czi,
    Nrrd,
    Mrc,
    Dcimg,
}

impl FormatId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tiff => "tiff",
            Self::Nd2 => "nd2",
            Self::Czi => "czi",
            Self::Nrrd => "nrrd",
            Self::Mrc => "mrc",
            Self::Dcimg => "dcimg",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Tiff => "TIFF / OME-TIFF",
            Self::Nd2 => "Nikon NIS-Elements ND2",
            Self::Czi => "Zeiss CZI",
            Self::Nrrd => "Nearly Raw Raster Data (NRRD)",
            Self::Mrc => "Medical Research Council (MRC)",
            Self::Dcimg => "Hamamatsu DCIMG",
        }
    }

    pub const fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Tiff => &["tif", "tiff", "tf2", "tf8", "btf", "ome"],
            Self::Nd2 => &["nd2"],
            Self::Czi => &["czi"],
            Self::Nrrd => &["nrrd", "nhdr"],
            Self::Mrc => &["mrc", "st", "ali", "map", "rec", "mrcs"],
            Self::Dcimg => &["dcimg"],
        }
    }
}

pub const SUPPORTED_FORMATS: &[FormatId] = &[
    FormatId::Tiff,
    FormatId::Nd2,
    FormatId::Czi,
    FormatId::Nrrd,
    FormatId::Mrc,
    FormatId::Dcimg,
];

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImageReaderSnapshot {
    pub current_path: PathBuf,
    pub inner: Box<ReaderSnapshot>,
}

/// Auto-detecting image reader for the supported MVP formats.
pub struct ImageReader {
    inner: Option<Box<dyn FormatReader>>,
    current_path: Option<PathBuf>,
    format: Option<FormatId>,
}

fn all_readers() -> Vec<(FormatId, Box<dyn FormatReader>)> {
    vec![
        (FormatId::Tiff, Box::new(crate::tiff::TiffReader::new())),
        (
            FormatId::Czi,
            Box::new(crate::formats::czi::CziReader::new()),
        ),
        (
            FormatId::Nd2,
            Box::new(crate::formats::nd2::Nd2Reader::new()),
        ),
        (
            FormatId::Nrrd,
            Box::new(crate::formats::nrrd::NrrdReader::new()),
        ),
        (
            FormatId::Dcimg,
            Box::new(crate::formats::dcimg::DcimgReader::new()),
        ),
        (
            FormatId::Mrc,
            Box::new(crate::formats::mrc::MrcReader::new()),
        ),
    ]
}

fn snapshot_format(snapshot: &ReaderSnapshot) -> Option<FormatId> {
    match snapshot {
        ReaderSnapshot::TiffReader(_) => Some(FormatId::Tiff),
        ReaderSnapshot::Nd2Reader(_) => Some(FormatId::Nd2),
        ReaderSnapshot::CziReader(_) => Some(FormatId::Czi),
        ReaderSnapshot::ImageReader(snapshot) => snapshot_format(&snapshot.inner),
        ReaderSnapshot::ChannelSeparator(snapshot) => snapshot_format(&snapshot.inner),
        ReaderSnapshot::ChannelMerger(snapshot) => snapshot_format(&snapshot.inner),
        ReaderSnapshot::ChannelFiller(snapshot) => snapshot_format(&snapshot.inner),
        ReaderSnapshot::DimensionSwapper(snapshot) => snapshot_format(&snapshot.inner),
        ReaderSnapshot::MinMaxCalculator(snapshot) => snapshot_format(&snapshot.inner),
        ReaderSnapshot::FileStitcher(snapshot) => snapshot
            .underlying_readers
            .first()
            .and_then(snapshot_format)
            .or_else(|| snapshot_format(&snapshot.prototype)),
    }
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
            format: None,
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut reader = Self::new();
        reader.set_id(path)?;
        Ok(reader)
    }

    pub fn from_snapshot(snapshot: ImageReaderSnapshot) -> Result<Self> {
        let format = snapshot_format(&snapshot.inner).ok_or(BioFormatsError::NotInitialized)?;
        Ok(Self {
            inner: Some(snapshot.inner.into_reader()?),
            current_path: Some(snapshot.current_path),
            format: Some(format),
        })
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

    pub fn format(&self) -> Option<FormatId> {
        self.format
    }

    pub fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes(plane_index)
    }

    pub fn open_bytes_into(&mut self, plane_index: u32, destination: &mut [u8]) -> Result<usize> {
        self.inner_mut()?.open_bytes_into(plane_index, destination)
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

    pub fn open_bytes_region_into(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        destination: &mut [u8],
    ) -> Result<usize> {
        self.inner_mut()?
            .open_bytes_region_into(plane_index, x, y, w, h, destination)
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

    pub fn used_files(&self) -> Vec<PathBuf> {
        self.inner()
            .expect("ImageReader not initialized")
            .used_files()
    }

    pub fn resolution_count(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .resolution_count()
    }

    pub fn set_flattened_resolutions(&mut self, flattened: bool) -> Result<()> {
        self.inner_mut()?.set_flattened_resolutions(flattened)
    }

    pub fn flattened_resolutions(&self) -> bool {
        self.inner()
            .expect("ImageReader not initialized")
            .flattened_resolutions()
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
        self.format = None;
        Ok(())
    }
}

impl FormatReader for ImageReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        all_readers()
            .into_iter()
            .any(|(_, reader)| reader.is_this_type_by_name(path))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        all_readers()
            .into_iter()
            .any(|(_, reader)| reader.is_this_type_by_bytes(header))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let header = peek_header(path, 4096)?;

        for (format, mut reader) in all_readers() {
            if reader.is_this_type_by_bytes(&header) {
                reader.set_id(path)?;
                self.inner = Some(reader);
                self.current_path = Some(path.to_path_buf());
                self.format = Some(format);
                return Ok(());
            }
        }

        for (format, mut reader) in all_readers() {
            if reader.is_this_type_by_name(path) {
                reader.set_id(path)?;
                self.inner = Some(reader);
                self.current_path = Some(path.to_path_buf());
                self.format = Some(format);
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

    fn current_file(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    fn used_files(&self) -> Vec<PathBuf> {
        self.inner()
            .map(|inner| inner.used_files())
            .unwrap_or_default()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_bytes(plane_index)
    }

    fn open_bytes_into(&mut self, plane_index: u32, destination: &mut [u8]) -> Result<usize> {
        self.inner_mut()?.open_bytes_into(plane_index, destination)
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

    fn open_bytes_region_into(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        destination: &mut [u8],
    ) -> Result<usize> {
        self.inner_mut()?
            .open_bytes_region_into(plane_index, x, y, w, h, destination)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner_mut()?.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .resolution_count()
    }

    fn set_flattened_resolutions(&mut self, flattened: bool) -> Result<()> {
        self.inner_mut()?.set_flattened_resolutions(flattened)
    }

    fn flattened_resolutions(&self) -> bool {
        self.inner()
            .expect("ImageReader not initialized")
            .flattened_resolutions()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner_mut()?.set_resolution(level)
    }

    fn resolution(&self) -> usize {
        self.inner()
            .expect("ImageReader not initialized")
            .resolution()
    }

    fn snapshot(&self) -> Result<ReaderSnapshot> {
        let current_path = self
            .current_path
            .clone()
            .ok_or(BioFormatsError::NotInitialized)?;
        let inner = self.inner()?.snapshot()?;
        Ok(ReaderSnapshot::ImageReader(ImageReaderSnapshot {
            current_path,
            inner: Box::new(inner),
        }))
    }
}
