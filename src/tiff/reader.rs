use std::collections::{BTreeMap, HashMap};
use std::fs::File;
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
use crate::common::reader::FormatReader;
use crate::snapshot::ReaderSnapshot;

use super::compression::decompress;
use super::ifd::{tag, Compression, Ifd, Photometric};
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

#[derive(Debug)]
struct TiffFileState {
    path: PathBuf,
    parser: TiffParser<BufReader<File>>,
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
}

#[derive(Debug)]
struct OmeTiffReader {
    minimal: MinimalTiffReader,
    series: Vec<SeriesData>,
    used_files: Vec<PathBuf>,
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
        let minimal = MinimalTiffReader::from_snapshot(snapshot.minimal)?;
        let backend = match snapshot.kind {
            TiffBackendKind::Generic => TiffBackend::Generic(BaseTiffReader {
                minimal,
                series: snapshot.series,
                used_files: snapshot.used_files,
            }),
            TiffBackendKind::Ome => TiffBackend::Ome(OmeTiffReader {
                minimal,
                series: snapshot.series,
                used_files: snapshot.used_files,
                metadata_file: snapshot.metadata_file,
            }),
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

    fn generic_from_tiff(path: &Path) -> Result<BaseTiffReader> {
        let minimal = MinimalTiffReader::from_paths(&[path.to_path_buf()])?;
        let mut reader = BaseTiffReader {
            used_files: vec![path.to_path_buf()],
            series: Vec::new(),
            minimal,
        };
        reader.series = build_generic_series(&reader.minimal, &reader.used_files)?;
        Ok(reader)
    }

    fn ome_from_path(path: &Path) -> Result<OmeTiffReader> {
        let lower = path.to_string_lossy().to_ascii_lowercase();
        let (xml, metadata_file, default_file) = if lower.ends_with(".companion.ome")
            || lower.ends_with(".ome")
        {
            (
                std::fs::read_to_string(path)?,
                Some(path.to_path_buf()),
                None,
            )
        } else {
            let minimal = MinimalTiffReader::from_paths(&[path.to_path_buf()])?;
            let xml = minimal
                .files
                .first()
                .and_then(first_ome_xml)
                .ok_or_else(|| BioFormatsError::Format("TIFF does not contain OME-XML".into()))?;
            if let Some(companion) = binary_only_metadata_file(&xml)? {
                let companion_path = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(companion);
                (
                    std::fs::read_to_string(&companion_path)?,
                    Some(companion_path),
                    Some(path.to_path_buf()),
                )
            } else {
                (xml, None, Some(path.to_path_buf()))
            }
        };

        let metadata_base = metadata_file
            .as_ref()
            .map(|value| value.parent().unwrap_or_else(|| Path::new(".")))
            .or_else(|| default_file.as_ref().and_then(|value| value.parent()))
            .unwrap_or_else(|| Path::new("."));
        let parsed = parse_ome_dataset(&xml, metadata_base, default_file.as_deref())?;
        let minimal = MinimalTiffReader::from_paths(&parsed.used_files)?;
        let used_files = if let Some(metadata_file) = metadata_file.as_ref() {
            let mut files = vec![metadata_file.clone()];
            files.extend(parsed.used_files.iter().cloned());
            files
        } else {
            parsed.used_files.clone()
        };

        Ok(OmeTiffReader {
            series: build_ome_series(&minimal, &parsed, &used_files)?,
            minimal,
            used_files,
            metadata_file,
        })
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
            || lower.ends_with(".tf8")
            || lower.ends_with(".btf")
            || lower.ends_with(".companion.ome")
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
        let lower = path.to_string_lossy().to_ascii_lowercase();
        if lower.ends_with(".companion.ome") || lower.ends_with(".ome") {
            self.backend = Some(TiffBackend::Ome(Self::ome_from_path(path)?));
            return Ok(());
        }

        match Self::ome_from_path(path) {
            Ok(reader) => {
                self.backend = Some(TiffBackend::Ome(reader));
                Ok(())
            }
            Err(_) => {
                self.backend = Some(TiffBackend::Generic(Self::generic_from_tiff(path)?));
                Ok(())
            }
        }
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
                minimal: reader.minimal.snapshot(),
                series: reader.series.clone(),
                used_files: reader.used_files.clone(),
                metadata_file: None,
            },
            TiffBackend::Ome(reader) => TiffReaderSnapshot {
                kind: TiffBackendKind::Ome,
                minimal: reader.minimal.snapshot(),
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

    fn snapshot(&self) -> MinimalTiffReaderSnapshot {
        MinimalTiffReaderSnapshot {
            files: self.files.iter().map(TiffFileState::snapshot).collect(),
            current_root_series: self.current_root_series,
            current_resolution: self.current_resolution,
            flattened_resolutions: self.flattened_resolutions,
        }
    }

    fn from_paths(paths: &[PathBuf]) -> Result<Self> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            files.push(TiffFileState::open(path)?);
        }
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
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut parser = TiffParser::new(reader)?;
        let ifds = parser.read_ifds()?;
        let sub_ifds = ifds
            .iter()
            .map(|ifd| {
                ifd.get_vec_u64(tag::SUB_IFD)
                    .into_iter()
                    .filter_map(|offset| parser.read_ifd(offset).ok().map(|value| value.0))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        Ok(Self {
            path: path.to_path_buf(),
            parser,
            ifds,
            sub_ifds,
        })
    }

    fn snapshot(&self) -> TiffFileSnapshot {
        TiffFileSnapshot {
            path: self.path.clone(),
            little_endian: self.parser.little_endian,
            ifds: self.ifds.clone(),
            sub_ifds: self.sub_ifds.clone(),
        }
    }

    fn from_snapshot(snapshot: TiffFileSnapshot) -> Result<Self> {
        let file = File::open(&snapshot.path)?;
        let reader = BufReader::new(file);
        let mut parser = TiffParser::new(reader)?;
        parser.little_endian = snapshot.little_endian;
        Ok(Self {
            path: snapshot.path,
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
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = if info.planar_config == 2 {
            1
        } else {
            info.samples_per_pixel as u32
        };
        let plane_byte_len = (w * h * effective_spp * bytes_per_sample) as usize;
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
        _plane_byte_len: usize,
    ) -> Result<Vec<u8>> {
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = if info.planar_config == 2 {
            1u32
        } else {
            info.samples_per_pixel as u32
        };
        let row_bytes = info.width * effective_spp * bytes_per_sample;
        let rows_per_strip = if info.rows_per_strip == 0 || info.rows_per_strip >= info.height {
            info.height
        } else {
            info.rows_per_strip
        };

        let mut plane_rows = Vec::with_capacity((h * row_bytes) as usize);
        for strip_index in 0..info.strip_offsets.len() {
            let strip_start_row = strip_index as u32 * rows_per_strip;
            let strip_end_row = (strip_start_row + rows_per_strip).min(info.height);
            if strip_end_row <= y || strip_start_row >= y + h {
                continue;
            }

            let offset = info.strip_offsets[strip_index];
            let byte_count = info.strip_byte_counts[strip_index] as usize;
            let compressed = read_bytes_at(&mut self.parser.reader, offset, byte_count)?;
            let strip_rows = strip_end_row - strip_start_row;
            let expected = (strip_rows * row_bytes) as usize;
            let mut strip_data = decompress(
                &compressed,
                info.compression,
                expected,
                info.predictor,
                info.samples_per_pixel,
                info.bits_per_sample,
                info.jpeg_tables.as_deref(),
            )?;
            strip_data.truncate(expected);
            let row_start = y.saturating_sub(strip_start_row) as usize;
            let row_end = (y + h - strip_start_row).min(strip_rows) as usize;
            for row in row_start..row_end {
                let start = row * row_bytes as usize;
                let end = start + row_bytes as usize;
                if end <= strip_data.len() {
                    plane_rows.extend_from_slice(&strip_data[start..end]);
                }
            }
        }

        if x == 0 && w == info.width {
            return Ok(plane_rows);
        }

        let x_start = (x * effective_spp * bytes_per_sample) as usize;
        let x_len = (w * effective_spp * bytes_per_sample) as usize;
        let full_row = row_bytes as usize;
        let mut out = Vec::with_capacity(h as usize * x_len);
        for row in 0..h as usize {
            let src = &plane_rows[row * full_row..];
            out.extend_from_slice(&src[x_start..x_start + x_len]);
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
        _plane_byte_len: usize,
    ) -> Result<Vec<u8>> {
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = if info.planar_config == 2 {
            1u32
        } else {
            info.samples_per_pixel as u32
        };
        let tile_row_bytes = (info.tile_width * effective_spp * bytes_per_sample) as usize;
        let tile_data_bytes = tile_row_bytes * info.tile_height as usize;
        let tiles_across = (info.width + info.tile_width - 1) / info.tile_width;
        let tx_start = x / info.tile_width;
        let tx_end = (x + w + info.tile_width - 1) / info.tile_width;
        let ty_start = y / info.tile_height;
        let ty_end = (y + h + info.tile_height - 1) / info.tile_height;
        let out_row_bytes = (w * effective_spp * bytes_per_sample) as usize;
        let mut out = vec![0u8; h as usize * out_row_bytes];

        for ty in ty_start..ty_end {
            for tx in tx_start..tx_end {
                let tile_index = (ty * tiles_across + tx) as usize;
                if tile_index >= info.tile_offsets.len() {
                    continue;
                }
                let offset = info.tile_offsets[tile_index];
                let byte_count = info.tile_byte_counts[tile_index] as usize;
                let compressed = read_bytes_at(&mut self.parser.reader, offset, byte_count)?;
                let mut tile_data = decompress(
                    &compressed,
                    info.compression,
                    tile_data_bytes,
                    info.predictor,
                    info.samples_per_pixel,
                    info.bits_per_sample,
                    info.jpeg_tables.as_deref(),
                )?;
                tile_data.resize(tile_data_bytes, 0);

                let tile_x0 = tx * info.tile_width;
                let tile_y0 = ty * info.tile_height;
                let src_x = x.saturating_sub(tile_x0) as usize;
                let src_y = y.saturating_sub(tile_y0) as usize;
                let dst_x = tile_x0.saturating_sub(x) as usize;
                let dst_y = tile_y0.saturating_sub(y) as usize;
                let copy_w = ((info.tile_width - src_x as u32).min(w - dst_x as u32)) as usize;
                let copy_h = ((info.tile_height - src_y as u32).min(h - dst_y as u32)) as usize;
                let copy_bytes = copy_w * effective_spp as usize * bytes_per_sample as usize;
                for row in 0..copy_h {
                    let src_off = ((src_y + row) * tile_row_bytes)
                        + src_x * effective_spp as usize * bytes_per_sample as usize;
                    let dst_off = ((dst_y + row) * out_row_bytes)
                        + dst_x * effective_spp as usize * bytes_per_sample as usize;
                    if src_off + copy_bytes <= tile_data.len() && dst_off + copy_bytes <= out.len()
                    {
                        out[dst_off..dst_off + copy_bytes]
                            .copy_from_slice(&tile_data[src_off..src_off + copy_bytes]);
                    }
                }
            }
        }

        Ok(out)
    }
}

fn ifd_info(ifd: &Ifd, _little_endian: bool) -> Result<IfdInfo> {
    let width = ifd
        .image_width()
        .ok_or_else(|| BioFormatsError::Format("IFD missing ImageWidth".into()))?;
    let height = ifd
        .image_length()
        .ok_or_else(|| BioFormatsError::Format("IFD missing ImageLength".into()))?;
    let samples_per_pixel = ifd.samples_per_pixel();
    let bps_vec = ifd.bits_per_sample();
    let bits_per_sample = bps_vec.first().copied().unwrap_or(8);
    let sample_format = ifd.get_u16(tag::SAMPLE_FORMAT).unwrap_or(1);
    let pixel_type = pixel_type_from_bps_format(bits_per_sample, sample_format);
    let photometric = ifd.photometric();
    let compression = ifd.compression();
    let planar_config = ifd.planar_configuration();
    let predictor = ifd.predictor();
    let is_tiled = ifd.is_tiled();
    let (tile_width, tile_height) = if is_tiled {
        (
            ifd.tile_width().unwrap_or(0),
            ifd.tile_length().unwrap_or(0),
        )
    } else {
        (0, 0)
    };
    let rows_per_strip = if is_tiled {
        0
    } else {
        ifd.get_u32(tag::ROWS_PER_STRIP).unwrap_or(height)
    };
    let strip_offsets = ifd.get_vec_u64(tag::STRIP_OFFSETS);
    let strip_byte_counts = ifd.get_vec_u64(tag::STRIP_BYTE_COUNTS);
    let tile_offsets = ifd.get_vec_u64(tag::TILE_OFFSETS);
    let tile_byte_counts = ifd.get_vec_u64(tag::TILE_BYTE_COUNTS);
    let color_map = if photometric == Photometric::Palette {
        if let Some(value) = ifd.get(tag::COLOR_MAP) {
            let data = value.as_vec_u16();
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

fn pixel_type_from_bps_format(bps: u16, sample_format: u16) -> PixelType {
    match (bps, sample_format) {
        (1, _) => PixelType::Bit,
        (8, 2) => PixelType::Int8,
        (8, _) => PixelType::Uint8,
        (16, 2) => PixelType::Int16,
        (16, _) => PixelType::Uint16,
        (32, 2) => PixelType::Int32,
        (32, 3) => PixelType::Float32,
        (32, _) => PixelType::Uint32,
        (64, 3) => PixelType::Float64,
        _ => PixelType::Uint8,
    }
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
        .filter_map(|(index, ifd)| ifd_info(ifd, little_endian).ok().map(|info| (index, info)))
        .collect::<Vec<_>>();
    if infos.is_empty() {
        return Ok(Vec::new());
    }

    let mut groups: Vec<Vec<(usize, IfdInfo)>> = Vec::new();
    for (index, info) in infos {
        if let Some(last) = groups.last_mut() {
            let previous = &last.last().unwrap().1;
            if previous.width == info.width
                && previous.height == info.height
                && previous.samples_per_pixel == info.samples_per_pixel
                && previous.bits_per_sample == info.bits_per_sample
            {
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
            );
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
) {
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
    if let Some(resolution) = rational_to_f64(ifd.get(tag::X_RESOLUTION)) {
        metadata
            .series_metadata
            .insert("XResolution".into(), MetadataValue::Float(resolution));
        if resolution > 0.0 {
            metadata.physical_size_x_um = Some(10_000.0 / resolution);
        }
    }
    if let Some(resolution) = rational_to_f64(ifd.get(tag::Y_RESOLUTION)) {
        metadata
            .series_metadata
            .insert("YResolution".into(), MetadataValue::Float(resolution));
        if resolution > 0.0 {
            metadata.physical_size_y_um = Some(10_000.0 / resolution);
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
        let mut metadata = root_metadata.clone();
        metadata.size_x = info.width;
        metadata.size_y = info.height;
        metadata.bits_per_pixel = info.bits_per_sample as u8;
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
    size_c_logical: u32,
    size_t: u32,
    pixel_type: PixelType,
    dimension_order: DimensionOrder,
    channels: Vec<ChannelMetadata>,
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
    ifd: usize,
    first_z: u32,
    first_c: u32,
    first_t: u32,
    plane_count: u32,
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
        let size_x = pixels
            .attribute("SizeX")
            .and_then(parse_u32)
            .ok_or_else(|| BioFormatsError::Format("OME Pixels missing SizeX".into()))?;
        let size_y = pixels
            .attribute("SizeY")
            .and_then(parse_u32)
            .ok_or_else(|| BioFormatsError::Format("OME Pixels missing SizeY".into()))?;
        let size_z = pixels.attribute("SizeZ").and_then(parse_u32).unwrap_or(1);
        let size_c_logical = pixels.attribute("SizeC").and_then(parse_u32).unwrap_or(1);
        let size_t = pixels.attribute("SizeT").and_then(parse_u32).unwrap_or(1);
        let pixel_type = pixels
            .attribute("Type")
            .and_then(pixel_type_from_ome)
            .unwrap_or(PixelType::Uint8);
        let dimension_order = pixels
            .attribute("DimensionOrder")
            .and_then(DimensionOrder::from_str)
            .unwrap_or(DimensionOrder::XYCZT);

        let channels = pixels
            .children()
            .filter(|node| node.has_tag_name("Channel"))
            .map(|node| ChannelMetadata {
                name: node.attribute("Name").map(str::to_owned),
                color: node
                    .attribute("Color")
                    .and_then(parse_i32)
                    .map(|value| value as u32),
                emission_wavelength_nm: node.attribute("EmissionWavelength").and_then(parse_f64),
                excitation_wavelength_nm: node
                    .attribute("ExcitationWavelength")
                    .and_then(parse_f64),
            })
            .collect::<Vec<_>>();

        let mut planes = vec![
            PlaneMetadata::default();
            (size_z.saturating_mul(size_c_logical).saturating_mul(size_t))
                as usize
        ];
        for plane_node in pixels.children().filter(|node| node.has_tag_name("Plane")) {
            let z = plane_node
                .attribute("TheZ")
                .and_then(parse_u32)
                .unwrap_or(0);
            let c = plane_node
                .attribute("TheC")
                .and_then(parse_u32)
                .unwrap_or(0);
            let t = plane_node
                .attribute("TheT")
                .and_then(parse_u32)
                .unwrap_or(0);
            let index = plane_index_for_dimension_order(
                dimension_order,
                size_z,
                size_c_logical,
                size_t,
                z,
                c,
                t,
            );
            if let Some(plane) = planes.get_mut(index as usize) {
                *plane = PlaneMetadata {
                    z,
                    c,
                    t,
                    delta_t_seconds: plane_node.attribute("DeltaT").and_then(parse_f64),
                    position_x_um: plane_node.attribute("PositionX").and_then(parse_f64),
                    position_y_um: plane_node.attribute("PositionY").and_then(parse_f64),
                    position_z_um: plane_node.attribute("PositionZ").and_then(parse_f64),
                };
            }
        }

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
                    ifd: node.attribute("IFD").and_then(parse_u32).unwrap_or(0) as usize,
                    first_z: node.attribute("FirstZ").and_then(parse_u32).unwrap_or(0),
                    first_c: node.attribute("FirstC").and_then(parse_u32).unwrap_or(0),
                    first_t: node.attribute("FirstT").and_then(parse_u32).unwrap_or(0),
                    plane_count: node
                        .attribute("PlaneCount")
                        .and_then(parse_u32)
                        .unwrap_or(1),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        if tiff_data.is_empty() {
            if let Some(default_file) = default_file {
                used_files.insert(
                    default_file.to_string_lossy().to_string(),
                    default_file.to_path_buf(),
                );
            }
        }

        images.push(OmeImage {
            size_x,
            size_y,
            size_z,
            size_c_logical,
            size_t,
            pixel_type,
            dimension_order,
            channels,
            planes,
            tiff_data,
            physical_size_x_um: pixels.attribute("PhysicalSizeX").and_then(parse_f64),
            physical_size_y_um: pixels.attribute("PhysicalSizeY").and_then(parse_f64),
            physical_size_z_um: pixels.attribute("PhysicalSizeZ").and_then(parse_f64),
            time_increment_seconds: pixels.attribute("TimeIncrement").and_then(parse_f64),
            acquisition_timestamp,
            objective_model: objective_model.clone(),
            objective_magnification,
            objective_na,
        });
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

fn parse_i32(value: &str) -> Option<i32> {
    value.parse().ok()
}

fn parse_f64(value: &str) -> Option<f64> {
    value.parse().ok()
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
            let mut planes = vec![
                None;
                image
                    .size_z
                    .saturating_mul(image.size_c_logical)
                    .saturating_mul(image.size_t) as usize
            ];
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
                    let start = plane_index_for_dimension_order(
                        image.dimension_order,
                        image.size_z,
                        image.size_c_logical,
                        image.size_t,
                        tiff_data.first_z,
                        tiff_data.first_c,
                        tiff_data.first_t,
                    );
                    for offset in 0..tiff_data.plane_count {
                        if let Some(slot) = planes.get_mut((start + offset) as usize) {
                            *slot = Some(PlaneRef {
                                file_index,
                                ifd_index: tiff_data.ifd + offset as usize,
                                sub_resolution: 0,
                            });
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

            let first_plane = &root_planes[0];
            let first_file = &minimal.files[first_plane.file_index];
            let ifd = first_file
                .ifds
                .get(first_plane.ifd_index)
                .ok_or_else(|| BioFormatsError::PlaneOutOfRange(first_plane.ifd_index as u32))?;
            let info = ifd_info(ifd, first_file.parser.little_endian)?;
            let rgb_count = if matches!(info.photometric, Photometric::Rgb | Photometric::YCbCr)
                && info.samples_per_pixel >= 3
            {
                info.samples_per_pixel as u32
            } else {
                1
            };
            let mut metadata = ImageMetadata {
                size_x: image.size_x,
                size_y: image.size_y,
                size_z: image.size_z,
                size_c: image.size_c_logical.saturating_mul(rgb_count),
                size_t: image.size_t,
                pixel_type: image.pixel_type,
                bits_per_pixel: (image.pixel_type.bytes_per_sample() * 8) as u8,
                image_count: image
                    .size_z
                    .saturating_mul(image.size_c_logical)
                    .saturating_mul(image.size_t),
                dimension_order: image.dimension_order,
                is_rgb: rgb_count > 1,
                is_interleaved: info.planar_config == 1,
                is_indexed: info.photometric == Photometric::Palette,
                is_false_color: true,
                is_little_endian: first_file.parser.little_endian,
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
                plane_metadata: image.planes.clone(),
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
