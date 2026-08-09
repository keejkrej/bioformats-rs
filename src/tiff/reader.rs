use std::collections::{BTreeMap, HashMap};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use roxmltree::Document;
use serde::{Deserialize, Serialize};

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;
use crate::common::metadata::{
    ChannelMetadata, DimensionOrder, ImageMetadata, MetadataValue, PlaneMetadata,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{validate_region, FormatReader};
use crate::snapshot::ReaderSnapshot;
use crate::source::{CompanionReference, SourceCursor, SourceHandle, SourceInfo, SourceInput};

use super::compression::{decompress, DecompressionOptions};
use super::ifd::{tag, Compression, Ifd, IfdValue, Photometric};
use super::parser::TiffParser;

#[derive(Debug, Clone)]
struct IfdInfo {
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    pixel_type: PixelType,
    compression: Compression,
    photometric: Photometric,
    planar_config: u16,
    predictor: u16,
    is_tiled: bool,
    tile_width: u32,
    tile_height: u32,
    rows_per_strip: u32,
    strip_offsets: Vec<u64>,
    strip_byte_counts: Vec<u64>,
    tile_offsets: Vec<u64>,
    tile_byte_counts: Vec<u64>,
    color_map: Option<(Vec<u16>, Vec<u16>, Vec<u16>)>,
    jpeg_tables: Option<Vec<u8>>,
    image_description: Option<String>,
}

fn returned_samples_per_pixel(info: &IfdInfo) -> u32 {
    u32::from(info.samples_per_pixel)
}

fn stored_samples_per_pixel(info: &IfdInfo) -> u32 {
    if info.planar_config == 2 {
        1
    } else {
        returned_samples_per_pixel(info)
    }
}

fn stored_component_planes(info: &IfdInfo) -> u32 {
    if info.planar_config == 2 {
        returned_samples_per_pixel(info)
    } else {
        1
    }
}

fn same_sample_layout(left: &IfdInfo, right: &IfdInfo) -> bool {
    left.pixel_type == right.pixel_type
        && left.bits_per_sample == right.bits_per_sample
        && left.samples_per_pixel == right.samples_per_pixel
        && left.planar_config == right.planar_config
        && left.photometric == right.photometric
}

fn same_plane_layout(left: &IfdInfo, right: &IfdInfo) -> bool {
    left.width == right.width
        && left.height == right.height
        && same_sample_layout(left, right)
        && left.color_map == right.color_map
}

fn checked_byte_count(factors: &[u64]) -> Result<usize> {
    let count = factors.iter().try_fold(1_u64, |count, factor| {
        count
            .checked_mul(*factor)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)
    })?;
    let count = usize::try_from(count).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    if count > isize::MAX as usize {
        return Err(BioFormatsError::PlaneByteCountOverflow);
    }
    Ok(count)
}

fn checked_index_mul(left: usize, right: usize, context: &str) -> Result<usize> {
    left.checked_mul(right).ok_or_else(|| {
        BioFormatsError::InvalidData(format!("TIFF {context} multiplication overflow"))
    })
}

fn checked_index_add(left: usize, right: usize, context: &str) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| BioFormatsError::InvalidData(format!("TIFF {context} addition overflow")))
}

fn checked_storage_len(value: u64, context: &str) -> Result<usize> {
    let value = usize::try_from(value).map_err(|_| {
        BioFormatsError::InvalidData(format!("TIFF {context} does not fit in memory"))
    })?;
    if value > isize::MAX as usize {
        return Err(BioFormatsError::InvalidData(format!(
            "TIFF {context} does not fit in memory"
        )));
    }
    Ok(value)
}

fn try_byte_buffer(capacity: usize, zeroed: bool) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    if zeroed {
        buffer.resize(capacity, 0);
    }
    Ok(buffer)
}

#[derive(Debug)]
struct TiffFileState {
    path: PathBuf,
    legacy_path: Option<PathBuf>,
    source_info: SourceInfo,
    parser: TiffParser<BufReader<SourceCursor>>,
    ifds: Vec<Ifd>,
    sub_ifds: Vec<Vec<Ifd>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TiffFileSnapshot {
    pub(crate) path: PathBuf,
    pub(crate) little_endian: bool,
    pub(crate) ifds: Vec<Ifd>,
    pub(crate) sub_ifds: Vec<Vec<Ifd>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaneRef {
    file_index: usize,
    ifd_index: usize,
    sub_resolution: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResolutionLevel {
    metadata: ImageMetadata,
    planes: Vec<PlaneRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SeriesData {
    metadata: ImageMetadata,
    resolutions: Vec<ResolutionLevel>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) enum TiffBackendKind {
    Generic,
    Ome,
}

#[derive(Debug)]
struct MinimalTiffReader {
    files: Vec<TiffFileState>,
    current_root_series: usize,
    current_resolution: usize,
    flattened_resolutions: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MinimalTiffReaderSnapshot {
    pub(crate) files: Vec<TiffFileSnapshot>,
    pub(crate) current_root_series: usize,
    pub(crate) current_resolution: usize,
    pub(crate) flattened_resolutions: bool,
}

#[derive(Debug)]
struct BaseTiffReader {
    minimal: MinimalTiffReader,
    series: Vec<SeriesData>,
    used_files: Vec<PathBuf>,
    used_sources: Vec<SourceInfo>,
}

#[derive(Debug)]
struct OmeTiffReader {
    minimal: MinimalTiffReader,
    series: Vec<SeriesData>,
    used_files: Vec<PathBuf>,
    used_sources: Vec<SourceInfo>,
    metadata_file: Option<PathBuf>,
}

#[derive(Debug)]
enum TiffBackend {
    Generic(BaseTiffReader),
    Ome(OmeTiffReader),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TiffReaderSnapshot {
    pub(crate) kind: TiffBackendKind,
    pub(crate) minimal: MinimalTiffReaderSnapshot,
    pub(crate) series: Vec<SeriesData>,
    pub(crate) used_files: Vec<PathBuf>,
    pub(crate) metadata_file: Option<PathBuf>,
}

impl TiffReaderSnapshot {
    pub(crate) fn retarget_primary_path(&mut self, path: &Path) {
        if let Some(first) = self.minimal.files.first_mut() {
            first.path = path.to_path_buf();
        }
        if let Some(first) = self.used_files.first_mut() {
            *first = path.to_path_buf();
        }
    }
}

pub struct TiffReader {
    backend: Option<TiffBackend>,
}

impl TiffReader {
    pub fn new() -> Self {
        Self { backend: None }
    }

    pub fn from_snapshot(snapshot: TiffReaderSnapshot) -> Result<Self> {
        let TiffReaderSnapshot {
            kind,
            minimal,
            series,
            used_files,
            metadata_file,
        } = snapshot;
        let minimal = MinimalTiffReader::from_snapshot(minimal)?;
        let pixel_sources = minimal
            .files
            .iter()
            .map(|file| file.source_info.clone())
            .collect::<Vec<_>>();
        let backend = match kind {
            TiffBackendKind::Generic => TiffBackend::Generic(BaseTiffReader {
                minimal,
                series,
                used_files,
                used_sources: pixel_sources,
            }),
            TiffBackendKind::Ome => {
                let mut used_sources = pixel_sources;
                if let Some(path) = metadata_file.as_ref() {
                    let source = SourceInput::from_path(path)?.primary_handle()?;
                    if !used_sources
                        .iter()
                        .any(|candidate| candidate.identity() == source.info().identity())
                    {
                        used_sources.insert(0, source.info().clone());
                    }
                }
                TiffBackend::Ome(OmeTiffReader {
                    minimal,
                    series,
                    used_files,
                    used_sources,
                    metadata_file,
                })
            }
        };
        Ok(Self {
            backend: Some(backend),
        })
    }

    fn backend(&self) -> Result<&TiffBackend> {
        self.backend.as_ref().ok_or(BioFormatsError::NotInitialized)
    }

    fn backend_mut(&mut self) -> Result<&mut TiffBackend> {
        self.backend.as_mut().ok_or(BioFormatsError::NotInitialized)
    }

    fn series_list(&self) -> Result<&[SeriesData]> {
        Ok(match self.backend()? {
            TiffBackend::Generic(reader) => &reader.series,
            TiffBackend::Ome(reader) => &reader.series,
        })
    }

    fn minimal(&self) -> Result<&MinimalTiffReader> {
        Ok(match self.backend()? {
            TiffBackend::Generic(reader) => &reader.minimal,
            TiffBackend::Ome(reader) => &reader.minimal,
        })
    }

    fn minimal_mut(&mut self) -> Result<&mut MinimalTiffReader> {
        Ok(match self.backend_mut()? {
            TiffBackend::Generic(reader) => &mut reader.minimal,
            TiffBackend::Ome(reader) => &mut reader.minimal,
        })
    }

    fn used_files_ref(&self) -> Result<&[PathBuf]> {
        Ok(match self.backend()? {
            TiffBackend::Generic(reader) => &reader.used_files,
            TiffBackend::Ome(reader) => &reader.used_files,
        })
    }

    fn used_sources_ref(&self) -> Result<&[SourceInfo]> {
        Ok(match self.backend()? {
            TiffBackend::Generic(reader) => &reader.used_sources,
            TiffBackend::Ome(reader) => &reader.used_sources,
        })
    }

    fn active_metadata(&self) -> Result<&ImageMetadata> {
        let minimal = self.minimal()?;
        let series = self.series_list()?;
        let (root, resolution) = minimal.active_indices(series)?;
        Ok(&series[root].resolutions[resolution].metadata)
    }

    fn active_planes(&self) -> Result<&[PlaneRef]> {
        let minimal = self.minimal()?;
        let series = self.series_list()?;
        let (root, resolution) = minimal.active_indices(series)?;
        Ok(&series[root].resolutions[resolution].planes)
    }

    fn generic_from_minimal(minimal: MinimalTiffReader) -> Result<BaseTiffReader> {
        let used_files = minimal
            .files
            .iter()
            .filter_map(|file| file.legacy_path.clone())
            .collect::<Vec<_>>();
        let used_sources = minimal
            .files
            .iter()
            .map(|file| file.source_info.clone())
            .collect::<Vec<_>>();
        let mut reader = BaseTiffReader {
            used_files,
            used_sources,
            series: Vec::new(),
            minimal,
        };
        reader.series = build_generic_series(&reader.minimal, &reader.used_files)?;
        Ok(reader)
    }

    fn ome_from_input(input: SourceInput) -> Result<OmeTiffReader> {
        let primary = input.primary_handle()?;
        let lower = primary.info().name().to_ascii_lowercase();
        let (xml, metadata_source, default_source) = if lower.ends_with(".companion.ome")
            || lower.ends_with(".ome")
        {
            (
                source_text(&primary, "OME metadata")?,
                Some(primary.clone()),
                None,
            )
        } else {
            let minimal = MinimalTiffReader::from_sources(vec![primary.clone()])?;
            let xml = minimal
                .files
                .first()
                .and_then(first_ome_xml)
                .ok_or_else(|| BioFormatsError::Format("TIFF does not contain OME-XML".into()))?;
            if let Some(companion) = binary_only_metadata_file(&xml)? {
                let companion_source =
                    required_companion(&input, &primary, CompanionReference::Named(&companion))?;
                (
                    source_text(&companion_source, "OME companion metadata")?,
                    Some(companion_source),
                    Some(primary.clone()),
                )
            } else {
                (xml, None, Some(primary.clone()))
            }
        };

        let reference_source = metadata_source
            .as_ref()
            .or(default_source.as_ref())
            .ok_or(BioFormatsError::NotInitialized)?;
        let reference_name = Path::new(reference_source.info().name());
        let metadata_base = reference_name.parent().unwrap_or_else(|| Path::new(""));
        let default_file = default_source
            .as_ref()
            .map(|source| Path::new(source.info().name()));
        let parsed = parse_ome_dataset(&xml, metadata_base, default_file)?;

        let mut named_sources = Vec::new();
        for logical_path in &parsed.used_files {
            let source = default_source
                .as_ref()
                .filter(|source| Path::new(source.info().name()) == logical_path)
                .cloned()
                .or_else(|| {
                    (Path::new(primary.info().name()) == logical_path).then(|| primary.clone())
                });
            let source = match source {
                Some(source) => source,
                None => {
                    let reference = logical_path
                        .strip_prefix(metadata_base)
                        .unwrap_or(logical_path)
                        .to_string_lossy();
                    required_companion(
                        &input,
                        reference_source,
                        CompanionReference::Named(&reference),
                    )?
                }
            };
            named_sources.push((logical_path.clone(), source));
        }
        let minimal = MinimalTiffReader::from_named_sources(named_sources)?;

        let mut used_files = Vec::new();
        let mut used_sources = Vec::new();
        if let Some(metadata_source) = metadata_source.as_ref() {
            push_source_once(&mut used_sources, metadata_source.info());
            if let Some(path) = metadata_source.path() {
                used_files.push(path.to_path_buf());
            }
        }
        for file in &minimal.files {
            push_source_once(&mut used_sources, &file.source_info);
            if let Some(path) = file.legacy_path.as_ref() {
                if !used_files.contains(path) {
                    used_files.push(path.clone());
                }
            }
        }
        let metadata_file = metadata_source.and_then(|source| source.path().map(Path::to_path_buf));

        Ok(OmeTiffReader {
            series: build_ome_series(&minimal, &parsed, &used_files)?,
            minimal,
            used_files,
            used_sources,
            metadata_file,
        })
    }
}

fn source_text(source: &SourceHandle, context: &str) -> Result<String> {
    String::from_utf8(source.read_all(context)?).map_err(|error| {
        BioFormatsError::Format(format!(
            "{context} source {} is not UTF-8: {error}",
            source.info().identity()
        ))
    })
}

fn required_companion(
    input: &SourceInput,
    from: &SourceHandle,
    reference: CompanionReference<'_>,
) -> Result<SourceHandle> {
    input
        .resolve(from, reference)?
        .into_iter()
        .next()
        .ok_or_else(|| BioFormatsError::CompanionNotFound {
            identity: from.info().identity().clone(),
            reference: match reference {
                CompanionReference::Named(name) => name.to_owned(),
                CompanionReference::Siblings => "<siblings>".to_owned(),
            },
        })
}

fn push_source_once(sources: &mut Vec<SourceInfo>, source: &SourceInfo) {
    if !sources
        .iter()
        .any(|existing| existing.identity() == source.identity())
    {
        sources.push(source.clone());
    }
}

impl Default for TiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let lower = path.to_string_lossy().to_ascii_lowercase();
        lower.ends_with(".tif")
            || lower.ends_with(".tiff")
            || lower.ends_with(".ome.tif")
            || lower.ends_with(".ome.tiff")
            || lower.ends_with(".tf2")
            || lower.ends_with(".tf8")
            || lower.ends_with(".btf")
            || lower.ends_with(".ome")
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        (header[0..2] == [0x49, 0x49] || header[0..2] == [0x4d, 0x4d])
            && (header[2..4] == [42, 0]
                || header[2..4] == [0, 42]
                || header[2..4] == [43, 0]
                || header[2..4] == [0, 43])
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.set_source(SourceInput::from_path(path)?)
    }

    fn set_source(&mut self, input: SourceInput) -> Result<()> {
        let primary = input.primary_handle()?;
        let lower = primary.info().name().to_ascii_lowercase();
        if lower.ends_with(".companion.ome") || lower.ends_with(".ome") {
            self.backend = Some(TiffBackend::Ome(Self::ome_from_input(input)?));
            return Ok(());
        }
        let minimal = MinimalTiffReader::from_sources(vec![primary])?;
        if minimal.files.first().and_then(first_ome_xml).is_some() {
            self.backend = Some(TiffBackend::Ome(Self::ome_from_input(input)?));
        } else {
            self.backend = Some(TiffBackend::Generic(Self::generic_from_minimal(minimal)?));
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.backend = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        let minimal = self.minimal().expect("TiffReader not initialized");
        minimal
            .series_count(self.series_list().expect("TiffReader not initialized"))
            .expect("active TIFF series state invalid")
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        let series_vec = self.series_list()?.to_vec();
        self.minimal_mut()?.set_series(series, &series_vec)
    }

    fn series(&self) -> usize {
        let minimal = self.minimal().expect("TiffReader not initialized");
        minimal.exposed_series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.active_metadata().expect("TiffReader not initialized")
    }

    fn current_file(&self) -> Option<&Path> {
        self.used_files_ref()
            .ok()
            .and_then(|files| files.first().map(PathBuf::as_path))
    }

    fn used_files(&self) -> Vec<PathBuf> {
        self.used_files_ref()
            .map(|files| files.to_vec())
            .unwrap_or_default()
    }

    fn used_sources(&self) -> Vec<SourceInfo> {
        self.used_sources_ref()
            .map(<[SourceInfo]>::to_vec)
            .unwrap_or_default()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let width = self.size_x();
        let height = self.size_y();
        let plane = self
            .active_planes()?
            .get(plane_index as usize)
            .cloned()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        self.minimal_mut()?.read_plane(&plane, 0, 0, width, height)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        validate_region(self.active_metadata()?, x, y, w, h)?;
        let plane = self
            .active_planes()?
            .get(plane_index as usize)
            .cloned()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        self.minimal_mut()?.read_plane(&plane, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.resolution_count() > 1 {
            let original_resolution = self.resolution();
            let thumbnail_resolution = self.resolution_count() - 1;
            self.set_resolution(thumbnail_resolution)?;
            let bytes = self.open_bytes(plane_index);
            self.set_resolution(original_resolution)?;
            return bytes;
        }

        let tw = self.size_x().min(256);
        let th = self.size_y().min(256);
        let tx = (self.size_x() - tw) / 2;
        let ty = (self.size_y() - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn snapshot(&self) -> Result<ReaderSnapshot> {
        let backend = self.backend()?;
        let snapshot = match backend {
            TiffBackend::Generic(reader) => TiffReaderSnapshot {
                kind: TiffBackendKind::Generic,
                minimal: reader.minimal.snapshot()?,
                series: reader.series.clone(),
                used_files: reader.used_files.clone(),
                metadata_file: None,
            },
            TiffBackend::Ome(reader) => TiffReaderSnapshot {
                kind: TiffBackendKind::Ome,
                minimal: reader.minimal.snapshot()?,
                series: reader.series.clone(),
                used_files: reader.used_files.clone(),
                metadata_file: reader.metadata_file.clone(),
            },
        };
        Ok(ReaderSnapshot::TiffReader(snapshot))
    }

    fn resolution_count(&self) -> usize {
        let minimal = self.minimal().expect("TiffReader not initialized");
        let series = self.series_list().expect("TiffReader not initialized");
        minimal
            .resolution_count(series)
            .expect("active TIFF resolution state invalid")
    }

    fn set_flattened_resolutions(&mut self, flattened: bool) -> Result<()> {
        self.minimal_mut()?.flattened_resolutions = flattened;
        Ok(())
    }

    fn flattened_resolutions(&self) -> bool {
        self.minimal()
            .map(|minimal| minimal.flattened_resolutions)
            .unwrap_or(true)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        let series_vec = self.series_list()?.to_vec();
        self.minimal_mut()?.set_resolution(level, &series_vec)
    }

    fn resolution(&self) -> usize {
        self.minimal()
            .map(|minimal| minimal.exposed_resolution())
            .unwrap_or(0)
    }
}

impl MinimalTiffReader {
    fn from_snapshot(snapshot: MinimalTiffReaderSnapshot) -> Result<Self> {
        let files = snapshot
            .files
            .into_iter()
            .map(TiffFileState::from_snapshot)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            files,
            current_root_series: snapshot.current_root_series,
            current_resolution: snapshot.current_resolution,
            flattened_resolutions: snapshot.flattened_resolutions,
        })
    }

    fn snapshot(&self) -> Result<MinimalTiffReaderSnapshot> {
        Ok(MinimalTiffReaderSnapshot {
            files: self
                .files
                .iter()
                .map(TiffFileState::snapshot)
                .collect::<Result<Vec<_>>>()?,
            current_root_series: self.current_root_series,
            current_resolution: self.current_resolution,
            flattened_resolutions: self.flattened_resolutions,
        })
    }

    fn from_sources(sources: Vec<SourceHandle>) -> Result<Self> {
        let named = sources
            .into_iter()
            .map(|source| (PathBuf::from(source.info().name()), source))
            .collect();
        Self::from_named_sources(named)
    }

    fn from_named_sources(sources: Vec<(PathBuf, SourceHandle)>) -> Result<Self> {
        let files = sources
            .into_iter()
            .map(|(logical_path, source)| TiffFileState::open_as(source, logical_path))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            files,
            current_root_series: 0,
            current_resolution: 0,
            flattened_resolutions: true,
        })
    }

    fn series_count(&self, series: &[SeriesData]) -> Result<usize> {
        if self.flattened_resolutions {
            Ok(series.iter().map(|entry| entry.resolutions.len()).sum())
        } else {
            Ok(series.len())
        }
    }

    fn active_indices(&self, series: &[SeriesData]) -> Result<(usize, usize)> {
        if series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if self.flattened_resolutions {
            let mut exposed = 0usize;
            for (root_index, entry) in series.iter().enumerate() {
                let next = exposed + entry.resolutions.len();
                if self.current_root_series < next {
                    return Ok((root_index, self.current_root_series - exposed));
                }
                exposed = next;
            }
            Err(BioFormatsError::SeriesOutOfRange(self.current_root_series))
        } else {
            let root = self.current_root_series;
            let resolution = self.current_resolution;
            if root >= series.len() {
                return Err(BioFormatsError::SeriesOutOfRange(root));
            }
            if resolution >= series[root].resolutions.len() {
                return Err(BioFormatsError::InvalidData(format!(
                    "resolution {resolution} out of range"
                )));
            }
            Ok((root, resolution))
        }
    }

    fn set_series(&mut self, series_index: usize, series: &[SeriesData]) -> Result<()> {
        if self.flattened_resolutions {
            let total: usize = series.iter().map(|entry| entry.resolutions.len()).sum();
            if series_index >= total {
                return Err(BioFormatsError::SeriesOutOfRange(series_index));
            }
            self.current_root_series = series_index;
            return Ok(());
        }

        if series_index >= series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series_index));
        }
        self.current_root_series = series_index;
        self.current_resolution = 0;
        Ok(())
    }

    fn exposed_series(&self) -> usize {
        self.current_root_series
    }

    fn resolution_count(&self, series: &[SeriesData]) -> Result<usize> {
        if self.flattened_resolutions {
            Ok(1)
        } else {
            let root = self.current_root_series;
            series
                .get(root)
                .map(|entry| entry.resolutions.len())
                .ok_or(BioFormatsError::SeriesOutOfRange(root))
        }
    }

    fn set_resolution(&mut self, level: usize, series: &[SeriesData]) -> Result<()> {
        if self.flattened_resolutions {
            if level == 0 {
                return Ok(());
            }
            return Err(BioFormatsError::InvalidData(
                "cannot set non-zero resolution while flattened".into(),
            ));
        }

        let root = self.current_root_series;
        let count = series
            .get(root)
            .map(|entry| entry.resolutions.len())
            .ok_or(BioFormatsError::SeriesOutOfRange(root))?;
        if level >= count {
            return Err(BioFormatsError::InvalidData(format!(
                "resolution {level} out of range"
            )));
        }
        self.current_resolution = level;
        Ok(())
    }

    fn exposed_resolution(&self) -> usize {
        if self.flattened_resolutions {
            0
        } else {
            self.current_resolution
        }
    }

    fn read_plane(&mut self, plane: &PlaneRef, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let file = self
            .files
            .get_mut(plane.file_index)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane.ifd_index as u32))?;
        file.read_plane(plane, x, y, w, h)
    }
}

impl TiffFileState {
    #[cfg(test)]
    fn open_path(path: &Path) -> Result<Self> {
        Self::open(SourceInput::from_path(path)?.primary_handle()?)
    }

    #[cfg(test)]
    fn open(source: SourceHandle) -> Result<Self> {
        let path = PathBuf::from(source.info().name());
        Self::open_as(source, path)
    }

    fn open_as(source: SourceHandle, path: PathBuf) -> Result<Self> {
        let legacy_path = source.path().map(Path::to_path_buf);
        let source_info = source.info().clone();
        let reader = BufReader::new(source.cursor());
        let mut parser = TiffParser::new(reader)?;
        let ifds = parser.read_ifds()?;
        let sub_ifds = ifds
            .iter()
            .map(|ifd| {
                checked_ifd_vec_u64(ifd, tag::SUB_IFD, "SubIFDs")?
                    .unwrap_or_default()
                    .into_iter()
                    .map(|offset| parser.read_ifd(offset).map(|value| value.0))
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            path,
            legacy_path,
            source_info,
            parser,
            ifds,
            sub_ifds,
        })
    }

    fn snapshot(&self) -> Result<TiffFileSnapshot> {
        let path = self.legacy_path.clone().ok_or_else(|| {
            BioFormatsError::SnapshotUnsupported(
                "TIFF reader initialized from application-provided sources".into(),
            )
        })?;
        Ok(TiffFileSnapshot {
            path,
            little_endian: self.parser.little_endian,
            ifds: self.ifds.clone(),
            sub_ifds: self.sub_ifds.clone(),
        })
    }

    fn from_snapshot(snapshot: TiffFileSnapshot) -> Result<Self> {
        let source = SourceInput::from_path(&snapshot.path)?.primary_handle()?;
        let source_info = source.info().clone();
        let reader = BufReader::new(source.cursor());
        let mut parser = TiffParser::new(reader)?;
        parser.little_endian = snapshot.little_endian;
        Ok(Self {
            path: snapshot.path.clone(),
            legacy_path: Some(snapshot.path),
            source_info,
            parser,
            ifds: snapshot.ifds,
            sub_ifds: snapshot.sub_ifds,
        })
    }

    fn read_plane(&mut self, plane: &PlaneRef, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let ifd = if plane.sub_resolution == 0 {
            self.ifds
                .get(plane.ifd_index)
                .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane.ifd_index as u32))?
        } else {
            self.sub_ifds
                .get(plane.ifd_index)
                .and_then(|levels| levels.get(plane.sub_resolution - 1))
                .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane.ifd_index as u32))?
        };
        let info = ifd_info(ifd, self.parser.little_endian)?;
        let bytes_per_sample = u64::from(info.bits_per_sample).div_ceil(8);
        if bytes_per_sample == 0 || info.samples_per_pixel == 0 {
            return Err(BioFormatsError::InvalidData(
                "TIFF sample width and SamplesPerPixel must be non-zero".into(),
            ));
        }
        let right = x.checked_add(w);
        let bottom = y.checked_add(h);
        if w == 0
            || h == 0
            || right.is_none_or(|right| right > info.width)
            || bottom.is_none_or(|bottom| bottom > info.height)
        {
            return Err(BioFormatsError::InvalidData(
                "TIFF read region is empty, overflows, or exceeds the IFD dimensions".into(),
            ));
        }
        let plane_byte_len = checked_byte_count(&[
            u64::from(w),
            u64::from(h),
            u64::from(returned_samples_per_pixel(&info)),
            bytes_per_sample,
        ])?;
        if info.is_tiled {
            self.read_tiled_plane(&info, x, y, w, h, plane_byte_len)
        } else {
            self.read_stripped_plane(&info, x, y, w, h, plane_byte_len)
        }
    }

    fn read_stripped_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        plane_byte_len: usize,
    ) -> Result<Vec<u8>> {
        let bytes_per_sample = u64::from(info.bits_per_sample).div_ceil(8);
        let stored_spp = u64::from(stored_samples_per_pixel(info));
        let component_planes = stored_component_planes(info);
        let row_bytes = checked_byte_count(&[u64::from(info.width), stored_spp, bytes_per_sample])?;
        let copy_bytes = checked_byte_count(&[u64::from(w), stored_spp, bytes_per_sample])?;
        let rows_per_strip = if info.rows_per_strip == 0 || info.rows_per_strip >= info.height {
            info.height
        } else {
            info.rows_per_strip
        };
        let strips_per_component = info.height.div_ceil(rows_per_strip);
        let region_bottom = y.checked_add(h).ok_or_else(|| {
            BioFormatsError::InvalidData("TIFF region bottom overflows u32".into())
        })?;
        let x_start = checked_byte_count(&[u64::from(x), stored_spp, bytes_per_sample])?;
        let mut out = try_byte_buffer(plane_byte_len, false)?;

        for component in 0..component_planes {
            for strip in 0..strips_per_component {
                let strip_start_row = strip.checked_mul(rows_per_strip).ok_or_else(|| {
                    BioFormatsError::InvalidData("TIFF strip row index overflows u32".into())
                })?;
                let strip_end_row = strip_start_row
                    .saturating_add(rows_per_strip)
                    .min(info.height);
                if strip_end_row <= y || strip_start_row >= region_bottom {
                    continue;
                }

                let component_offset = checked_index_mul(
                    component as usize,
                    strips_per_component as usize,
                    "strip component offset",
                )?;
                let strip_index =
                    checked_index_add(component_offset, strip as usize, "strip index")?;
                let offset = *info.strip_offsets.get(strip_index).ok_or_else(|| {
                    BioFormatsError::InvalidData(format!(
                        "TIFF strip offset {strip_index} is missing"
                    ))
                })?;
                let byte_count = *info.strip_byte_counts.get(strip_index).ok_or_else(|| {
                    BioFormatsError::InvalidData(format!(
                        "TIFF strip byte count {strip_index} is missing"
                    ))
                })?;
                let byte_count = checked_storage_len(byte_count, "strip byte count")?;
                let strip_rows = strip_end_row - strip_start_row;
                // A final strip may be encoded at the declared RowsPerStrip
                // height even though only its in-image rows are copied.
                let decode_limit = checked_byte_count(&[rows_per_strip.into(), row_bytes as u64])?;
                let required = checked_byte_count(&[strip_rows.into(), row_bytes as u64])?;
                if info.compression == Compression::None
                    && !(required..=decode_limit).contains(&byte_count)
                {
                    return Err(BioFormatsError::InvalidData(format!(
                        "uncompressed TIFF strip {strip_index} has {byte_count} bytes; expected between {required} and {decode_limit}"
                    )));
                }
                let compressed = read_bytes_at(&mut self.parser.reader, offset, byte_count)?;
                let strip_data = decompress(
                    &compressed,
                    info.compression,
                    DecompressionOptions {
                        expected_len: decode_limit,
                        predictor: info.predictor,
                        samples_per_pixel: stored_spp as u16,
                        bits_per_sample: info.bits_per_sample,
                        row_width: info.width,
                        little_endian: self.parser.little_endian,
                        jpeg_tables: info.jpeg_tables.as_deref(),
                    },
                )?;
                if strip_data.len() < required {
                    return Err(BioFormatsError::InvalidData(format!(
                        "TIFF strip {strip_index} decoded to {} bytes; expected at least {required}",
                        strip_data.len()
                    )));
                }

                let first_row = y.max(strip_start_row);
                let last_row = region_bottom.min(strip_end_row);
                for source_y in first_row..last_row {
                    let row = (source_y - strip_start_row) as usize;
                    let start = checked_index_add(
                        checked_index_mul(row, row_bytes, "strip row offset")?,
                        x_start,
                        "strip column offset",
                    )?;
                    let end = checked_index_add(start, copy_bytes, "strip row end")?;
                    if end > strip_data.len() {
                        return Err(BioFormatsError::InvalidData(format!(
                            "TIFF strip {strip_index} region exceeds decoded data"
                        )));
                    }
                    let output_end =
                        checked_index_add(out.len(), copy_bytes, "strip output position")?;
                    if output_end > plane_byte_len {
                        return Err(BioFormatsError::InvalidData(
                            "TIFF strips exceed the expected plane byte count".into(),
                        ));
                    }
                    out.extend_from_slice(&strip_data[start..end]);
                }
            }
        }

        if out.len() != plane_byte_len {
            return Err(BioFormatsError::InvalidData(format!(
                "TIFF strips produced {} bytes; expected {plane_byte_len}",
                out.len()
            )));
        }
        Ok(out)
    }

    fn read_tiled_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        plane_byte_len: usize,
    ) -> Result<Vec<u8>> {
        let bytes_per_sample = u64::from(info.bits_per_sample).div_ceil(8);
        let stored_spp = u64::from(stored_samples_per_pixel(info));
        let component_planes = stored_component_planes(info);
        if info.tile_width == 0 || info.tile_height == 0 {
            return Err(BioFormatsError::InvalidData(
                "TIFF tiled image has a zero tile dimension".into(),
            ));
        }
        let tile_row_bytes =
            checked_byte_count(&[u64::from(info.tile_width), stored_spp, bytes_per_sample])?;
        let tile_data_bytes =
            checked_byte_count(&[u64::from(info.tile_height), tile_row_bytes as u64])?;
        let tiles_across = info.width.div_ceil(info.tile_width);
        let tiles_down = info.height.div_ceil(info.tile_height);
        let tiles_per_component = checked_index_mul(
            tiles_across as usize,
            tiles_down as usize,
            "tiles per component",
        )?;
        let tx_start = x / info.tile_width;
        let region_right = x.checked_add(w).ok_or_else(|| {
            BioFormatsError::InvalidData("TIFF region right edge overflows u32".into())
        })?;
        let tx_end = region_right.div_ceil(info.tile_width);
        let ty_start = y / info.tile_height;
        let region_bottom = y.checked_add(h).ok_or_else(|| {
            BioFormatsError::InvalidData("TIFF region bottom edge overflows u32".into())
        })?;
        let ty_end = region_bottom.div_ceil(info.tile_height);
        let out_row_bytes = checked_byte_count(&[u64::from(w), stored_spp, bytes_per_sample])?;
        let out_component_bytes = checked_byte_count(&[u64::from(h), out_row_bytes as u64])?;
        let computed_plane_bytes =
            checked_byte_count(&[u64::from(component_planes), out_component_bytes as u64])?;
        if computed_plane_bytes != plane_byte_len {
            return Err(BioFormatsError::InvalidData(
                "TIFF tiled plane layout does not match its byte count".into(),
            ));
        }
        let mut out = try_byte_buffer(plane_byte_len, true)?;

        for component in 0..component_planes {
            for ty in ty_start..ty_end {
                for tx in tx_start..tx_end {
                    let component_offset = checked_index_mul(
                        component as usize,
                        tiles_per_component,
                        "tile component offset",
                    )?;
                    let tile_row_offset =
                        checked_index_mul(ty as usize, tiles_across as usize, "tile row offset")?;
                    let tile_in_component =
                        checked_index_add(tile_row_offset, tx as usize, "tile column offset")?;
                    let tile_index =
                        checked_index_add(component_offset, tile_in_component, "tile index")?;
                    let offset = *info.tile_offsets.get(tile_index).ok_or_else(|| {
                        BioFormatsError::InvalidData(format!(
                            "TIFF tile offset {tile_index} is missing"
                        ))
                    })?;
                    let byte_count = *info.tile_byte_counts.get(tile_index).ok_or_else(|| {
                        BioFormatsError::InvalidData(format!(
                            "TIFF tile byte count {tile_index} is missing"
                        ))
                    })?;
                    let byte_count = checked_storage_len(byte_count, "tile byte count")?;
                    if info.compression == Compression::None && byte_count > tile_data_bytes {
                        return Err(BioFormatsError::InvalidData(format!(
                            "uncompressed TIFF tile {tile_index} has {byte_count} bytes; maximum is {tile_data_bytes}"
                        )));
                    }
                    let compressed = read_bytes_at(&mut self.parser.reader, offset, byte_count)?;
                    let tile_data = decompress(
                        &compressed,
                        info.compression,
                        DecompressionOptions {
                            expected_len: tile_data_bytes,
                            predictor: info.predictor,
                            samples_per_pixel: stored_spp as u16,
                            bits_per_sample: info.bits_per_sample,
                            row_width: info.tile_width,
                            little_endian: self.parser.little_endian,
                            jpeg_tables: info.jpeg_tables.as_deref(),
                        },
                    )?;
                    let tile_x0 = tx.checked_mul(info.tile_width).ok_or_else(|| {
                        BioFormatsError::InvalidData("TIFF tile X offset overflows u32".into())
                    })?;
                    let tile_y0 = ty.checked_mul(info.tile_height).ok_or_else(|| {
                        BioFormatsError::InvalidData("TIFF tile Y offset overflows u32".into())
                    })?;
                    let stored_width = info.width.saturating_sub(tile_x0).min(info.tile_width);
                    let stored_height = info.height.saturating_sub(tile_y0).min(info.tile_height);
                    let stored_row_bytes = checked_byte_count(&[
                        u64::from(stored_width),
                        stored_spp,
                        bytes_per_sample,
                    ])?;
                    let required_tile_bytes = if stored_height == 0 {
                        0
                    } else {
                        checked_index_add(
                            checked_index_mul(
                                stored_height.saturating_sub(1) as usize,
                                tile_row_bytes,
                                "TIFF stored tile row offset",
                            )?,
                            stored_row_bytes,
                            "TIFF stored tile end",
                        )?
                    };
                    if tile_data.len() < required_tile_bytes {
                        return Err(BioFormatsError::InvalidData(format!(
                            "TIFF tile {tile_index} decoded to {} bytes; at least {required_tile_bytes} bytes are required for its in-image pixels",
                            tile_data.len()
                        )));
                    }
                    let copy_x0 = x.max(tile_x0);
                    let copy_y0 = y.max(tile_y0);
                    let copy_x1 = region_right
                        .min(tile_x0.saturating_add(info.tile_width))
                        .min(info.width);
                    let copy_y1 = region_bottom
                        .min(tile_y0.saturating_add(info.tile_height))
                        .min(info.height);
                    let src_x = (copy_x0 - tile_x0) as usize;
                    let src_y = (copy_y0 - tile_y0) as usize;
                    let dst_x = (copy_x0 - x) as usize;
                    let dst_y = (copy_y0 - y) as usize;
                    let copy_w = (copy_x1 - copy_x0) as usize;
                    let copy_h = (copy_y1 - copy_y0) as usize;
                    let copy_bytes =
                        checked_byte_count(&[copy_w as u64, stored_spp, bytes_per_sample])?;
                    for row in 0..copy_h {
                        let src_row = checked_index_add(src_y, row, "tile source row")?;
                        let src_row_offset =
                            checked_index_mul(src_row, tile_row_bytes, "tile source row offset")?;
                        let src_column_offset =
                            checked_byte_count(&[src_x as u64, stored_spp, bytes_per_sample])?;
                        let src_off = checked_index_add(
                            src_row_offset,
                            src_column_offset,
                            "tile source offset",
                        )?;

                        let component_offset = checked_index_mul(
                            component as usize,
                            out_component_bytes,
                            "tile output component offset",
                        )?;
                        let dst_row = checked_index_add(dst_y, row, "tile output row")?;
                        let dst_row_offset =
                            checked_index_mul(dst_row, out_row_bytes, "tile output row offset")?;
                        let dst_column_offset =
                            checked_byte_count(&[dst_x as u64, stored_spp, bytes_per_sample])?;
                        let dst_off = checked_index_add(
                            checked_index_add(
                                component_offset,
                                dst_row_offset,
                                "tile output row position",
                            )?,
                            dst_column_offset,
                            "tile output column position",
                        )?;
                        let src_end = checked_index_add(src_off, copy_bytes, "tile source end")?;
                        let dst_end = checked_index_add(dst_off, copy_bytes, "tile output end")?;
                        if src_end > tile_data.len() || dst_end > out.len() {
                            return Err(BioFormatsError::InvalidData(format!(
                                "TIFF tile {tile_index} copy exceeds decoded or output data"
                            )));
                        }
                        out[dst_off..dst_end].copy_from_slice(&tile_data[src_off..src_end]);
                    }
                }
            }
        }

        Ok(out)
    }
}

fn checked_ifd_u32(ifd: &Ifd, tag_id: u16, name: &str) -> Result<Option<u32>> {
    let Some(value) = ifd.get(tag_id) else {
        return Ok(None);
    };
    let value = value.as_u64().ok_or_else(|| {
        BioFormatsError::InvalidData(format!("TIFF {name} is not an unsigned integer"))
    })?;
    Ok(Some(u32::try_from(value).map_err(|_| {
        BioFormatsError::InvalidData(format!("TIFF {name} value {value} exceeds u32"))
    })?))
}

fn checked_ifd_u16(ifd: &Ifd, tag_id: u16, name: &str) -> Result<Option<u16>> {
    let Some(value) = ifd.get(tag_id) else {
        return Ok(None);
    };
    let value = value.as_u64().ok_or_else(|| {
        BioFormatsError::InvalidData(format!("TIFF {name} is not an unsigned integer"))
    })?;
    Ok(Some(u16::try_from(value).map_err(|_| {
        BioFormatsError::InvalidData(format!("TIFF {name} value {value} exceeds u16"))
    })?))
}

fn checked_ifd_vec_u64(ifd: &Ifd, tag_id: u16, name: &str) -> Result<Option<Vec<u64>>> {
    let Some(value) = ifd.get(tag_id) else {
        return Ok(None);
    };
    let length = match value {
        IfdValue::Byte(values) => values.len(),
        IfdValue::Short(values) => values.len(),
        IfdValue::Long(values) | IfdValue::IFD(values) => values.len(),
        IfdValue::Long8(values) | IfdValue::IFD8(values) => values.len(),
        _ => {
            return Err(BioFormatsError::InvalidData(format!(
                "TIFF {name} is not an unsigned integer array"
            )))
        }
    };
    if length == 0 {
        return Err(BioFormatsError::InvalidData(format!(
            "TIFF {name} is present but empty"
        )));
    }
    let mut values = Vec::new();
    values.try_reserve_exact(length).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate TIFF {name} values: {error}"))
    })?;
    match value {
        IfdValue::Byte(items) => values.extend(items.iter().copied().map(u64::from)),
        IfdValue::Short(items) => values.extend(items.iter().copied().map(u64::from)),
        IfdValue::Long(items) | IfdValue::IFD(items) => {
            values.extend(items.iter().copied().map(u64::from));
        }
        IfdValue::Long8(items) | IfdValue::IFD8(items) => values.extend_from_slice(items),
        _ => unreachable!("validated unsigned TIFF array type"),
    }
    Ok(Some(values))
}

fn checked_ifd_vec_u16(ifd: &Ifd, tag_id: u16, name: &str) -> Result<Option<Vec<u16>>> {
    let Some(raw) = checked_ifd_vec_u64(ifd, tag_id, name)? else {
        return Ok(None);
    };
    let mut values = Vec::new();
    values.try_reserve_exact(raw.len()).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate TIFF {name} values: {error}"))
    })?;
    for item in raw {
        values.push(u16::try_from(item).map_err(|_| {
            BioFormatsError::InvalidData(format!("TIFF {name} value {item} exceeds u16"))
        })?);
    }
    Ok(Some(values))
}

fn ifd_info(ifd: &Ifd, _little_endian: bool) -> Result<IfdInfo> {
    let width = checked_ifd_u32(ifd, tag::IMAGE_WIDTH, "ImageWidth")?
        .ok_or_else(|| BioFormatsError::Format("IFD missing ImageWidth".into()))?;
    let height = checked_ifd_u32(ifd, tag::IMAGE_LENGTH, "ImageLength")?
        .ok_or_else(|| BioFormatsError::Format("IFD missing ImageLength".into()))?;
    let samples_per_pixel =
        checked_ifd_u16(ifd, tag::SAMPLES_PER_PIXEL, "SamplesPerPixel")?.unwrap_or(1);
    if samples_per_pixel == 0 {
        return Err(BioFormatsError::Format(
            "TIFF SamplesPerPixel must be positive".into(),
        ));
    }
    let bps_vec =
        checked_ifd_vec_u16(ifd, tag::BITS_PER_SAMPLE, "BitsPerSample")?.unwrap_or_else(|| vec![1]);
    let bits_per_sample = bps_vec.first().copied().unwrap_or(8);
    if bps_vec.iter().any(|bits| *bits != bits_per_sample)
        || (bps_vec.len() > 1 && bps_vec.len() != samples_per_pixel as usize)
    {
        return Err(BioFormatsError::UnsupportedFormat(
            "TIFF: mixed or inconsistent BitsPerSample values are not yet supported".into(),
        ));
    }
    let sample_formats =
        checked_ifd_vec_u16(ifd, tag::SAMPLE_FORMAT, "SampleFormat")?.unwrap_or_default();
    let sample_format = sample_formats.first().copied().unwrap_or(1);
    if sample_formats.iter().any(|format| *format != sample_format)
        || (sample_formats.len() > 1 && sample_formats.len() != samples_per_pixel as usize)
    {
        return Err(BioFormatsError::UnsupportedFormat(
            "TIFF: mixed or inconsistent SampleFormat values are not yet supported".into(),
        ));
    }
    let pixel_type = pixel_type_from_bps_format(bits_per_sample, sample_format)?;
    let photometric = Photometric::from(
        checked_ifd_u16(
            ifd,
            tag::PHOTOMETRIC_INTERPRETATION,
            "PhotometricInterpretation",
        )?
        .unwrap_or(1),
    );
    let compression =
        Compression::from(checked_ifd_u16(ifd, tag::COMPRESSION, "Compression")?.unwrap_or(1));
    let planar_config =
        checked_ifd_u16(ifd, tag::PLANAR_CONFIGURATION, "PlanarConfiguration")?.unwrap_or(1);
    if !matches!(planar_config, 1 | 2) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "TIFF: unsupported PlanarConfiguration {planar_config}"
        )));
    }
    let predictor = checked_ifd_u16(ifd, tag::PREDICTOR, "Predictor")?.unwrap_or(1);
    if predictor != 1 && !(predictor == 2 && matches!(bits_per_sample, 8 | 16)) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "TIFF: Predictor {predictor} with {bits_per_sample}-bit samples is not yet supported"
        )));
    }
    if checked_ifd_u16(ifd, tag::FILL_ORDER, "FillOrder")?.unwrap_or(1) != 1 {
        return Err(BioFormatsError::UnsupportedFormat(
            "TIFF: FillOrder 2 is not yet supported".into(),
        ));
    }
    match photometric {
        Photometric::MinIsBlack | Photometric::Rgb | Photometric::Palette => {}
        Photometric::YCbCr if matches!(compression, Compression::Jpeg | Compression::JpegNew) => {}
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TIFF: photometric interpretation {other:?} is not yet normalized"
            )))
        }
    }
    let is_tiled = ifd.is_tiled();
    let (tile_width, tile_height) = if is_tiled {
        (
            checked_ifd_u32(ifd, tag::TILE_WIDTH, "TileWidth")?.unwrap_or(0),
            checked_ifd_u32(ifd, tag::TILE_LENGTH, "TileLength")?.unwrap_or(0),
        )
    } else {
        (0, 0)
    };
    let rows_per_strip = if is_tiled {
        0
    } else {
        checked_ifd_u32(ifd, tag::ROWS_PER_STRIP, "RowsPerStrip")?.unwrap_or(height)
    };
    let strip_offsets =
        checked_ifd_vec_u64(ifd, tag::STRIP_OFFSETS, "StripOffsets")?.unwrap_or_default();
    let strip_byte_counts =
        checked_ifd_vec_u64(ifd, tag::STRIP_BYTE_COUNTS, "StripByteCounts")?.unwrap_or_default();
    let tile_offsets =
        checked_ifd_vec_u64(ifd, tag::TILE_OFFSETS, "TileOffsets")?.unwrap_or_default();
    let tile_byte_counts =
        checked_ifd_vec_u64(ifd, tag::TILE_BYTE_COUNTS, "TileByteCounts")?.unwrap_or_default();
    let color_map = if photometric == Photometric::Palette {
        if let Some(data) = checked_ifd_vec_u16(ifd, tag::COLOR_MAP, "ColorMap")? {
            let expected_color_map_length = 1_usize
                .checked_shl(u32::from(bits_per_sample))
                .and_then(|entries| entries.checked_mul(3))
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            if data.len() != expected_color_map_length {
                return Err(BioFormatsError::InvalidData(
                    format!(
                        "TIFF ColorMap has {} entries; expected {expected_color_map_length} for {bits_per_sample}-bit indices",
                        data.len()
                    ),
                ));
            }
            let third = data.len() / 3;
            Some((
                data[..third].to_vec(),
                data[third..2 * third].to_vec(),
                data[2 * third..].to_vec(),
            ))
        } else {
            None
        }
    } else {
        None
    };
    let jpeg_tables = ifd.get(tag::JPEG_TABLES).and_then(|value| match value {
        super::ifd::IfdValue::Undefined(bytes) => Some(bytes.clone()),
        _ => None,
    });
    let image_description = ifd.get_str(tag::IMAGE_DESCRIPTION).map(str::to_owned);

    Ok(IfdInfo {
        width,
        height,
        samples_per_pixel,
        bits_per_sample,
        pixel_type,
        compression,
        photometric,
        planar_config,
        predictor,
        is_tiled,
        tile_width,
        tile_height,
        rows_per_strip,
        strip_offsets,
        strip_byte_counts,
        tile_offsets,
        tile_byte_counts,
        color_map,
        jpeg_tables,
        image_description,
    })
}

fn pixel_type_from_bps_format(bps: u16, sample_format: u16) -> Result<PixelType> {
    let pixel_type = match (bps, sample_format) {
        (8, 2) => PixelType::Int8,
        (8, 1 | 4) => PixelType::Uint8,
        (16, 2) => PixelType::Int16,
        (16, 1 | 4) => PixelType::Uint16,
        (32, 2) => PixelType::Int32,
        (32, 3) => PixelType::Float32,
        (32, 1 | 4) => PixelType::Uint32,
        (64, 3) => PixelType::Float64,
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TIFF: {bps}-bit samples with SampleFormat {sample_format} are not yet supported"
            )))
        }
    };
    Ok(pixel_type)
}

fn first_ome_xml(file: &TiffFileState) -> Option<String> {
    file.ifds
        .first()
        .and_then(|ifd| ifd.get_str(tag::IMAGE_DESCRIPTION))
        .map(str::trim)
        .filter(|value| {
            value.starts_with("<?xml") || value.starts_with("<OME") || value.contains("<OME ")
        })
        .map(str::to_owned)
}

fn binary_only_metadata_file(xml: &str) -> Result<Option<String>> {
    let document = Document::parse(xml)
        .map_err(|error| BioFormatsError::Format(format!("invalid OME-XML: {error}")))?;
    Ok(document
        .descendants()
        .find(|node| node.has_tag_name("BinaryOnly"))
        .and_then(|node| node.attribute("MetadataFile"))
        .map(str::to_owned))
}

fn build_generic_series(
    minimal: &MinimalTiffReader,
    used_files: &[PathBuf],
) -> Result<Vec<SeriesData>> {
    let file = minimal
        .files
        .first()
        .ok_or_else(|| BioFormatsError::Format("missing TIFF file".into()))?;
    let little_endian = file.parser.little_endian;
    let infos = file
        .ifds
        .iter()
        .enumerate()
        .map(|(index, ifd)| ifd_info(ifd, little_endian).map(|info| (index, info)))
        .collect::<Result<Vec<_>>>()?;
    if infos.is_empty() {
        return Ok(Vec::new());
    }

    let mut groups: Vec<Vec<(usize, IfdInfo)>> = Vec::new();
    for (index, info) in infos {
        if let Some(last) = groups.last_mut() {
            let previous = &last.last().unwrap().1;
            if same_plane_layout(previous, &info) {
                last.push((index, info));
                continue;
            }
        }
        groups.push(vec![(index, info)]);
    }

    groups
        .into_iter()
        .map(|group| {
            let first = &group[0].1;
            let plane_refs = group
                .iter()
                .map(|(index, _)| PlaneRef {
                    file_index: 0,
                    ifd_index: *index,
                    sub_resolution: 0,
                })
                .collect::<Vec<_>>();
            let mut metadata =
                generic_metadata_from_info(first, plane_refs.len() as u32, used_files);
            apply_standard_tiff_metadata(
                &mut metadata,
                &file.ifds[group[0].0],
                first,
                little_endian,
            )?;
            let resolutions = build_resolution_levels(minimal, plane_refs, metadata.clone())?;
            metadata = resolutions[0].metadata.clone();
            Ok(SeriesData {
                metadata,
                resolutions,
            })
        })
        .collect()
}

fn generic_metadata_from_info(
    info: &IfdInfo,
    image_count: u32,
    used_files: &[PathBuf],
) -> ImageMetadata {
    let is_rgb = matches!(info.photometric, Photometric::Rgb | Photometric::YCbCr)
        && info.samples_per_pixel >= 3;
    let is_indexed = info.photometric == Photometric::Palette;
    let mut metadata = ImageMetadata {
        size_x: info.width,
        size_y: info.height,
        size_z: image_count,
        size_c: if is_rgb {
            info.samples_per_pixel as u32
        } else {
            1
        },
        size_t: 1,
        pixel_type: info.pixel_type,
        bits_per_pixel: info.bits_per_sample as u8,
        samples_per_pixel: returned_samples_per_pixel(info),
        image_count,
        dimension_order: DimensionOrder::XYZTC,
        is_rgb,
        is_interleaved: info.planar_config == 1,
        is_indexed,
        is_false_color: true,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: info.color_map.as_ref().map(|(red, green, blue)| {
            crate::common::metadata::LookupTable {
                red: red.clone(),
                green: green.clone(),
                blue: blue.clone(),
            }
        }),
        used_files: used_files.to_vec(),
        ..ImageMetadata::default()
    };
    if let Some(description) = &info.image_description {
        metadata.series_metadata.insert(
            "ImageDescription".into(),
            MetadataValue::String(description.clone()),
        );
    }
    metadata
}

fn apply_standard_tiff_metadata(
    metadata: &mut ImageMetadata,
    ifd: &Ifd,
    info: &IfdInfo,
    little_endian: bool,
) -> Result<()> {
    if let Some(software) = ifd.get_str(tag::SOFTWARE) {
        metadata.series_metadata.insert(
            "Software".into(),
            MetadataValue::String(software.to_string()),
        );
    }
    if let Some(datetime) = ifd.get_str(tag::DATE_TIME) {
        metadata.acquisition_timestamp = Some(datetime.to_string());
        metadata.series_metadata.insert(
            "DateTime".into(),
            MetadataValue::String(datetime.to_string()),
        );
    }
    let resolution_unit =
        checked_ifd_u16(ifd, tag::RESOLUTION_UNIT, "ResolutionUnit")?.unwrap_or(1);
    let resolution_multiplier_um = match resolution_unit {
        2 => 25_400.0,
        3 => 10_000.0,
        _ => 1.0,
    };
    metadata.series_metadata.insert(
        "ResolutionUnit".into(),
        MetadataValue::Int(i64::from(resolution_unit)),
    );
    if let Some(resolution) = rational_to_f64(ifd.get(tag::X_RESOLUTION)) {
        metadata
            .series_metadata
            .insert("XResolution".into(), MetadataValue::Float(resolution));
        if resolution > 0.0 {
            metadata.physical_size_x_um = Some(resolution_multiplier_um / resolution);
        }
    }
    if let Some(resolution) = rational_to_f64(ifd.get(tag::Y_RESOLUTION)) {
        metadata
            .series_metadata
            .insert("YResolution".into(), MetadataValue::Float(resolution));
        if resolution > 0.0 {
            metadata.physical_size_y_um = Some(resolution_multiplier_um / resolution);
        }
    }
    metadata.is_little_endian = little_endian;
    metadata.lookup_table =
        info.color_map
            .as_ref()
            .map(|(red, green, blue)| crate::common::metadata::LookupTable {
                red: red.clone(),
                green: green.clone(),
                blue: blue.clone(),
            });
    Ok(())
}

fn rational_to_f64(value: Option<&super::ifd::IfdValue>) -> Option<f64> {
    match value {
        Some(super::ifd::IfdValue::Rational(values)) if !values.is_empty() => {
            let (numerator, denominator) = values[0];
            (denominator != 0).then_some(numerator as f64 / denominator as f64)
        }
        _ => None,
    }
}

fn build_resolution_levels(
    minimal: &MinimalTiffReader,
    root_planes: Vec<PlaneRef>,
    root_metadata: ImageMetadata,
) -> Result<Vec<ResolutionLevel>> {
    let mut levels = vec![ResolutionLevel {
        metadata: root_metadata.clone(),
        planes: root_planes.clone(),
    }];
    let sub_count = root_planes
        .iter()
        .map(|plane| minimal.files[plane.file_index].sub_ifds[plane.ifd_index].len())
        .min()
        .unwrap_or(0);
    let root_sample_plane = root_planes
        .first()
        .ok_or_else(|| BioFormatsError::Format("TIFF series contains no root planes".into()))?;
    let root_sample_file = &minimal.files[root_sample_plane.file_index];
    let root_sample_ifd = root_sample_file
        .ifds
        .get(root_sample_plane.ifd_index)
        .ok_or_else(|| BioFormatsError::Format("missing TIFF root IFD".into()))?;
    let root_info = ifd_info(root_sample_ifd, root_sample_file.parser.little_endian)?;

    for level in 0..sub_count {
        let sample_ifd = minimal.files[root_planes[0].file_index].sub_ifds
            [root_planes[0].ifd_index]
            .get(level)
            .ok_or_else(|| BioFormatsError::Format("missing SubIFD".into()))?;
        let info = ifd_info(
            sample_ifd,
            minimal.files[root_planes[0].file_index]
                .parser
                .little_endian,
        )?;
        let sample_little_endian = minimal.files[root_planes[0].file_index]
            .parser
            .little_endian;
        if !same_sample_layout(&root_info, &info)
            || sample_little_endian != root_sample_file.parser.little_endian
        {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TIFF SubIFD resolution {} changes the root pixel layout",
                level + 1
            )));
        }
        for (plane_index, plane) in root_planes.iter().enumerate() {
            let file = minimal.files.get(plane.file_index).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "TIFF pyramid plane {plane_index} references a missing file"
                ))
            })?;
            let sub_ifd = file
                .sub_ifds
                .get(plane.ifd_index)
                .and_then(|levels| levels.get(level))
                .ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "TIFF pyramid plane {plane_index} is missing SubIFD level {}",
                        level + 1
                    ))
                })?;
            let plane_info = ifd_info(sub_ifd, file.parser.little_endian)?;
            if !same_plane_layout(&info, &plane_info)
                || file.parser.little_endian != sample_little_endian
            {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "TIFF pyramid plane {plane_index} has a different layout at resolution {}",
                    level + 1
                )));
            }
        }
        let mut metadata = root_metadata.clone();
        metadata.size_x = info.width;
        metadata.size_y = info.height;
        metadata.lookup_table = info.color_map.as_ref().map(|(red, green, blue)| {
            crate::common::metadata::LookupTable {
                red: red.clone(),
                green: green.clone(),
                blue: blue.clone(),
            }
        });
        metadata.resolution_count = (sub_count + 1) as u32;
        let planes = root_planes
            .iter()
            .map(|plane| PlaneRef {
                file_index: plane.file_index,
                ifd_index: plane.ifd_index,
                sub_resolution: level + 1,
            })
            .collect();
        levels.push(ResolutionLevel { metadata, planes });
    }

    let level_count = levels.len() as u32;
    for level in &mut levels {
        level.metadata.resolution_count = level_count;
    }
    Ok(levels)
}

#[derive(Debug)]
struct OmeDataset {
    images: Vec<OmeImage>,
    used_files: Vec<PathBuf>,
}

#[derive(Debug)]
struct OmeImage {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c_samples: u32,
    size_t: u32,
    pixel_type: PixelType,
    significant_bits: u8,
    dimension_order: DimensionOrder,
    channels: Vec<ChannelMetadata>,
    channel_samples_per_pixel: Vec<u32>,
    planes: Vec<PlaneMetadata>,
    tiff_data: Vec<OmeTiffData>,
    physical_size_x_um: Option<f64>,
    physical_size_y_um: Option<f64>,
    physical_size_z_um: Option<f64>,
    time_increment_seconds: Option<f64>,
    acquisition_timestamp: Option<String>,
    objective_model: Option<String>,
    objective_magnification: Option<f64>,
    objective_na: Option<f64>,
}

#[derive(Debug, Clone)]
struct OmeTiffData {
    file: PathBuf,
    ifd: Option<usize>,
    first_z: u32,
    first_c: u32,
    first_t: u32,
    plane_count: Option<u32>,
}

fn parse_ome_dataset(
    xml: &str,
    base_dir: &Path,
    default_file: Option<&Path>,
) -> Result<OmeDataset> {
    let document = Document::parse(xml)
        .map_err(|error| BioFormatsError::Format(format!("invalid OME-XML: {error}")))?;
    let root = document
        .descendants()
        .find(|node| node.has_tag_name("OME"))
        .ok_or_else(|| BioFormatsError::Format("OME root element not found".into()))?;

    let instrument = root
        .descendants()
        .find(|node| node.has_tag_name("Objective"));
    let objective_model = instrument
        .and_then(|node| node.attribute("Model"))
        .map(str::to_owned);
    let objective_magnification = instrument
        .and_then(|node| node.attribute("NominalMagnification"))
        .and_then(parse_f64);
    let objective_na = instrument
        .and_then(|node| node.attribute("LensNA"))
        .and_then(parse_f64);

    let mut images = Vec::new();
    let mut used_files = BTreeMap::<String, PathBuf>::new();

    for image_node in root.children().filter(|node| node.has_tag_name("Image")) {
        let acquisition_timestamp = image_node
            .children()
            .find(|node| node.has_tag_name("AcquisitionDate"))
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let pixels = image_node
            .children()
            .find(|node| node.has_tag_name("Pixels"))
            .ok_or_else(|| BioFormatsError::Format("OME Image missing Pixels".into()))?;
        let size_x = positive_u32_attribute(pixels, "SizeX", None)?;
        let size_y = positive_u32_attribute(pixels, "SizeY", None)?;
        let size_z = positive_u32_attribute(pixels, "SizeZ", Some(1))?;
        let size_c_samples = positive_u32_attribute(pixels, "SizeC", Some(1))?;
        let size_t = positive_u32_attribute(pixels, "SizeT", Some(1))?;
        let raw_pixel_type = pixels
            .attribute("Type")
            .ok_or_else(|| BioFormatsError::Format("OME Pixels missing Type".into()))?;
        let pixel_type = pixel_type_from_ome(raw_pixel_type).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "OME pixel type {raw_pixel_type} is not supported"
            ))
        })?;
        let storage_bits = u32::try_from(pixel_type.bytes_per_sample())
            .ok()
            .and_then(|bytes| bytes.checked_mul(8))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let significant_bits =
            positive_u32_attribute(pixels, "SignificantBits", Some(storage_bits))?;
        if significant_bits > storage_bits {
            return Err(BioFormatsError::Format(format!(
                "OME SignificantBits {significant_bits} exceeds {storage_bits}-bit {raw_pixel_type} storage"
            )));
        }
        let significant_bits = u8::try_from(significant_bits).map_err(|_| {
            BioFormatsError::Format("OME SignificantBits does not fit in u8".into())
        })?;
        let raw_dimension_order = pixels
            .attribute("DimensionOrder")
            .ok_or_else(|| BioFormatsError::Format("OME Pixels missing DimensionOrder".into()))?;
        let dimension_order = DimensionOrder::from_str(raw_dimension_order).ok_or_else(|| {
            BioFormatsError::Format(format!(
                "OME DimensionOrder {raw_dimension_order} is invalid"
            ))
        })?;

        let mut channels = Vec::new();
        let mut channel_samples_per_pixel = Vec::new();
        for node in pixels
            .children()
            .filter(|node| node.has_tag_name("Channel"))
        {
            channel_samples_per_pixel.push(positive_u32_attribute(
                node,
                "SamplesPerPixel",
                Some(1),
            )?);
            channels.push(ChannelMetadata {
                name: node.attribute("Name").map(str::to_owned),
                color: node
                    .attribute("Color")
                    .and_then(parse_i32)
                    .map(|value| value as u32),
                emission_wavelength_nm: optional_length_nm(
                    node,
                    "EmissionWavelength",
                    "EmissionWavelengthUnit",
                    "nm",
                )?,
                excitation_wavelength_nm: optional_length_nm(
                    node,
                    "ExcitationWavelength",
                    "ExcitationWavelengthUnit",
                    "nm",
                )?,
            });
        }

        let planes = pixels
            .children()
            .filter(|node| node.has_tag_name("Plane"))
            .map(|plane_node| {
                Ok(PlaneMetadata {
                    z: positive_or_zero_u32_attribute(plane_node, "TheZ", 0)?,
                    c: positive_or_zero_u32_attribute(plane_node, "TheC", 0)?,
                    t: positive_or_zero_u32_attribute(plane_node, "TheT", 0)?,
                    delta_t_seconds: optional_time_seconds(
                        plane_node,
                        "DeltaT",
                        "DeltaTUnit",
                        "s",
                    )?,
                    position_x_um: optional_length_um(
                        plane_node,
                        "PositionX",
                        "PositionXUnit",
                        "reference frame",
                    )?,
                    position_y_um: optional_length_um(
                        plane_node,
                        "PositionY",
                        "PositionYUnit",
                        "reference frame",
                    )?,
                    position_z_um: optional_length_um(
                        plane_node,
                        "PositionZ",
                        "PositionZUnit",
                        "reference frame",
                    )?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let tiff_data = pixels
            .children()
            .filter(|node| node.has_tag_name("TiffData"))
            .map(|node| {
                let file_name = node.attribute("FileName").map(str::to_owned).or_else(|| {
                    node.children()
                        .find(|child| child.has_tag_name("UUID"))
                        .and_then(|child| child.attribute("FileName"))
                        .map(str::to_owned)
                });
                let file = if let Some(file_name) = file_name {
                    base_dir.join(file_name)
                } else if let Some(default_file) = default_file {
                    default_file.to_path_buf()
                } else {
                    return Err(BioFormatsError::Format(
                        "OME-TIFF TiffData missing FileName in companion dataset".into(),
                    ));
                };
                used_files.insert(file.to_string_lossy().to_string(), file.clone());
                Ok(OmeTiffData {
                    file,
                    ifd: optional_u32_attribute(node, "IFD")?
                        .map(|value| {
                            usize::try_from(value).map_err(|_| {
                                BioFormatsError::Format(
                                    "OME TiffData IFD does not fit in usize".into(),
                                )
                            })
                        })
                        .transpose()?,
                    first_z: optional_u32_attribute(node, "FirstZ")?.unwrap_or(0),
                    first_c: optional_u32_attribute(node, "FirstC")?.unwrap_or(0),
                    first_t: optional_u32_attribute(node, "FirstT")?.unwrap_or(0),
                    plane_count: optional_u32_attribute(node, "PlaneCount")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        if tiff_data.is_empty() {
            if let Some(default_file) = default_file {
                used_files.insert(
                    default_file.to_string_lossy().to_string(),
                    default_file.to_path_buf(),
                );
            } else {
                return Err(BioFormatsError::Format(
                    "companion OME metadata does not map any TIFF files".into(),
                ));
            }
        }

        images.push(OmeImage {
            size_x,
            size_y,
            size_z,
            size_c_samples,
            size_t,
            pixel_type,
            significant_bits,
            dimension_order,
            channels,
            channel_samples_per_pixel,
            planes,
            tiff_data,
            physical_size_x_um: optional_length_um(
                pixels,
                "PhysicalSizeX",
                "PhysicalSizeXUnit",
                "µm",
            )?,
            physical_size_y_um: optional_length_um(
                pixels,
                "PhysicalSizeY",
                "PhysicalSizeYUnit",
                "µm",
            )?,
            physical_size_z_um: optional_length_um(
                pixels,
                "PhysicalSizeZ",
                "PhysicalSizeZUnit",
                "µm",
            )?,
            time_increment_seconds: optional_time_seconds(
                pixels,
                "TimeIncrement",
                "TimeIncrementUnit",
                "s",
            )?,
            acquisition_timestamp,
            objective_model: objective_model.clone(),
            objective_magnification,
            objective_na,
        });
    }

    if images.is_empty() {
        return Err(BioFormatsError::Format(
            "OME metadata contains no Image elements".into(),
        ));
    }

    Ok(OmeDataset {
        images,
        used_files: used_files.into_values().collect(),
    })
}

fn pixel_type_from_ome(value: &str) -> Option<PixelType> {
    match value {
        "int8" => Some(PixelType::Int8),
        "uint8" => Some(PixelType::Uint8),
        "int16" => Some(PixelType::Int16),
        "uint16" => Some(PixelType::Uint16),
        "int32" => Some(PixelType::Int32),
        "uint32" => Some(PixelType::Uint32),
        "float" => Some(PixelType::Float32),
        "double" => Some(PixelType::Float64),
        _ => None,
    }
}

fn parse_u32(value: &str) -> Option<u32> {
    value.parse().ok()
}

fn positive_u32_attribute(
    node: roxmltree::Node<'_, '_>,
    name: &str,
    default: Option<u32>,
) -> Result<u32> {
    match node.attribute(name) {
        Some(value) => parse_u32(value).filter(|value| *value > 0).ok_or_else(|| {
            BioFormatsError::Format(format!("OME {name} must be a positive integer"))
        }),
        None => {
            default.ok_or_else(|| BioFormatsError::Format(format!("OME Pixels missing {name}")))
        }
    }
}

fn positive_or_zero_u32_attribute(
    node: roxmltree::Node<'_, '_>,
    name: &str,
    default: u32,
) -> Result<u32> {
    match node.attribute(name) {
        Some(value) => parse_u32(value).ok_or_else(|| {
            BioFormatsError::Format(format!("OME {name} must be a non-negative integer"))
        }),
        None => Ok(default),
    }
}

fn parse_i32(value: &str) -> Option<i32> {
    value.parse().ok()
}

fn parse_f64(value: &str) -> Option<f64> {
    value.parse().ok()
}

fn optional_u32_attribute(node: roxmltree::Node<'_, '_>, name: &str) -> Result<Option<u32>> {
    let Some(raw) = node.attribute(name) else {
        return Ok(None);
    };
    raw.parse::<u32>()
        .map(Some)
        .map_err(|_| BioFormatsError::Format(format!("OME {name} must be a non-negative integer")))
}

fn optional_quantity(
    node: roxmltree::Node<'_, '_>,
    value_name: &str,
    unit_name: &str,
    default_unit: &str,
    target_unit: &str,
    factor: fn(&str) -> Option<f64>,
) -> Result<Option<f64>> {
    let Some(raw_value) = node.attribute(value_name) else {
        return Ok(None);
    };
    let value = raw_value
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| {
            BioFormatsError::Format(format!("OME {value_name} must be a finite number"))
        })?;
    let unit = node.attribute(unit_name).unwrap_or(default_unit);
    let factor = factor(unit).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "OME {value_name} unit {unit:?} cannot be represented as {target_unit}"
        ))
    })?;
    let converted = value * factor;
    if !converted.is_finite() {
        return Err(BioFormatsError::Format(format!(
            "OME {value_name} overflows after conversion to {target_unit}"
        )));
    }
    Ok(Some(converted))
}

fn length_factor_um(unit: &str) -> Option<f64> {
    Some(match unit {
        "Ym" => 1e30,
        "Zm" => 1e27,
        "Em" => 1e24,
        "Pm" => 1e21,
        "Tm" => 1e18,
        "Gm" => 1e15,
        "Mm" => 1e12,
        "km" => 1e9,
        "hm" => 1e8,
        "dam" => 1e7,
        "m" => 1e6,
        "dm" => 1e5,
        "cm" => 1e4,
        "mm" => 1e3,
        "µm" | "μm" | "um" => 1.0,
        "nm" => 1e-3,
        "pm" => 1e-6,
        "fm" => 1e-9,
        "am" => 1e-12,
        "zm" => 1e-15,
        "ym" => 1e-18,
        "Å" => 1e-4,
        "thou" => 25.4,
        "li" => 25_400.0 / 12.0,
        "in" => 25_400.0,
        "ft" => 304_800.0,
        "yd" => 914_400.0,
        "mi" => 1_609_344_000.0,
        "ua" => 1.495_978_707e17,
        "ly" => 9.460_730_472_580_8e21,
        "pc" => 3.085_677_581_491_367e22,
        "pt" => 25_400.0 / 72.0,
        "pixel" | "reference frame" => return None,
        _ => return None,
    })
}

fn time_factor_seconds(unit: &str) -> Option<f64> {
    Some(match unit {
        "Ys" => 1e24,
        "Zs" => 1e21,
        "Es" => 1e18,
        "Ps" => 1e15,
        "Ts" => 1e12,
        "Gs" => 1e9,
        "Ms" => 1e6,
        "ks" => 1e3,
        "hs" => 1e2,
        "das" => 1e1,
        "s" => 1.0,
        "ds" => 1e-1,
        "cs" => 1e-2,
        "ms" => 1e-3,
        "µs" | "μs" | "us" => 1e-6,
        "ns" => 1e-9,
        "ps" => 1e-12,
        "fs" => 1e-15,
        "as" => 1e-18,
        "zs" => 1e-21,
        "ys" => 1e-24,
        "min" => 60.0,
        "h" => 3_600.0,
        "d" => 86_400.0,
        _ => return None,
    })
}

fn optional_length_um(
    node: roxmltree::Node<'_, '_>,
    value_name: &str,
    unit_name: &str,
    default_unit: &str,
) -> Result<Option<f64>> {
    optional_quantity(
        node,
        value_name,
        unit_name,
        default_unit,
        "µm",
        length_factor_um,
    )
}

fn optional_length_nm(
    node: roxmltree::Node<'_, '_>,
    value_name: &str,
    unit_name: &str,
    default_unit: &str,
) -> Result<Option<f64>> {
    optional_length_um(node, value_name, unit_name, default_unit)?
        .map(|value| {
            let converted = value * 1_000.0;
            converted.is_finite().then_some(converted).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "OME {value_name} overflows after conversion to nm"
                ))
            })
        })
        .transpose()
}

fn optional_time_seconds(
    node: roxmltree::Node<'_, '_>,
    value_name: &str,
    unit_name: &str,
    default_unit: &str,
) -> Result<Option<f64>> {
    optional_quantity(
        node,
        value_name,
        unit_name,
        default_unit,
        "s",
        time_factor_seconds,
    )
}

fn plane_index_for_dimension_order(
    order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    z: u32,
    c: u32,
    t: u32,
) -> u32 {
    let metadata = ImageMetadata {
        size_z,
        size_c,
        size_t,
        image_count: size_z.saturating_mul(size_c).saturating_mul(size_t),
        dimension_order: order,
        ..ImageMetadata::default()
    };
    metadata.get_index(z, c, t)
}

fn build_ome_series(
    minimal: &MinimalTiffReader,
    dataset: &OmeDataset,
    used_files: &[PathBuf],
) -> Result<Vec<SeriesData>> {
    let file_indices = minimal
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.clone(), index))
        .collect::<HashMap<_, _>>();

    dataset
        .images
        .iter()
        .map(|image| {
            let (sample_file_index, sample_ifd_index) =
                if let Some(tiff_data) = image.tiff_data.first() {
                    (
                        *file_indices.get(&tiff_data.file).ok_or_else(|| {
                            BioFormatsError::Format(format!(
                                "OME-TIFF referenced file {} was not loaded",
                                tiff_data.file.display()
                            ))
                        })?,
                        tiff_data.ifd.unwrap_or(0),
                    )
                } else {
                    (0, 0)
                };
            let sample_file = minimal.files.get(sample_file_index).ok_or_else(|| {
                BioFormatsError::Format("OME-TIFF does not reference a TIFF file".into())
            })?;
            let sample_ifd = sample_file.ifds.get(sample_ifd_index).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "OME-TIFF references missing IFD {sample_ifd_index}"
                ))
            })?;
            let info = ifd_info(sample_ifd, sample_file.parser.little_endian)?;
            if info.width != image.size_x || info.height != image.size_y {
                return Err(BioFormatsError::Format(format!(
                    "OME Pixels dimensions {}x{} do not match TIFF IFD dimensions {}x{}",
                    image.size_x, image.size_y, info.width, info.height
                )));
            }
            if info.pixel_type != image.pixel_type {
                return Err(BioFormatsError::Format(format!(
                    "OME pixel type {:?} does not match TIFF pixel type {:?}",
                    image.pixel_type, info.pixel_type
                )));
            }
            if image.significant_bits > info.bits_per_sample as u8 {
                return Err(BioFormatsError::Format(format!(
                    "OME SignificantBits {} exceeds TIFF BitsPerSample {}",
                    image.significant_bits, info.bits_per_sample
                )));
            }

            let samples_per_pixel = returned_samples_per_pixel(&info);
            if image.size_c_samples % samples_per_pixel != 0 {
                return Err(BioFormatsError::Format(format!(
                    "OME SizeC {} is not divisible by TIFF SamplesPerPixel {}",
                    image.size_c_samples, samples_per_pixel
                )));
            }
            let logical_channel_count = image.size_c_samples / samples_per_pixel;
            if logical_channel_count == 0 {
                return Err(BioFormatsError::Format(
                    "OME-TIFF has no logical channels".into(),
                ));
            }
            if !image.channel_samples_per_pixel.is_empty() {
                if image.channels.len() != logical_channel_count as usize {
                    return Err(BioFormatsError::Format(format!(
                        "OME declares {} logical Channel elements but SizeC/SamplesPerPixel requires {logical_channel_count}",
                        image.channels.len()
                    )));
                }
                if image
                    .channel_samples_per_pixel
                    .iter()
                    .any(|count| *count != samples_per_pixel)
                {
                    return Err(BioFormatsError::UnsupportedFormat(
                        "OME-TIFF channels with differing SamplesPerPixel are not yet supported"
                            .into(),
                    ));
                }
            }

            let image_count = image
                .size_z
                .checked_mul(logical_channel_count)
                .and_then(|count| count.checked_mul(image.size_t))
                .ok_or_else(|| {
                    BioFormatsError::Format("OME-TIFF plane count exceeds u32".into())
                })?;
            let plane_count = usize::try_from(image_count).map_err(|_| {
                BioFormatsError::Format("OME-TIFF plane count does not fit in memory".into())
            })?;
            let mut planes = Vec::new();
            planes.try_reserve_exact(plane_count).map_err(|_| {
                BioFormatsError::Format("OME-TIFF plane map does not fit in memory".into())
            })?;
            planes.resize(plane_count, None);
            if image.tiff_data.is_empty() {
                let default_file_index = 0usize;
                for (index, plane) in planes.iter_mut().enumerate() {
                    *plane = Some(PlaneRef {
                        file_index: default_file_index,
                        ifd_index: index,
                        sub_resolution: 0,
                    });
                }
            } else {
                for tiff_data in &image.tiff_data {
                    let file_index = *file_indices.get(&tiff_data.file).ok_or_else(|| {
                        BioFormatsError::Format(format!(
                            "OME-TIFF referenced file {} was not loaded",
                            tiff_data.file.display()
                        ))
                    })?;
                    if tiff_data.first_z >= image.size_z
                        || tiff_data.first_c >= logical_channel_count
                        || tiff_data.first_t >= image.size_t
                    {
                        return Err(BioFormatsError::Format(format!(
                            "OME-TIFF TiffData coordinate ({}, {}, {}) is out of range",
                            tiff_data.first_z, tiff_data.first_c, tiff_data.first_t
                        )));
                    }
                    let start = plane_index_for_dimension_order(
                        image.dimension_order,
                        image.size_z,
                        logical_channel_count,
                        image.size_t,
                        tiff_data.first_z,
                        tiff_data.first_c,
                        tiff_data.first_t,
                    );
                    let plane_count = match (tiff_data.ifd, tiff_data.plane_count) {
                        // OME's implicit form `<TiffData/>` maps consecutive
                        // IFDs from the referenced file, rather than only IFD 0.
                        (None, None) => {
                            let remaining = image_count.checked_sub(start).ok_or_else(|| {
                                BioFormatsError::Format(
                                    "OME-TIFF implicit TiffData starts past the image".into(),
                                )
                            })?;
                            u32::try_from(minimal.files[file_index].ifds.len())
                                .unwrap_or(u32::MAX)
                                .min(remaining)
                        }
                        (_, count) => count.unwrap_or(1),
                    };
                    for offset in 0..plane_count {
                        let plane_index = start.checked_add(offset).ok_or_else(|| {
                            BioFormatsError::Format(
                                "OME-TIFF TiffData plane index overflows u32".into(),
                            )
                        })?;
                        let slot = planes.get_mut(plane_index as usize).ok_or_else(|| {
                            BioFormatsError::Format(format!(
                                "OME-TIFF TiffData maps plane {plane_index} out of range"
                            ))
                        })?;
                        let ifd_index = tiff_data
                            .ifd
                            .unwrap_or(0)
                            .checked_add(offset as usize)
                            .ok_or_else(|| {
                                BioFormatsError::Format(
                                    "OME-TIFF TiffData IFD index overflows usize".into(),
                                )
                            })?;
                        if slot
                            .replace(PlaneRef {
                                file_index,
                                ifd_index,
                                sub_resolution: 0,
                            })
                            .is_some()
                        {
                            return Err(BioFormatsError::Format(format!(
                                "OME-TIFF plane {plane_index} is mapped more than once"
                            )));
                        }
                    }
                }
            }
            let root_planes = planes
                .into_iter()
                .enumerate()
                .map(|(index, plane)| {
                    plane.ok_or_else(|| {
                        BioFormatsError::Format(format!("OME-TIFF plane {index} was not mapped"))
                    })
                })
                .collect::<Result<Vec<_>>>()?;

            for (plane_index, plane) in root_planes.iter().enumerate() {
                let file = minimal.files.get(plane.file_index).ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "OME-TIFF plane {plane_index} references a missing file"
                    ))
                })?;
                let ifd = file.ifds.get(plane.ifd_index).ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "OME-TIFF plane {plane_index} references missing IFD {}",
                        plane.ifd_index
                    ))
                })?;
                let plane_info = ifd_info(ifd, file.parser.little_endian)?;
                if !same_plane_layout(&plane_info, &info)
                    || file.parser.little_endian != sample_file.parser.little_endian
                {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "OME-TIFF plane {plane_index} has a different pixel layout"
                    )));
                }
            }

            let coordinate_metadata = ImageMetadata {
                size_z: image.size_z,
                size_c: image.size_c_samples,
                size_t: image.size_t,
                samples_per_pixel,
                image_count,
                dimension_order: image.dimension_order,
                ..ImageMetadata::default()
            };
            let mut plane_metadata = Vec::new();
            plane_metadata.try_reserve_exact(plane_count).map_err(|_| {
                BioFormatsError::Format("OME-TIFF plane metadata does not fit in memory".into())
            })?;
            for index in 0..image_count {
                let (z, c, t) = coordinate_metadata.get_zct_coords(index);
                plane_metadata.push(PlaneMetadata {
                    z,
                    c,
                    t,
                    ..PlaneMetadata::default()
                });
            }
            let mut explicit_planes = Vec::new();
            explicit_planes
                .try_reserve_exact(plane_count)
                .map_err(|_| {
                    BioFormatsError::Format(
                        "OME-TIFF explicit plane map does not fit in memory".into(),
                    )
                })?;
            explicit_planes.resize(plane_count, false);
            for plane in &image.planes {
                if plane.z >= image.size_z
                    || plane.c >= logical_channel_count
                    || plane.t >= image.size_t
                {
                    return Err(BioFormatsError::Format(format!(
                        "OME Plane coordinate ({}, {}, {}) is out of range",
                        plane.z, plane.c, plane.t
                    )));
                }
                let index = coordinate_metadata.get_index(plane.z, plane.c, plane.t) as usize;
                if explicit_planes[index] {
                    return Err(BioFormatsError::Format(format!(
                        "OME Plane metadata for index {index} is duplicated"
                    )));
                }
                explicit_planes[index] = true;
                plane_metadata[index] = plane.clone();
            }

            let is_rgb = matches!(info.photometric, Photometric::Rgb | Photometric::YCbCr)
                && info.samples_per_pixel >= 3;
            let mut metadata = ImageMetadata {
                size_x: image.size_x,
                size_y: image.size_y,
                size_z: image.size_z,
                size_c: image.size_c_samples,
                size_t: image.size_t,
                pixel_type: image.pixel_type,
                bits_per_pixel: image.significant_bits,
                samples_per_pixel,
                image_count,
                dimension_order: image.dimension_order,
                is_rgb,
                is_interleaved: info.planar_config == 1,
                is_indexed: info.photometric == Photometric::Palette,
                is_false_color: true,
                is_little_endian: sample_file.parser.little_endian,
                resolution_count: 1,
                series_metadata: HashMap::new(),
                lookup_table: info.color_map.as_ref().map(|(red, green, blue)| {
                    crate::common::metadata::LookupTable {
                        red: red.clone(),
                        green: green.clone(),
                        blue: blue.clone(),
                    }
                }),
                physical_size_x_um: image.physical_size_x_um,
                physical_size_y_um: image.physical_size_y_um,
                physical_size_z_um: image.physical_size_z_um,
                time_increment_seconds: image.time_increment_seconds,
                acquisition_timestamp: image.acquisition_timestamp.clone(),
                objective_model: image.objective_model.clone(),
                objective_magnification: image.objective_magnification,
                objective_na: image.objective_na,
                channel_metadata: image.channels.clone(),
                plane_metadata,
                used_files: used_files.to_vec(),
            };
            if let Some(description) = &info.image_description {
                metadata.series_metadata.insert(
                    "ImageDescription".into(),
                    MetadataValue::String(description.clone()),
                );
            }
            let resolutions = build_resolution_levels(minimal, root_planes, metadata.clone())?;
            metadata = resolutions[0].metadata.clone();
            Ok(SeriesData {
                metadata,
                resolutions,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiff::ifd::IfdValue;
    use std::fs;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempTiff {
        path: PathBuf,
    }

    impl TempTiff {
        fn write(name: &str, bytes: &[u8]) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bioformats_rs_planar_{name}_{}_{}.tif",
                std::process::id(),
                nonce
            ));
            fs::write(&path, bytes).unwrap();
            Self { path }
        }
    }

    impl Drop for TempTiff {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn push_tag(out: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&field_type.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }

    fn push_tag_be(out: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&field_type.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        out.extend_from_slice(&value.to_be_bytes());
    }

    fn planar_stripped_tiff() -> Vec<u8> {
        const ENTRY_COUNT: u16 = 12;
        const IFD_OFFSET: u32 = 8;
        const IFD_SIZE: u32 = 2 + ENTRY_COUNT as u32 * 12 + 4;
        const OFFSETS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;
        const COUNTS_OFFSET: u32 = OFFSETS_OFFSET + 2 * 4;
        const PIXELS_OFFSET: u32 = COUNTS_OFFSET + 2 * 4;

        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&IFD_OFFSET.to_le_bytes());
        out.extend_from_slice(&ENTRY_COUNT.to_le_bytes());
        push_tag(&mut out, tag::IMAGE_WIDTH, 4, 1, 3);
        push_tag(&mut out, tag::IMAGE_LENGTH, 4, 1, 2);
        push_tag(&mut out, tag::BITS_PER_SAMPLE, 3, 2, 8 | (8 << 16));
        push_tag(&mut out, tag::COMPRESSION, 3, 1, 1);
        push_tag(
            &mut out,
            tag::PHOTOMETRIC_INTERPRETATION,
            3,
            1,
            Photometric::MinIsBlack as u32,
        );
        push_tag(&mut out, tag::STRIP_OFFSETS, 4, 2, OFFSETS_OFFSET);
        push_tag(&mut out, tag::SAMPLES_PER_PIXEL, 3, 1, 2);
        push_tag(&mut out, tag::ROWS_PER_STRIP, 4, 1, 2);
        push_tag(&mut out, tag::STRIP_BYTE_COUNTS, 4, 2, COUNTS_OFFSET);
        push_tag(&mut out, tag::PLANAR_CONFIGURATION, 3, 1, 2);
        push_tag(&mut out, tag::PREDICTOR, 3, 1, 2);
        push_tag(&mut out, tag::EXTRA_SAMPLES, 3, 1, 2);
        out.extend_from_slice(&0u32.to_le_bytes());

        for offset in [PIXELS_OFFSET, PIXELS_OFFSET + 6] {
            out.extend_from_slice(&offset.to_le_bytes());
        }
        for _ in 0..2 {
            out.extend_from_slice(&6u32.to_le_bytes());
        }

        // Horizontal predictor values, ordered by component; each strip has two rows.
        out.extend_from_slice(&[1, 1, 1, 4, 1, 1]);
        out.extend_from_slice(&[10, 10, 10, 40, 10, 10]);
        out
    }

    fn planar_tiled_tiff() -> Vec<u8> {
        const ENTRY_COUNT: u16 = 13;
        const IFD_OFFSET: u32 = 8;
        const IFD_SIZE: u32 = 2 + ENTRY_COUNT as u32 * 12 + 4;
        const OFFSETS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;
        const COUNTS_OFFSET: u32 = OFFSETS_OFFSET + 8 * 4;
        const PIXELS_OFFSET: u32 = COUNTS_OFFSET + 8 * 4;

        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&IFD_OFFSET.to_le_bytes());
        out.extend_from_slice(&ENTRY_COUNT.to_le_bytes());
        push_tag(&mut out, tag::IMAGE_WIDTH, 4, 1, 3);
        push_tag(&mut out, tag::IMAGE_LENGTH, 4, 1, 2);
        push_tag(&mut out, tag::BITS_PER_SAMPLE, 3, 2, 8 | (8 << 16));
        push_tag(&mut out, tag::COMPRESSION, 3, 1, 1);
        push_tag(
            &mut out,
            tag::PHOTOMETRIC_INTERPRETATION,
            3,
            1,
            Photometric::MinIsBlack as u32,
        );
        push_tag(&mut out, tag::SAMPLES_PER_PIXEL, 3, 1, 2);
        push_tag(&mut out, tag::PLANAR_CONFIGURATION, 3, 1, 2);
        push_tag(&mut out, tag::PREDICTOR, 3, 1, 2);
        push_tag(&mut out, tag::TILE_WIDTH, 4, 1, 2);
        push_tag(&mut out, tag::TILE_LENGTH, 4, 1, 1);
        push_tag(&mut out, tag::TILE_OFFSETS, 4, 8, OFFSETS_OFFSET);
        push_tag(&mut out, tag::TILE_BYTE_COUNTS, 4, 8, COUNTS_OFFSET);
        push_tag(&mut out, tag::EXTRA_SAMPLES, 3, 1, 2);
        out.extend_from_slice(&0u32.to_le_bytes());

        for index in 0..8 {
            out.extend_from_slice(&(PIXELS_OFFSET + index * 2).to_le_bytes());
        }
        for _ in 0..8 {
            out.extend_from_slice(&2u32.to_le_bytes());
        }

        // Horizontal predictor values, ordered by component then tile.
        out.extend_from_slice(&[1, 1, 3, 253, 4, 1, 6, 250]);
        out.extend_from_slice(&[10, 10, 30, 226, 40, 10, 60, 196]);
        out
    }

    fn padded_final_legacy_deflate_strip_tiff() -> Vec<u8> {
        const ENTRY_COUNT: u16 = 10;
        const IFD_OFFSET: u32 = 8;
        const IFD_SIZE: u32 = 2 + ENTRY_COUNT as u32 * 12 + 4;
        const OFFSETS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;
        const COUNTS_OFFSET: u32 = OFFSETS_OFFSET + 2 * 4;
        const PIXELS_OFFSET: u32 = COUNTS_OFFSET + 2 * 4;

        let encode = |pixels: &[u8]| {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(pixels).unwrap();
            encoder.finish().unwrap()
        };
        let first = encode(&[1, 2, 3, 4, 5, 6]);
        let last = encode(&[7, 8, 9, 200, 201, 202]);

        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42_u16.to_le_bytes());
        out.extend_from_slice(&IFD_OFFSET.to_le_bytes());
        out.extend_from_slice(&ENTRY_COUNT.to_le_bytes());
        push_tag(&mut out, tag::IMAGE_WIDTH, 4, 1, 3);
        push_tag(&mut out, tag::IMAGE_LENGTH, 4, 1, 3);
        push_tag(&mut out, tag::BITS_PER_SAMPLE, 3, 1, 8);
        push_tag(&mut out, tag::COMPRESSION, 3, 1, 32946);
        push_tag(
            &mut out,
            tag::PHOTOMETRIC_INTERPRETATION,
            3,
            1,
            Photometric::MinIsBlack as u32,
        );
        push_tag(&mut out, tag::STRIP_OFFSETS, 4, 2, OFFSETS_OFFSET);
        push_tag(&mut out, tag::SAMPLES_PER_PIXEL, 3, 1, 1);
        push_tag(&mut out, tag::ROWS_PER_STRIP, 4, 1, 2);
        push_tag(&mut out, tag::STRIP_BYTE_COUNTS, 4, 2, COUNTS_OFFSET);
        push_tag(&mut out, tag::PLANAR_CONFIGURATION, 3, 1, 1);
        out.extend_from_slice(&0_u32.to_le_bytes());

        out.extend_from_slice(&PIXELS_OFFSET.to_le_bytes());
        out.extend_from_slice(&(PIXELS_OFFSET + first.len() as u32).to_le_bytes());
        out.extend_from_slice(&(first.len() as u32).to_le_bytes());
        out.extend_from_slice(&(last.len() as u32).to_le_bytes());
        out.extend_from_slice(&first);
        out.extend_from_slice(&last);
        out
    }

    fn big_endian_sixteen_bit_tiff() -> Vec<u8> {
        const ENTRY_COUNT: u16 = 10;
        const IFD_OFFSET: u32 = 8;
        const IFD_SIZE: u32 = 2 + ENTRY_COUNT as u32 * 12 + 4;
        const PIXELS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;

        let mut out = Vec::new();
        out.extend_from_slice(b"MM");
        out.extend_from_slice(&42u16.to_be_bytes());
        out.extend_from_slice(&IFD_OFFSET.to_be_bytes());
        out.extend_from_slice(&ENTRY_COUNT.to_be_bytes());
        push_tag_be(&mut out, tag::IMAGE_WIDTH, 4, 1, 3);
        push_tag_be(&mut out, tag::IMAGE_LENGTH, 4, 1, 2);
        push_tag_be(&mut out, tag::BITS_PER_SAMPLE, 3, 1, 16 << 16);
        push_tag_be(&mut out, tag::COMPRESSION, 3, 1, 1 << 16);
        push_tag_be(
            &mut out,
            tag::PHOTOMETRIC_INTERPRETATION,
            3,
            1,
            (Photometric::MinIsBlack as u32) << 16,
        );
        push_tag_be(&mut out, tag::STRIP_OFFSETS, 4, 1, PIXELS_OFFSET);
        push_tag_be(&mut out, tag::SAMPLES_PER_PIXEL, 3, 1, 1 << 16);
        push_tag_be(&mut out, tag::ROWS_PER_STRIP, 4, 1, 2);
        push_tag_be(&mut out, tag::STRIP_BYTE_COUNTS, 4, 1, 12);
        push_tag_be(&mut out, tag::PREDICTOR, 3, 1, 2 << 16);
        out.extend_from_slice(&0u32.to_be_bytes());

        for value in [0x0100_u16, 0x0002, 0x00fe, 0x1000, 0x0001, 0x0002] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }

    fn grayscale_alpha_info(planar_config: u16) -> IfdInfo {
        let mut ifd = Ifd::default();
        ifd.entries
            .insert(tag::IMAGE_WIDTH, IfdValue::Long(vec![2]));
        ifd.entries
            .insert(tag::IMAGE_LENGTH, IfdValue::Long(vec![1]));
        ifd.entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(vec![8, 8]));
        ifd.entries.insert(
            tag::PHOTOMETRIC_INTERPRETATION,
            IfdValue::Short(vec![Photometric::MinIsBlack as u16]),
        );
        ifd.entries
            .insert(tag::SAMPLES_PER_PIXEL, IfdValue::Short(vec![2]));
        ifd.entries.insert(
            tag::PLANAR_CONFIGURATION,
            IfdValue::Short(vec![planar_config]),
        );
        ifd.entries
            .insert(tag::EXTRA_SAMPLES, IfdValue::Short(vec![2]));
        ifd_info(&ifd, true).unwrap()
    }

    fn baseline_ifd() -> Ifd {
        let mut ifd = Ifd::default();
        ifd.entries
            .insert(tag::IMAGE_WIDTH, IfdValue::Long(vec![2]));
        ifd.entries
            .insert(tag::IMAGE_LENGTH, IfdValue::Long(vec![1]));
        ifd.entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(vec![8]));
        ifd.entries.insert(
            tag::PHOTOMETRIC_INTERPRETATION,
            IfdValue::Short(vec![Photometric::MinIsBlack as u16]),
        );
        ifd
    }

    #[test]
    fn rejects_tiff_layouts_that_need_unported_sample_transforms() {
        let mut packed = baseline_ifd();
        packed
            .entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(vec![1]));
        assert!(matches!(
            ifd_info(&packed, true),
            Err(BioFormatsError::UnsupportedFormat(_))
        ));

        let mut floating_predictor = baseline_ifd();
        floating_predictor
            .entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(vec![32]));
        floating_predictor
            .entries
            .insert(tag::SAMPLE_FORMAT, IfdValue::Short(vec![3]));
        floating_predictor
            .entries
            .insert(tag::PREDICTOR, IfdValue::Short(vec![3]));
        assert!(matches!(
            ifd_info(&floating_predictor, true),
            Err(BioFormatsError::UnsupportedFormat(_))
        ));

        let mut white_is_zero = baseline_ifd();
        white_is_zero.entries.insert(
            tag::PHOTOMETRIC_INTERPRETATION,
            IfdValue::Short(vec![Photometric::MinIsWhite as u16]),
        );
        assert!(matches!(
            ifd_info(&white_is_zero, true),
            Err(BioFormatsError::UnsupportedFormat(_))
        ));

        let mut raw_ycbcr = baseline_ifd();
        raw_ycbcr.entries.insert(
            tag::PHOTOMETRIC_INTERPRETATION,
            IfdValue::Short(vec![Photometric::YCbCr as u16]),
        );
        assert!(matches!(
            ifd_info(&raw_ycbcr, true),
            Err(BioFormatsError::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn series_layout_key_distinguishes_sample_format_planar_and_photometric() {
        let unsigned = ifd_info(&baseline_ifd(), true).unwrap();

        let mut signed_ifd = baseline_ifd();
        signed_ifd
            .entries
            .insert(tag::SAMPLE_FORMAT, IfdValue::Short(vec![2]));
        let signed = ifd_info(&signed_ifd, true).unwrap();
        assert!(!same_plane_layout(&unsigned, &signed));

        let chunky = grayscale_alpha_info(1);
        let planar = grayscale_alpha_info(2);
        assert!(!same_plane_layout(&chunky, &planar));

        let mut palette = unsigned.clone();
        palette.photometric = Photometric::Palette;
        assert!(!same_plane_layout(&unsigned, &palette));
    }

    #[test]
    fn metadata_reports_chunky_grayscale_alpha_samples() {
        let info = grayscale_alpha_info(1);
        let metadata = generic_metadata_from_info(&info, 1, &[]);

        assert_eq!(metadata.samples_per_pixel, 2);
        assert_eq!(metadata.size_c, 1);
        assert!(!metadata.is_rgb);
    }

    #[test]
    fn metadata_reports_all_samples_for_planar_planes() {
        let info = grayscale_alpha_info(2);
        let metadata = generic_metadata_from_info(&info, 1, &[]);

        assert_eq!(metadata.samples_per_pixel, 2);
        assert!(!metadata.is_interleaved);
    }

    #[test]
    fn standard_tiff_resolution_uses_declared_units() {
        for (unit, expected_x_um) in [(1_u16, 0.01), (2, 254.0), (3, 100.0)] {
            let mut ifd = baseline_ifd();
            ifd.entries
                .insert(tag::X_RESOLUTION, IfdValue::Rational(vec![(100, 1)]));
            ifd.entries
                .insert(tag::RESOLUTION_UNIT, IfdValue::Short(vec![unit]));
            let info = ifd_info(&ifd, true).unwrap();
            let mut metadata = ImageMetadata::default();
            apply_standard_tiff_metadata(&mut metadata, &ifd, &info, true).unwrap();
            assert_eq!(metadata.physical_size_x_um, Some(expected_x_um));
            assert!(matches!(
                metadata.series_metadata.get("ResolutionUnit"),
                Some(MetadataValue::Int(value)) if *value == i64::from(unit)
            ));
        }
    }

    #[test]
    fn reads_all_planar_samples_from_strips_and_tiles() {
        let fixtures = [
            TempTiff::write("strips", &planar_stripped_tiff()),
            TempTiff::write("tiles", &planar_tiled_tiff()),
        ];
        let expected_full = [1, 2, 3, 4, 5, 6, 10, 20, 30, 40, 50, 60];
        let expected_region = [2, 3, 5, 6, 20, 30, 50, 60];

        for fixture in &fixtures {
            let mut reader = TiffReader::new();
            reader.set_id(&fixture.path).unwrap();

            assert_eq!(reader.metadata().samples_per_pixel, 2);
            assert!(!reader.metadata().is_interleaved);
            assert_eq!(reader.open_bytes(0).unwrap(), expected_full);
            assert_eq!(
                reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
                expected_region
            );
        }
    }

    #[test]
    fn reads_big_endian_sixteen_bit_predictor_by_scanline() {
        let fixture = TempTiff::write("predictor_be16", &big_endian_sixteen_bit_tiff());
        let mut reader = TiffReader::new();
        reader.set_id(&fixture.path).unwrap();

        let expected = [0x0100_u16, 0x0102, 0x0200, 0x1000, 0x1001, 0x1003]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let expected_region = [0x0102_u16, 0x0200, 0x1001, 0x1003]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();

        assert!(!reader.metadata().is_little_endian);
        assert_eq!(reader.open_bytes(0).unwrap(), expected);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            expected_region
        );
    }

    #[test]
    fn accepts_padded_final_strip_with_legacy_zlib_code() {
        let fixture = TempTiff::write(
            "padded_final_legacy_deflate",
            &padded_final_legacy_deflate_strip_tiff(),
        );
        let mut reader = TiffReader::new();
        reader.set_id(&fixture.path).unwrap();

        assert_eq!(reader.open_bytes(0).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
            [5, 6, 8, 9]
        );
    }

    #[test]
    fn rejects_plane_byte_count_overflow_before_allocating() {
        let fixture = TempTiff::write("overflow", &planar_stripped_tiff());
        let mut state = TiffFileState::open_path(&fixture.path).unwrap();
        let ifd = &mut state.ifds[0];
        ifd.entries
            .insert(tag::IMAGE_WIDTH, IfdValue::Long(vec![u32::MAX]));
        ifd.entries
            .insert(tag::IMAGE_LENGTH, IfdValue::Long(vec![u32::MAX]));
        ifd.entries
            .insert(tag::SAMPLES_PER_PIXEL, IfdValue::Short(vec![u16::MAX]));
        ifd.entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(vec![64]));
        ifd.entries
            .insert(tag::SAMPLE_FORMAT, IfdValue::Short(vec![3]));
        ifd.entries.insert(tag::PREDICTOR, IfdValue::Short(vec![1]));

        let error = state
            .read_plane(
                &PlaneRef {
                    file_index: 0,
                    ifd_index: 0,
                    sub_resolution: 0,
                },
                0,
                0,
                u32::MAX,
                u32::MAX,
            )
            .unwrap_err();

        assert!(matches!(error, BioFormatsError::PlaneByteCountOverflow));
    }

    #[test]
    fn rejects_structural_tiff_values_that_do_not_fit_target_types() {
        let fixture = TempTiff::write("structural_overflow", &planar_stripped_tiff());
        let mut state = TiffFileState::open_path(&fixture.path).unwrap();
        let ifd = &mut state.ifds[0];

        ifd.entries.insert(
            tag::IMAGE_WIDTH,
            IfdValue::Long8(vec![u64::from(u32::MAX) + 1]),
        );
        assert!(matches!(
            ifd_info(ifd, true),
            Err(BioFormatsError::InvalidData(message)) if message.contains("ImageWidth")
        ));

        ifd.entries
            .insert(tag::IMAGE_WIDTH, IfdValue::Long(vec![3]));
        ifd.entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(Vec::new()));
        assert!(matches!(
            ifd_info(ifd, true),
            Err(BioFormatsError::InvalidData(message)) if message.contains("present but empty")
        ));

        ifd.entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Long(vec![65_544]));
        assert!(matches!(
            ifd_info(ifd, true),
            Err(BioFormatsError::InvalidData(message)) if message.contains("BitsPerSample")
        ));

        ifd.entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(vec![8, 8]));
        ifd.entries
            .insert(tag::COMPRESSION, IfdValue::Long(vec![65_537]));
        assert!(matches!(
            ifd_info(ifd, true),
            Err(BioFormatsError::InvalidData(message)) if message.contains("Compression")
        ));

        ifd.entries
            .insert(tag::COMPRESSION, IfdValue::Short(vec![1]));
        ifd.entries
            .insert(tag::SAMPLES_PER_PIXEL, IfdValue::Short(vec![1]));
        ifd.entries
            .insert(tag::BITS_PER_SAMPLE, IfdValue::Short(vec![8]));
        ifd.entries.insert(
            tag::PHOTOMETRIC_INTERPRETATION,
            IfdValue::Short(vec![Photometric::Palette as u16]),
        );
        ifd.entries
            .insert(tag::COLOR_MAP, IfdValue::Short(vec![0; 6]));
        assert!(matches!(
            ifd_info(ifd, true),
            Err(BioFormatsError::InvalidData(message)) if message.contains("expected 768")
        ));
    }

    #[test]
    fn rejects_short_or_oversized_uncompressed_tile_storage() {
        let fixture = TempTiff::write("raw_tile_bounds", &planar_tiled_tiff());

        let mut short = TiffFileState::open_path(&fixture.path).unwrap();
        short.ifds[0].entries.insert(
            tag::TILE_BYTE_COUNTS,
            IfdValue::Long(vec![1, 2, 2, 2, 2, 2, 2, 2]),
        );
        assert!(matches!(
            short.read_plane(
                &PlaneRef {
                    file_index: 0,
                    ifd_index: 0,
                    sub_resolution: 0,
                },
                0,
                0,
                3,
                2,
            ),
            Err(BioFormatsError::InvalidData(message)) if message.contains("at least 2 bytes")
        ));

        let mut oversized = TiffFileState::open_path(&fixture.path).unwrap();
        oversized.ifds[0].entries.insert(
            tag::TILE_BYTE_COUNTS,
            IfdValue::Long(vec![u32::MAX, 2, 2, 2, 2, 2, 2, 2]),
        );
        assert!(matches!(
            oversized.read_plane(
                &PlaneRef {
                    file_index: 0,
                    ifd_index: 0,
                    sub_resolution: 0,
                },
                0,
                0,
                3,
                2,
            ),
            Err(BioFormatsError::InvalidData(message)) if message.contains("maximum is 2")
        ));
    }
}
