//! Nikon ND2 format reader.
//!
//! This reader still covers a subset of Bio-Formats ND2, but it now models:
//! - explicit logical channel vs packed RGB semantics
//! - explicit series and plane maps
//! - typed metadata extraction from textual ND2 metadata chunks when present
//!
//! Compression: raw bytes or zlib. JPEG2000 is detected but not decoded.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{
    ChannelMetadata, DimensionOrder, ImageMetadata, MetadataValue, PlaneMetadata,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::snapshot::ReaderSnapshot;
use roxmltree::Document;
use serde::{Deserialize, Serialize};

pub const ND2_MAGIC: [u8; 4] = [0xDA, 0xCE, 0xBE, 0x0A];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nd2Chunk {
    name: String,
    data_offset: u64,
    data_length: u64,
}

fn scan_chunks(file: &mut BufReader<File>) -> std::io::Result<Vec<Nd2Chunk>> {
    let mut chunks = Vec::new();
    file.seek(SeekFrom::Start(0))?;

    loop {
        let mut magic = [0u8; 4];
        if file.read_exact(&mut magic).is_err() {
            break;
        }
        if magic != ND2_MAGIC {
            break;
        }

        let mut name_len_bytes = [0u8; 4];
        file.read_exact(&mut name_len_bytes)?;
        let name_len = u32::from_le_bytes(name_len_bytes) as usize;

        let mut data_len_bytes = [0u8; 8];
        file.read_exact(&mut data_len_bytes)?;
        let data_len = u64::from_le_bytes(data_len_bytes);

        let mut name_bytes = vec![0u8; name_len];
        file.read_exact(&mut name_bytes)?;
        let name = String::from_utf8_lossy(&name_bytes)
            .trim_end_matches('\0')
            .to_string();

        let data_offset = file.stream_position()?;
        chunks.push(Nd2Chunk {
            name,
            data_offset,
            data_length: data_len,
        });
        file.seek(SeekFrom::Start(data_offset + data_len))?;
    }

    Ok(chunks)
}

fn read_chunk_data(file: &mut BufReader<File>, chunk: &Nd2Chunk) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(chunk.data_offset))?;
    let mut data = vec![0u8; chunk.data_length as usize];
    file.read_exact(&mut data)?;
    Ok(data)
}

fn is_textual_metadata_chunk(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !lower.starts_with("imagedataseq")
        && (lower.contains("metadata")
            || lower.contains("attrib")
            || lower.contains("text")
            || lower.contains("calibra")
            || lower.contains("customdata"))
}

fn looks_like_xml(data: &[u8]) -> bool {
    let trimmed = data
        .iter()
        .copied()
        .skip_while(|byte| byte.is_ascii_whitespace() || *byte == 0);
    matches!(trimmed.into_iter().next(), Some(b'<'))
}

fn detect_jpeg2000(data: &[u8]) -> bool {
    data.starts_with(&[0xff, 0x4f, 0xff, 0x51])
        || data.starts_with(&[0x00, 0x00, 0x00, 0x0c, 0x6a, 0x50, 0x20, 0x20])
}

#[derive(Debug, Default)]
struct Nd2MetadataModel {
    size_x: Option<u32>,
    size_y: Option<u32>,
    logical_channels: Option<u32>,
    size_z: Option<u32>,
    size_t: Option<u32>,
    series_count: Option<u32>,
    bits_per_pixel: Option<u8>,
    physical_size_x_um: Option<f64>,
    physical_size_y_um: Option<f64>,
    physical_size_z_um: Option<f64>,
    time_increment_seconds: Option<f64>,
    objective_model: Option<String>,
    objective_magnification: Option<f64>,
    channel_metadata: Vec<ChannelMetadata>,
    exposure_times_seconds: Vec<f64>,
    timepoints_seconds: Vec<f64>,
    positions_x_um: Vec<f64>,
    positions_y_um: Vec<f64>,
    positions_z_um: Vec<f64>,
}

fn first_text_value(document: &Document<'_>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        document
            .descendants()
            .find(|node| node.is_element() && node.tag_name().name() == *name)
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn collect_text_values(document: &Document<'_>, names: &[&str]) -> Vec<String> {
    document
        .descendants()
        .filter(|node| node.is_element() && names.contains(&node.tag_name().name()))
        .filter_map(|node| node.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_nd2_text_metadata(fragments: &[String]) -> Nd2MetadataModel {
    let xml = fragments
        .iter()
        .filter(|fragment| fragment.contains('<'))
        .map(String::as_str)
        .collect::<String>();
    if xml.trim().is_empty() {
        return Nd2MetadataModel::default();
    }
    let wrapped = format!("<ND2>{xml}</ND2>");
    let Ok(document) = Document::parse(&wrapped) else {
        return Nd2MetadataModel::default();
    };

    let mut metadata = Nd2MetadataModel::default();
    metadata.size_x = first_text_value(&document, &["uiWidth", "uiCamPxlCountX"])
        .and_then(|value| value.parse::<u32>().ok());
    metadata.size_y = first_text_value(&document, &["uiHeight", "uiCamPxlCountY"])
        .and_then(|value| value.parse::<u32>().ok());
    metadata.logical_channels = first_text_value(&document, &["ChannelCount", "uiComp"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    metadata.size_z = first_text_value(&document, &["zCount", "uiZStackHome"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    metadata.size_t = first_text_value(&document, &["timeCount", "TimeCount"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    metadata.series_count = first_text_value(&document, &["XYCount", "SeriesCount"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    metadata.bits_per_pixel =
        first_text_value(&document, &["uiBpcInMemory", "uiBpcSignificant", "uiBpc"])
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value > 0);

    let calibrations = collect_text_values(&document, &["dCalibration"])
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .collect::<Vec<_>>();
    metadata.physical_size_x_um = calibrations.first().copied().filter(|value| *value > 0.0);
    metadata.physical_size_y_um = calibrations
        .get(1)
        .copied()
        .or(metadata.physical_size_x_um)
        .filter(|value| *value > 0.0);
    metadata.physical_size_z_um = first_text_value(&document, &["dZStep"])
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0);

    metadata.time_increment_seconds = first_text_value(&document, &["TimeIncrement", "dTimeStep"])
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0);
    metadata.objective_magnification = first_text_value(&document, &["dObjectiveMag"])
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0);
    metadata.objective_model = first_text_value(&document, &["sObjective"]);

    let names = collect_text_values(&document, &["sDescription"]);
    let excitation = collect_text_values(&document, &["ExcitationWavelength", "ExWavelength"])
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .collect::<Vec<_>>();
    let emission = collect_text_values(&document, &["EmissionWavelength", "EmWavelength"])
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .collect::<Vec<_>>();
    let channel_count = metadata.logical_channels.unwrap_or(0) as usize;
    let channel_len = channel_count
        .max(names.len())
        .max(excitation.len())
        .max(emission.len());
    metadata.channel_metadata = (0..channel_len)
        .map(|index| ChannelMetadata {
            name: names.get(index).cloned(),
            color: None,
            emission_wavelength_nm: emission.get(index).copied(),
            excitation_wavelength_nm: excitation.get(index).copied(),
        })
        .collect();

    metadata.exposure_times_seconds = collect_text_values(&document, &["dExposureTime"])
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .map(|value| value / 1000.0)
        .collect();
    metadata.timepoints_seconds =
        collect_text_values(&document, &["dTimeMSec", "TimeMSec", "dTime"])
            .into_iter()
            .filter_map(|value| value.parse::<f64>().ok())
            .map(|value| value / 1000.0)
            .collect();
    metadata.positions_x_um = collect_text_values(&document, &["dPosX"])
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .collect();
    metadata.positions_y_um = collect_text_values(&document, &["dPosY"])
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .collect();
    metadata.positions_z_um = collect_text_values(&document, &["dPosZ"])
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .collect();

    if metadata.series_count.is_none() && !metadata.positions_x_um.is_empty() {
        metadata.series_count = Some(metadata.positions_x_um.len() as u32);
    }

    metadata
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nd2Series {
    metadata: ImageMetadata,
    planes: Vec<usize>,
    samples_per_pixel: u32,
}

pub struct Nd2Reader {
    file: Option<BufReader<File>>,
    path: Option<PathBuf>,
    chunks: Vec<Nd2Chunk>,
    image_chunks: Vec<usize>,
    series: Vec<Nd2Series>,
    current_series: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nd2ReaderSnapshot {
    pub path: PathBuf,
    pub chunks: Vec<Nd2Chunk>,
    pub image_chunks: Vec<usize>,
    pub series: Vec<Nd2Series>,
    pub current_series: usize,
}

impl Nd2Reader {
    pub fn new() -> Self {
        Self {
            file: None,
            path: None,
            chunks: Vec::new(),
            image_chunks: Vec::new(),
            series: Vec::new(),
            current_series: 0,
        }
    }

    pub fn from_snapshot(snapshot: Nd2ReaderSnapshot) -> Result<Self> {
        let file = File::open(&snapshot.path).map_err(BioFormatsError::Io)?;
        Ok(Self {
            file: Some(BufReader::new(file)),
            path: Some(snapshot.path),
            chunks: snapshot.chunks,
            image_chunks: snapshot.image_chunks,
            series: snapshot.series,
            current_series: snapshot.current_series,
        })
    }

    fn collect_metadata_fragments(file: &mut BufReader<File>, chunks: &[Nd2Chunk]) -> Vec<String> {
        chunks
            .iter()
            .filter(|chunk| is_textual_metadata_chunk(&chunk.name))
            .filter_map(|chunk| read_chunk_data(file, chunk).ok())
            .filter(|data| looks_like_xml(data))
            .filter_map(|data| String::from_utf8(data).ok())
            .collect()
    }

    fn infer_packed_rgb(
        logical_channels: u32,
        series_count: u32,
        size_z: u32,
        size_t: u32,
        image_chunk_count: usize,
        first_chunk_len: Option<u64>,
        size_x: u32,
        size_y: u32,
        bytes_per_sample: usize,
    ) -> bool {
        if logical_channels < 3 {
            return false;
        }
        let packed_planes = series_count.saturating_mul(size_z).saturating_mul(size_t);
        if packed_planes > 0 && packed_planes as usize == image_chunk_count {
            return true;
        }
        let planar_planes = packed_planes.saturating_mul(logical_channels);
        if planar_planes > 0 && planar_planes as usize == image_chunk_count {
            return false;
        }
        let Some(first_chunk_len) = first_chunk_len else {
            return false;
        };
        let mono_expected = size_x as u64 * size_y as u64 * bytes_per_sample as u64;
        let rgb_expected = mono_expected.saturating_mul(logical_channels as u64);
        first_chunk_len >= rgb_expected && first_chunk_len < rgb_expected.saturating_add(4096)
    }

    fn build_series(
        model: &Nd2MetadataModel,
        image_chunks: &[usize],
        chunk_lengths: &[u64],
    ) -> Result<Vec<Nd2Series>> {
        let series_count = model.series_count.unwrap_or(1).max(1);
        let logical_channels = model.logical_channels.unwrap_or(1).max(1);
        let size_x = model.size_x.unwrap_or(0);
        let size_y = model.size_y.unwrap_or(0);
        let bits_per_pixel = model.bits_per_pixel.unwrap_or(8).max(8);
        let pixel_type = match bits_per_pixel {
            8 => PixelType::Uint8,
            16 => PixelType::Uint16,
            32 => PixelType::Uint32,
            _ => PixelType::Uint16,
        };
        let bytes_per_sample = pixel_type.bytes_per_sample();
        let size_z = model.size_z.unwrap_or(1).max(1);
        let mut size_t = model.size_t.unwrap_or(1).max(1);

        let packed_rgb = Self::infer_packed_rgb(
            logical_channels,
            series_count,
            size_z,
            size_t,
            image_chunks.len(),
            chunk_lengths.first().copied(),
            size_x,
            size_y,
            bytes_per_sample,
        );
        let samples_per_pixel = if packed_rgb { logical_channels } else { 1 };
        let planes_per_zt = if packed_rgb { 1 } else { logical_channels };
        let planes_per_series = (image_chunks.len() as u32 / series_count).max(1);

        if size_z.saturating_mul(size_t).saturating_mul(planes_per_zt) != planes_per_series {
            if size_z > 1 && planes_per_series % (size_z * planes_per_zt) == 0 {
                size_t = (planes_per_series / (size_z * planes_per_zt)).max(1);
            } else if size_t > 1 && planes_per_series % (size_t * planes_per_zt) == 0 {
                let inferred_z = (planes_per_series / (size_t * planes_per_zt)).max(1);
                if inferred_z > 0 {
                    return Self::build_series(
                        &Nd2MetadataModel {
                            size_z: Some(inferred_z),
                            size_t: Some(size_t),
                            ..Nd2MetadataModel {
                                size_x: model.size_x,
                                size_y: model.size_y,
                                logical_channels: model.logical_channels,
                                size_z: model.size_z,
                                size_t: model.size_t,
                                series_count: model.series_count,
                                bits_per_pixel: model.bits_per_pixel,
                                physical_size_x_um: model.physical_size_x_um,
                                physical_size_y_um: model.physical_size_y_um,
                                physical_size_z_um: model.physical_size_z_um,
                                time_increment_seconds: model.time_increment_seconds,
                                objective_model: model.objective_model.clone(),
                                objective_magnification: model.objective_magnification,
                                channel_metadata: model.channel_metadata.clone(),
                                exposure_times_seconds: model.exposure_times_seconds.clone(),
                                timepoints_seconds: model.timepoints_seconds.clone(),
                                positions_x_um: model.positions_x_um.clone(),
                                positions_y_um: model.positions_y_um.clone(),
                                positions_z_um: model.positions_z_um.clone(),
                            }
                        },
                        image_chunks,
                        chunk_lengths,
                    );
                }
            } else if model.size_t.is_none() {
                size_t = (planes_per_series / (size_z * planes_per_zt)).max(1);
            }
        }

        let image_count = size_z.saturating_mul(size_t).saturating_mul(planes_per_zt);
        let zc_planes = size_z.saturating_mul(planes_per_zt);
        let mut series_chunks = vec![Vec::new(); series_count as usize];
        let mut assigned = 0usize;
        for t in 0..size_t {
            for series_index in 0..series_count as usize {
                for plane in 0..zc_planes {
                    let chunk_index = t as usize * series_count as usize * zc_planes as usize
                        + series_index * zc_planes as usize
                        + plane as usize;
                    if let Some(image_chunk) = image_chunks.get(chunk_index) {
                        series_chunks[series_index].push(*image_chunk);
                        assigned += 1;
                    }
                }
            }
        }
        if assigned != image_chunks.len() {
            let per_series = (image_chunks.len() / series_count as usize).max(1);
            for (series_index, chunks) in series_chunks.iter_mut().enumerate() {
                let start = series_index * per_series;
                let end = ((series_index + 1) * per_series).min(image_chunks.len());
                if chunks.is_empty() && start < end {
                    chunks.extend_from_slice(&image_chunks[start..end]);
                }
            }
        }

        let mut series = Vec::new();
        for (series_index, planes) in series_chunks.into_iter().enumerate() {
            let mut metadata = ImageMetadata {
                size_x,
                size_y,
                size_z,
                size_c: if packed_rgb {
                    samples_per_pixel
                } else {
                    logical_channels
                },
                size_t,
                pixel_type,
                bits_per_pixel,
                image_count: image_count.max(planes.len() as u32),
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: packed_rgb,
                is_interleaved: packed_rgb,
                is_indexed: false,
                is_false_color: true,
                is_little_endian: true,
                resolution_count: 1,
                series_metadata: HashMap::new(),
                lookup_table: None,
                physical_size_x_um: model.physical_size_x_um,
                physical_size_y_um: model.physical_size_y_um,
                physical_size_z_um: model.physical_size_z_um,
                time_increment_seconds: model.time_increment_seconds,
                acquisition_timestamp: None,
                objective_model: model.objective_model.clone(),
                objective_magnification: model.objective_magnification,
                objective_na: None,
                channel_metadata: if !packed_rgb
                    && model.channel_metadata.len() >= logical_channels as usize
                {
                    model.channel_metadata[..logical_channels as usize].to_vec()
                } else {
                    model.channel_metadata.clone()
                },
                plane_metadata: Vec::new(),
                used_files: Vec::new(),
            };
            metadata
                .series_metadata
                .insert("nd2_chunks".into(), MetadataValue::Int(planes.len() as i64));
            metadata.series_metadata.insert(
                "nd2_series_index".into(),
                MetadataValue::Int(series_index as i64),
            );

            let logical_channels_for_index = if packed_rgb { 1 } else { logical_channels };
            let index_meta = ImageMetadata {
                size_z,
                size_c: logical_channels_for_index,
                size_t,
                image_count: size_z
                    .saturating_mul(size_t)
                    .saturating_mul(logical_channels_for_index),
                dimension_order: DimensionOrder::XYCZT,
                ..ImageMetadata::default()
            };

            metadata.plane_metadata = (0..metadata.image_count)
                .map(|plane_index| {
                    let (z, c, t) = index_meta.get_zct_coords(plane_index);
                    let absolute_index =
                        series_index * metadata.image_count as usize + plane_index as usize;
                    PlaneMetadata {
                        z,
                        c,
                        t,
                        delta_t_seconds: model
                            .timepoints_seconds
                            .get(absolute_index)
                            .copied()
                            .or_else(|| {
                                metadata.time_increment_seconds.map(|step| step * t as f64)
                            }),
                        position_x_um: model.positions_x_um.get(series_index).copied(),
                        position_y_um: model.positions_y_um.get(series_index).copied(),
                        position_z_um: model
                            .positions_z_um
                            .get(absolute_index)
                            .copied()
                            .or_else(|| model.positions_z_um.get(series_index).copied()),
                    }
                })
                .collect();

            series.push(Nd2Series {
                metadata,
                planes,
                samples_per_pixel,
            });
        }

        Ok(series)
    }

    fn current_series(&self) -> Result<&Nd2Series> {
        self.series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)
    }
}

impl Default for Nd2Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Nd2Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("nd2"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(&ND2_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(file);
        let chunks = scan_chunks(&mut reader).map_err(BioFormatsError::Io)?;
        let image_chunks = chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| chunk.name.starts_with("ImageDataSeq"))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let chunk_lengths = image_chunks
            .iter()
            .filter_map(|index| chunks.get(*index).map(|chunk| chunk.data_length))
            .collect::<Vec<_>>();

        let fragments = Self::collect_metadata_fragments(&mut reader, &chunks);
        let mut metadata = parse_nd2_text_metadata(&fragments);

        if metadata.size_x.unwrap_or(0) == 0 || metadata.size_y.unwrap_or(0) == 0 {
            if let Some(first_chunk_len) = chunk_lengths.first().copied() {
                let logical_channels = metadata.logical_channels.unwrap_or(1).max(1);
                let bpp = metadata.bits_per_pixel.unwrap_or(8).max(8);
                let bytes_per_sample = ((bpp as u64 + 7) / 8).max(1);
                let total_pixels = first_chunk_len / bytes_per_sample / logical_channels as u64;
                let side = (total_pixels as f64).sqrt() as u32;
                if side > 0 {
                    metadata.size_x.get_or_insert(side);
                    metadata.size_y.get_or_insert(side);
                }
            }
        }

        let series = Self::build_series(&metadata, &image_chunks, &chunk_lengths)?;
        self.file = Some(reader);
        self.path = Some(path.to_path_buf());
        self.chunks = chunks;
        self.image_chunks = image_chunks;
        self.series = series;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.file = None;
        self.path = None;
        self.chunks.clear();
        self.image_chunks.clear();
        self.series.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        &self
            .series
            .get(self.current_series)
            .expect("set_id not called")
            .metadata
    }

    fn current_file(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self.current_series()?.clone();
        if plane_index >= series.metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let chunk_index = *series
            .planes
            .get(plane_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let chunk = self
            .chunks
            .get(chunk_index)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let data = read_chunk_data(file, chunk).map_err(BioFormatsError::Io)?;

        let expected = series.metadata.size_x as usize
            * series.metadata.size_y as usize
            * series.samples_per_pixel as usize
            * series.metadata.pixel_type.bytes_per_sample();

        if data.len() >= expected {
            return Ok(data[data.len() - expected..].to_vec());
        }

        use flate2::read::ZlibDecoder;
        let mut decoder = ZlibDecoder::new(data.as_slice());
        let mut decompressed = Vec::new();
        if decoder.read_to_end(&mut decompressed).is_ok() && decompressed.len() >= expected {
            return Ok(decompressed[decompressed.len() - expected..].to_vec());
        }

        if detect_jpeg2000(&data) {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2: JPEG2000 compression not yet supported".into(),
            ));
        }

        Err(BioFormatsError::Format(format!(
            "ND2: plane {} data too small ({} < {})",
            plane_index,
            data.len(),
            expected
        )))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let series = self.current_series()?;
        let row_bytes = series.metadata.size_x as usize
            * series.samples_per_pixel as usize
            * series.metadata.pixel_type.bytes_per_sample();
        let out_row = w as usize
            * series.samples_per_pixel as usize
            * series.metadata.pixel_type.bytes_per_sample();
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let start = x as usize
                * series.samples_per_pixel as usize
                * series.metadata.pixel_type.bytes_per_sample();
            out.extend_from_slice(&src[start..start + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let metadata = &self.current_series()?.metadata;
        let (thumb_w, thumb_h) = (metadata.size_x.min(256), metadata.size_y.min(256));
        let (thumb_x, thumb_y) = (
            (metadata.size_x - thumb_w) / 2,
            (metadata.size_y - thumb_h) / 2,
        );
        self.open_bytes_region(plane_index, thumb_x, thumb_y, thumb_w, thumb_h)
    }

    fn snapshot(&self) -> Result<ReaderSnapshot> {
        Ok(ReaderSnapshot::Nd2Reader(Nd2ReaderSnapshot {
            path: self.path.clone().ok_or(BioFormatsError::NotInitialized)?,
            chunks: self.chunks.clone(),
            image_chunks: self.image_chunks.clone(),
            series: self.series.clone(),
            current_series: self.current_series,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_planar_three_channel_dataset() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(4),
            logical_channels: Some(3),
            size_z: Some(1),
            size_t: Some(2),
            series_count: Some(1),
            bits_per_pixel: Some(8),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[0, 1, 2, 3, 4, 5], &[16; 6]).unwrap();
        assert_eq!(series.len(), 1);
        assert!(!series[0].metadata.is_rgb);
        assert_eq!(series[0].metadata.size_c, 3);
        assert_eq!(series[0].metadata.image_count, 6);
        assert_eq!(series[0].samples_per_pixel, 1);
    }

    #[test]
    fn infers_packed_rgb_dataset() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(4),
            logical_channels: Some(3),
            size_z: Some(1),
            size_t: Some(2),
            series_count: Some(1),
            bits_per_pixel: Some(8),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[0, 1], &[48, 48]).unwrap();
        assert_eq!(series[0].metadata.image_count, 2);
        assert!(series[0].metadata.is_rgb);
        assert_eq!(series[0].metadata.logical_channel_count(), 1);
        assert_eq!(series[0].samples_per_pixel, 3);
    }

    #[test]
    fn assigns_time_major_multi_series_planes() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(4),
            logical_channels: Some(1),
            size_z: Some(1),
            size_t: Some(2),
            series_count: Some(2),
            bits_per_pixel: Some(8),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[10, 11, 12, 13], &[16; 4]).unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].planes, vec![10, 12]);
        assert_eq!(series[1].planes, vec![11, 13]);
    }

    #[test]
    fn parses_text_metadata_fields() {
        let metadata = parse_nd2_text_metadata(&[r#"
<Root>
  <uiWidth>16</uiWidth>
  <uiHeight>8</uiHeight>
  <uiComp>2</uiComp>
  <zCount>3</zCount>
  <timeCount>4</timeCount>
  <XYCount>2</XYCount>
  <uiBpcSignificant>16</uiBpcSignificant>
  <dCalibration>0.25</dCalibration>
  <dCalibration>0.5</dCalibration>
  <dZStep>1.5</dZStep>
  <dObjectiveMag>60</dObjectiveMag>
  <sObjective>Plan Apo</sObjective>
  <sDescription>GFP</sDescription>
  <sDescription>RFP</sDescription>
  <EmWavelength>520</EmWavelength>
  <EmWavelength>610</EmWavelength>
  <dExposureTime>100</dExposureTime>
  <dExposureTime>150</dExposureTime>
  <dPosX>1.0</dPosX>
  <dPosX>2.0</dPosX>
</Root>
"#
        .to_string()]);
        assert_eq!(metadata.size_x, Some(16));
        assert_eq!(metadata.size_y, Some(8));
        assert_eq!(metadata.logical_channels, Some(2));
        assert_eq!(metadata.size_z, Some(3));
        assert_eq!(metadata.size_t, Some(4));
        assert_eq!(metadata.series_count, Some(2));
        assert_eq!(metadata.bits_per_pixel, Some(16));
        assert_eq!(metadata.physical_size_x_um, Some(0.25));
        assert_eq!(metadata.physical_size_y_um, Some(0.5));
        assert_eq!(metadata.physical_size_z_um, Some(1.5));
        assert_eq!(metadata.objective_magnification, Some(60.0));
        assert_eq!(metadata.objective_model.as_deref(), Some("Plan Apo"));
        assert_eq!(metadata.channel_metadata[0].name.as_deref(), Some("GFP"));
        assert_eq!(
            metadata.channel_metadata[1].emission_wavelength_nm,
            Some(610.0)
        );
        assert_eq!(metadata.exposure_times_seconds, vec![0.1, 0.15]);
        assert_eq!(metadata.positions_x_um, vec![1.0, 2.0]);
    }
}
