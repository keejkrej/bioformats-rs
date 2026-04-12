//! Zeiss CZI (ZISRAWFILE) format reader.
//!
//! This reader ports a pragmatic subset of Bio-Formats' dataset modelling:
//! - explicit logical channel vs. RGB sample separation
//! - multi-series grouping across scene/acquisition/angle/mosaic dimensions
//! - multi-file dataset discovery
//! - typed metadata extraction from the CZI metadata XML
//!
//! Supported compressions: Uncompressed, JPEG (new-style), LZW, Zstd.
//! JPEG-XR is detected but not decoded.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{
    ChannelMetadata, DimensionOrder, ImageMetadata, MetadataValue, PlaneMetadata,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::snapshot::ReaderSnapshot;
use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CziPixelInfo {
    pixel_type: PixelType,
    samples_per_pixel: u32,
    rgb: bool,
    bgr_order: bool,
}

fn czi_pixel_info(code: i32) -> CziPixelInfo {
    match code {
        0 => CziPixelInfo {
            pixel_type: PixelType::Uint8,
            samples_per_pixel: 1,
            rgb: false,
            bgr_order: false,
        },
        1 => CziPixelInfo {
            pixel_type: PixelType::Uint16,
            samples_per_pixel: 1,
            rgb: false,
            bgr_order: false,
        },
        2 => CziPixelInfo {
            pixel_type: PixelType::Float32,
            samples_per_pixel: 1,
            rgb: false,
            bgr_order: false,
        },
        3 => CziPixelInfo {
            pixel_type: PixelType::Uint8,
            samples_per_pixel: 3,
            rgb: true,
            bgr_order: true,
        },
        4 => CziPixelInfo {
            pixel_type: PixelType::Uint16,
            samples_per_pixel: 3,
            rgb: true,
            bgr_order: true,
        },
        8 => CziPixelInfo {
            pixel_type: PixelType::Float32,
            samples_per_pixel: 3,
            rgb: true,
            bgr_order: true,
        },
        9 => CziPixelInfo {
            pixel_type: PixelType::Uint8,
            samples_per_pixel: 4,
            rgb: true,
            bgr_order: true,
        },
        10 | 11 => CziPixelInfo {
            pixel_type: PixelType::Float32,
            samples_per_pixel: 2,
            rgb: false,
            bgr_order: false,
        },
        12 => CziPixelInfo {
            pixel_type: PixelType::Uint32,
            samples_per_pixel: 1,
            rgb: false,
            bgr_order: false,
        },
        13 => CziPixelInfo {
            pixel_type: PixelType::Float64,
            samples_per_pixel: 1,
            rgb: false,
            bgr_order: false,
        },
        _ => CziPixelInfo {
            pixel_type: PixelType::Uint8,
            samples_per_pixel: 1,
            rgb: false,
            bgr_order: false,
        },
    }
}

const SEG_HEADER: usize = 32;

fn read_seg_type(data: &[u8]) -> String {
    let end = data[..16].iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&data[..end]).into_owned()
}

fn read_i32(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn read_i64(data: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]))
}

fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    pixel_type: i32,
    file_position: i64,
    compression: i32,
    dims: HashMap<String, (i32, i32)>,
}

fn parse_dir_entry(data: &[u8]) -> DirEntry {
    let pixel_type = read_i32(data, 2);
    let file_position = read_i64(data, 6);
    let compression = read_i32(data, 18);
    let dim_count = read_i32(data, 28) as usize;

    let mut dims = HashMap::new();
    let dim_array_start = 32;
    for i in 0..dim_count {
        let off = dim_array_start + i * 20;
        if off + 20 > data.len() {
            break;
        }
        let dim_name = std::str::from_utf8(&data[off..off + 4])
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let start = read_i32(data, off + 4);
        let size = read_i32(data, off + 8);
        if !dim_name.is_empty() {
            dims.insert(dim_name, (start, size));
        }
    }

    DirEntry {
        pixel_type,
        file_position,
        compression,
        dims,
    }
}

struct CziParsedFile {
    meta_xml: String,
    entries: Vec<DirEntry>,
}

fn parse_czi_file(f: &mut BufReader<File>) -> std::io::Result<CziParsedFile> {
    let mut hdr = vec![0u8; SEG_HEADER];
    f.read_exact(&mut hdr)?;
    let seg_type = read_seg_type(&hdr);
    if !seg_type.starts_with("ZISRAWFILE") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Not a CZI file",
        ));
    }

    let mut fh = vec![0u8; 80];
    f.read_exact(&mut fh)?;
    let dir_position = read_u64(&fh, 36);
    let meta_position = read_u64(&fh, 44);

    let mut meta_xml = String::new();
    if meta_position > 0 {
        f.seek(SeekFrom::Start(meta_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        let mut meta_body_hdr = vec![0u8; 256];
        f.read_exact(&mut meta_body_hdr)?;
        let xml_size = read_i32(&meta_body_hdr, 0) as usize;
        if xml_size > 0 {
            let mut xml_bytes = vec![0u8; xml_size];
            f.read_exact(&mut xml_bytes)?;
            meta_xml = String::from_utf8_lossy(&xml_bytes).into_owned();
        }
    }

    let mut entries = Vec::new();
    if dir_position > 0 {
        f.seek(SeekFrom::Start(dir_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        let mut dir_hdr = vec![0u8; 128];
        f.read_exact(&mut dir_hdr)?;
        let entry_count = read_i32(&dir_hdr, 0) as usize;
        for _ in 0..entry_count {
            let mut entry_buf = vec![0u8; 256];
            if f.read_exact(&mut entry_buf).is_err() {
                break;
            }
            entries.push(parse_dir_entry(&entry_buf));
        }
    }

    Ok(CziParsedFile { meta_xml, entries })
}

fn decompress_subblock(data: &[u8], compression: i32) -> Result<Vec<u8>> {
    match compression {
        0 => Ok(data.to_vec()),
        1 => {
            let mut dec = jpeg_decoder::Decoder::new(data);
            dec.decode()
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        2 => {
            use weezl::{decode::Decoder, BitOrder};
            let mut dec = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
            dec.decode(data)
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        4 => Err(BioFormatsError::UnsupportedFormat(
            "CZI: JPEG-XR compression not yet supported".into(),
        )),
        5 | 6 => zstd::decode_all(data).map_err(BioFormatsError::Io),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "CZI: unknown compression {}",
            compression
        ))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CziLocatedEntry {
    file_index: usize,
    entry: DirEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CziPlaneRef {
    entry_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CziSeries {
    metadata: ImageMetadata,
    planes: Vec<CziPlaneRef>,
    samples_per_pixel: u32,
    bgr_order: bool,
}

#[derive(Debug, Default)]
struct CziMetadataModel {
    physical_size_x_um: Option<f64>,
    physical_size_y_um: Option<f64>,
    physical_size_z_um: Option<f64>,
    time_increment_seconds: Option<f64>,
    objective_model: Option<String>,
    objective_na: Option<f64>,
    objective_magnification: Option<f64>,
    channel_metadata: Vec<ChannelMetadata>,
    scene_positions: Vec<(Option<f64>, Option<f64>, Option<f64>)>,
}

fn child_element_text(node: Node<'_, '_>, child_name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == child_name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn parse_f64(value: Option<&str>) -> Option<f64> {
    value.and_then(|value| value.trim().parse::<f64>().ok())
}

fn parse_czi_metadata(xml: &str) -> CziMetadataModel {
    let Ok(document) = Document::parse(xml) else {
        return CziMetadataModel::default();
    };

    let mut metadata = CziMetadataModel::default();

    for distance in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Distance")
    {
        let Some(id) = distance.attribute("Id") else {
            continue;
        };
        let Some(value) =
            child_element_text(distance, "Value").and_then(|value| value.parse::<f64>().ok())
        else {
            continue;
        };
        let value_um = value * 1_000_000.0;
        match id {
            "X" if value_um > 0.0 => metadata.physical_size_x_um = Some(value_um),
            "Y" if value_um > 0.0 => metadata.physical_size_y_um = Some(value_um),
            "Z" if value_um > 0.0 => metadata.physical_size_z_um = Some(value_um),
            _ => {}
        }
    }

    metadata.time_increment_seconds = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "Increment")
        .and_then(|node| node.text())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| *value > 0.0);

    metadata.objective_model = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "Objective")
        .and_then(|node| {
            node.attribute("Model")
                .map(str::to_owned)
                .or_else(|| child_element_text(node, "Name"))
        });
    metadata.objective_na = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "Objective")
        .and_then(|node| {
            parse_f64(node.attribute("LensNA")).or_else(|| {
                child_element_text(node, "LensNA").and_then(|value| value.parse::<f64>().ok())
            })
        });
    metadata.objective_magnification = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "Objective")
        .and_then(|node| {
            parse_f64(node.attribute("NominalMagnification")).or_else(|| {
                child_element_text(node, "NominalMagnification")
                    .and_then(|value| value.parse::<f64>().ok())
            })
        });

    metadata.channel_metadata = document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Channel")
        .map(|channel| ChannelMetadata {
            name: channel
                .attribute("Name")
                .map(str::to_owned)
                .or_else(|| child_element_text(channel, "Name")),
            color: channel
                .attribute("Color")
                .and_then(|value| value.parse::<u32>().ok()),
            emission_wavelength_nm: channel
                .attribute("EmissionWavelength")
                .and_then(|value| value.parse::<f64>().ok())
                .or_else(|| {
                    child_element_text(channel, "EmissionWavelength")
                        .and_then(|value| value.parse::<f64>().ok())
                }),
            excitation_wavelength_nm: channel
                .attribute("ExcitationWavelength")
                .and_then(|value| value.parse::<f64>().ok())
                .or_else(|| {
                    child_element_text(channel, "ExcitationWavelength")
                        .and_then(|value| value.parse::<f64>().ok())
                }),
        })
        .collect();

    for scene in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Scene")
    {
        let mut added = false;
        for position in scene
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "Position")
        {
            metadata.scene_positions.push((
                position
                    .attribute("X")
                    .and_then(|value| value.parse::<f64>().ok()),
                position
                    .attribute("Y")
                    .and_then(|value| value.parse::<f64>().ok()),
                position
                    .attribute("Z")
                    .and_then(|value| value.parse::<f64>().ok()),
            ));
            added = true;
        }
        if !added {
            if let Some(center) = scene
                .descendants()
                .find(|node| node.is_element() && node.tag_name().name() == "CenterPosition")
                .and_then(|node| node.text())
            {
                let coords = center
                    .split(',')
                    .map(|value| value.trim().parse::<f64>().ok())
                    .collect::<Vec<_>>();
                metadata.scene_positions.push((
                    coords.first().copied().flatten(),
                    coords.get(1).copied().flatten(),
                    coords.get(2).copied().flatten(),
                ));
            }
        }
    }

    metadata
}

fn file_stem_without_part(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(
        stem.strip_suffix(')')
            .and_then(|value| value.rsplit_once(" ("))
            .and_then(|(base, suffix)| suffix.parse::<usize>().ok().map(|_| base))
            .unwrap_or(stem)
            .to_string(),
    )
}

fn czi_part_index(path: &Path) -> usize {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| {
            stem.strip_suffix(')')
                .and_then(|value| value.rsplit_once(" ("))
                .and_then(|(_, suffix)| suffix.parse::<usize>().ok())
        })
        .unwrap_or(0)
}

fn discover_czi_files(path: &Path) -> Vec<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let Some(base) = file_stem_without_part(path) else {
        return vec![path.to_path_buf()];
    };
    let master = parent.join(format!("{base}.czi"));
    let primary = if master.exists() {
        master
    } else {
        path.to_path_buf()
    };

    let Ok(entries) = fs::read_dir(parent) else {
        return vec![primary];
    };

    let mut files = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("czi"))
                .unwrap_or(false)
                && file_stem_without_part(candidate)
                    .map(|candidate_base| candidate_base == base)
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    if !files.iter().any(|candidate| candidate == &primary) {
        files.push(primary.clone());
    }

    files.sort_by(|left, right| {
        let left_primary = *left == primary;
        let right_primary = *right == primary;
        right_primary
            .cmp(&left_primary)
            .then_with(|| czi_part_index(left).cmp(&czi_part_index(right)))
            .then_with(|| left.cmp(right))
    });
    files
}

fn dim_start(entry: &DirEntry, key: &str) -> i32 {
    entry.dims.get(key).map(|(start, _)| *start).unwrap_or(0)
}

fn dim_extent(entry: &DirEntry, key: &str) -> u32 {
    entry
        .dims
        .get(key)
        .map(|(start, size)| (*start + (*size).max(1)) as u32)
        .unwrap_or(1)
}

fn dim_size(entry: &DirEntry, key: &str) -> u32 {
    entry
        .dims
        .get(key)
        .map(|(_, size)| (*size).max(1) as u32)
        .unwrap_or(1)
}

fn plane_priority(entry: &DirEntry) -> (i64, bool) {
    let area = dim_size(entry, "X") as i64 * dim_size(entry, "Y") as i64;
    let origin = dim_start(entry, "X") == 0 && dim_start(entry, "Y") == 0;
    (area, origin)
}

fn bgr_to_rgb_in_place(data: &mut [u8], samples_per_pixel: u32, bytes_per_sample: usize) {
    if samples_per_pixel < 3 {
        return;
    }
    let pixel_stride = samples_per_pixel as usize * bytes_per_sample;
    let third_sample_offset = 2 * bytes_per_sample;
    for pixel in data.chunks_exact_mut(pixel_stride) {
        for offset in 0..bytes_per_sample {
            pixel.swap(offset, third_sample_offset + offset);
        }
    }
}

fn build_czi_series(
    entries: &[CziLocatedEntry],
    metadata_xml: &str,
    used_files: &[PathBuf],
) -> Result<Vec<CziSeries>> {
    if entries.is_empty() {
        return Err(BioFormatsError::Format(
            "CZI dataset contained no readable subblocks".into(),
        ));
    }

    let xml = parse_czi_metadata(metadata_xml);
    let mut grouped = BTreeMap::<(i32, i32, i32, i32), Vec<usize>>::new();
    for (index, entry) in entries.iter().enumerate() {
        let key = (
            dim_start(&entry.entry, "S"),
            dim_start(&entry.entry, "B"),
            dim_start(&entry.entry, "V"),
            dim_start(&entry.entry, "M"),
        );
        grouped.entry(key).or_default().push(index);
    }

    let mut series = Vec::new();
    for ((scene_index, _, _, _), group) in grouped {
        let first_entry = &entries[group[0]].entry;
        let pixel = czi_pixel_info(first_entry.pixel_type);
        let logical_channels = group
            .iter()
            .map(|index| dim_extent(&entries[*index].entry, "C"))
            .max()
            .unwrap_or(1)
            .max(1);
        let size_z = group
            .iter()
            .map(|index| dim_extent(&entries[*index].entry, "Z"))
            .max()
            .unwrap_or(1)
            .max(1);
        let size_t = group
            .iter()
            .map(|index| dim_extent(&entries[*index].entry, "T"))
            .max()
            .unwrap_or(1)
            .max(1);
        let size_x = group
            .iter()
            .map(|index| dim_size(&entries[*index].entry, "X"))
            .max()
            .unwrap_or(0);
        let size_y = group
            .iter()
            .map(|index| dim_size(&entries[*index].entry, "Y"))
            .max()
            .unwrap_or(0);
        let image_count = logical_channels
            .saturating_mul(size_z)
            .saturating_mul(size_t);

        let mut metadata = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: logical_channels.saturating_mul(if pixel.rgb {
                pixel.samples_per_pixel
            } else {
                1
            }),
            size_t,
            pixel_type: pixel.pixel_type,
            bits_per_pixel: (pixel.pixel_type.bytes_per_sample() * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: pixel.rgb,
            is_interleaved: pixel.rgb,
            is_indexed: false,
            is_false_color: true,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            physical_size_x_um: xml.physical_size_x_um,
            physical_size_y_um: xml.physical_size_y_um,
            physical_size_z_um: xml.physical_size_z_um,
            time_increment_seconds: xml.time_increment_seconds,
            acquisition_timestamp: None,
            objective_model: xml.objective_model.clone(),
            objective_magnification: xml.objective_magnification,
            objective_na: xml.objective_na,
            channel_metadata: if xml.channel_metadata.len() >= logical_channels as usize {
                xml.channel_metadata[..logical_channels as usize].to_vec()
            } else {
                xml.channel_metadata.clone()
            },
            plane_metadata: Vec::new(),
            used_files: used_files.to_vec(),
        };
        metadata.series_metadata.insert(
            "czi_subblocks".into(),
            MetadataValue::Int(group.len() as i64),
        );
        metadata.series_metadata.insert(
            "czi_scene_index".into(),
            MetadataValue::Int(scene_index as i64),
        );

        let temp = ImageMetadata {
            size_z,
            size_c: logical_channels,
            size_t,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            ..ImageMetadata::default()
        };
        let mut planes: Vec<Option<usize>> = vec![None; image_count as usize];
        for index in &group {
            let entry = &entries[*index].entry;
            let z = dim_start(entry, "Z").max(0) as u32;
            let c = dim_start(entry, "C").max(0) as u32;
            let t = dim_start(entry, "T").max(0) as u32;
            let plane_index = temp.get_index(z, c, t) as usize;
            if plane_index >= planes.len() {
                continue;
            }
            match planes[plane_index] {
                Some(current) => {
                    let current_entry = &entries[current].entry;
                    if plane_priority(entry) > plane_priority(current_entry) {
                        planes[plane_index] = Some(*index);
                    }
                }
                None => planes[plane_index] = Some(*index),
            }
        }

        let scene_position = usize::try_from(scene_index)
            .ok()
            .and_then(|index| xml.scene_positions.get(index).copied())
            .unwrap_or((None, None, None));

        metadata.plane_metadata = (0..image_count)
            .map(|plane_index| {
                let (z, c, t) = temp.get_zct_coords(plane_index);
                PlaneMetadata {
                    z,
                    c,
                    t,
                    delta_t_seconds: metadata.time_increment_seconds.map(|step| step * t as f64),
                    position_x_um: scene_position.0,
                    position_y_um: scene_position.1,
                    position_z_um: scene_position
                        .2
                        .or_else(|| metadata.physical_size_z_um.map(|step| step * z as f64)),
                }
            })
            .collect();

        let planes = planes
            .into_iter()
            .enumerate()
            .map(|(plane_index, entry_index)| {
                entry_index
                    .map(|entry_index| CziPlaneRef { entry_index })
                    .ok_or_else(|| {
                        BioFormatsError::Format(format!(
                            "CZI series plane {} could not be mapped",
                            plane_index
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        series.push(CziSeries {
            metadata,
            planes,
            samples_per_pixel: pixel.samples_per_pixel,
            bgr_order: pixel.bgr_order,
        });
    }

    Ok(series)
}

pub struct CziReader {
    path: Option<PathBuf>,
    used_files: Vec<PathBuf>,
    entries: Vec<CziLocatedEntry>,
    meta_xml: String,
    series: Vec<CziSeries>,
    current_series: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CziReaderSnapshot {
    pub path: PathBuf,
    pub used_files: Vec<PathBuf>,
    pub entries: Vec<CziLocatedEntry>,
    pub meta_xml: String,
    pub series: Vec<CziSeries>,
    pub current_series: usize,
}

impl CziReader {
    pub fn new() -> Self {
        Self {
            path: None,
            used_files: Vec::new(),
            entries: Vec::new(),
            meta_xml: String::new(),
            series: Vec::new(),
            current_series: 0,
        }
    }

    pub fn from_snapshot(snapshot: CziReaderSnapshot) -> Result<Self> {
        Ok(Self {
            path: Some(snapshot.path),
            used_files: snapshot.used_files,
            entries: snapshot.entries,
            meta_xml: snapshot.meta_xml,
            series: snapshot.series,
            current_series: snapshot.current_series,
        })
    }

    fn current_series(&self) -> Result<&CziSeries> {
        self.series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)
    }

    fn read_plane(&self, plane: &CziPlaneRef, series: &CziSeries) -> Result<Vec<u8>> {
        let located = self
            .entries
            .get(plane.entry_index)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane.entry_index as u32))?;
        let path = self
            .used_files
            .get(located.file_index)
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut file = File::open(path).map_err(BioFormatsError::Io)?;

        file.seek(SeekFrom::Start(located.entry.file_position as u64))
            .map_err(BioFormatsError::Io)?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        file.read_exact(&mut seg_hdr).map_err(BioFormatsError::Io)?;
        let mut subblock_hdr = vec![0u8; 16];
        file.read_exact(&mut subblock_hdr)
            .map_err(BioFormatsError::Io)?;
        let metadata_size = read_i32(&subblock_hdr, 0) as i64;
        let attach_size = read_i32(&subblock_hdr, 4) as i64;
        let data_size = read_u64(&subblock_hdr, 8) as usize;

        file.seek(SeekFrom::Current(256 + metadata_size + attach_size))
            .map_err(BioFormatsError::Io)?;

        let mut compressed = vec![0u8; data_size];
        file.read_exact(&mut compressed)
            .map_err(BioFormatsError::Io)?;
        let mut raw = decompress_subblock(&compressed, located.entry.compression)?;

        let expected = series.metadata.size_x as usize
            * series.metadata.size_y as usize
            * series.samples_per_pixel as usize
            * series.metadata.pixel_type.bytes_per_sample();
        raw.truncate(expected);
        raw.resize(expected, 0);
        if series.bgr_order {
            bgr_to_rgb_in_place(
                &mut raw,
                series.samples_per_pixel,
                series.metadata.pixel_type.bytes_per_sample(),
            );
        }
        Ok(raw)
    }
}

impl Default for CziReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for CziReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("czi"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"ZISRAWFILE")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let used_files = discover_czi_files(path);
        let mut entries = Vec::new();
        let mut meta_xml = String::new();

        for (file_index, file_path) in used_files.iter().enumerate() {
            let file = File::open(file_path).map_err(BioFormatsError::Io)?;
            let mut reader = BufReader::new(file);
            let parsed = parse_czi_file(&mut reader).map_err(BioFormatsError::Io)?;
            if meta_xml.is_empty() && !parsed.meta_xml.trim().is_empty() {
                meta_xml = parsed.meta_xml.clone();
            }
            entries.extend(
                parsed
                    .entries
                    .into_iter()
                    .map(|entry| CziLocatedEntry { file_index, entry }),
            );
        }

        let series = build_czi_series(&entries, &meta_xml, &used_files)?;
        self.path = Some(used_files[0].clone());
        self.used_files = used_files;
        self.entries = entries;
        self.meta_xml = meta_xml;
        self.series = series;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.used_files.clear();
        self.entries.clear();
        self.meta_xml.clear();
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

    fn used_files(&self) -> Vec<PathBuf> {
        self.used_files.clone()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self.current_series()?;
        if plane_index >= series.metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane = &series.planes[plane_index as usize];
        self.read_plane(plane, series)
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
        let samples = series.samples_per_pixel as usize;
        let bytes_per_sample = series.metadata.pixel_type.bytes_per_sample();
        let row_bytes = series.metadata.size_x as usize * samples * bytes_per_sample;
        let out_row = w as usize * samples * bytes_per_sample;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let start = x as usize * samples * bytes_per_sample;
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
        Ok(ReaderSnapshot::CziReader(CziReaderSnapshot {
            path: self.path.clone().ok_or(BioFormatsError::NotInitialized)?,
            used_files: self.used_files.clone(),
            entries: self.entries.clone(),
            meta_xml: self.meta_xml.clone(),
            series: self.series.clone(),
            current_series: self.current_series,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("bioformats_rs_{name}_{nanos}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn entry(pixel_type: i32, dims: &[(&str, (i32, i32))]) -> CziLocatedEntry {
        CziLocatedEntry {
            file_index: 0,
            entry: DirEntry {
                pixel_type,
                file_position: 0,
                compression: 0,
                dims: dims
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), *value))
                    .collect(),
            },
        }
    }

    #[test]
    fn swaps_bgr_to_rgb() {
        let mut pixels = vec![1u8, 2, 3, 10, 20, 30];
        bgr_to_rgb_in_place(&mut pixels, 3, 1);
        assert_eq!(pixels, vec![3, 2, 1, 30, 20, 10]);
    }

    #[test]
    fn discovers_master_and_parts() {
        let dir = TempDir::new("czi_parts");
        let master = dir.path.join("sample.czi");
        let part2 = dir.path.join("sample (2).czi");
        let part1 = dir.path.join("sample (1).czi");
        fs::write(&master, []).unwrap();
        fs::write(&part2, []).unwrap();
        fs::write(&part1, []).unwrap();

        let files = discover_czi_files(&part1);
        assert_eq!(files, vec![master, part1, part2]);
    }

    #[test]
    fn builds_series_with_logical_channels() {
        let xml = r#"
<ImageDocument>
  <Metadata>
    <Scaling>
      <Items>
        <Distance Id="X"><Value>0.0000005</Value></Distance>
        <Distance Id="Y"><Value>0.0000006</Value></Distance>
      </Items>
    </Scaling>
    <Information>
      <Image>
        <Dimensions>
          <T><Positions><Interval><Increment>2.5</Increment></Interval></Positions></T>
          <S>
            <Scenes>
              <Scene><CenterPosition>1.0,2.0,3.0</CenterPosition></Scene>
              <Scene><CenterPosition>4.0,5.0,6.0</CenterPosition></Scene>
            </Scenes>
          </S>
          <Channels>
            <Channel Name="GFP"><EmissionWavelength>520</EmissionWavelength></Channel>
            <Channel Name="RFP"><EmissionWavelength>610</EmissionWavelength></Channel>
          </Channels>
        </Dimensions>
      </Image>
      <Instrument>
        <Objectives>
          <Objective Model="Plan-Apo"><LensNA>1.4</LensNA></Objective>
        </Objectives>
      </Instrument>
    </Information>
  </Metadata>
</ImageDocument>
"#;
        let entries = vec![
            entry(
                3,
                &[
                    ("S", (0, 1)),
                    ("C", (0, 1)),
                    ("Z", (0, 1)),
                    ("T", (0, 1)),
                    ("X", (0, 8)),
                    ("Y", (0, 6)),
                ],
            ),
            entry(
                3,
                &[
                    ("S", (0, 1)),
                    ("C", (1, 1)),
                    ("Z", (0, 1)),
                    ("T", (0, 1)),
                    ("X", (0, 8)),
                    ("Y", (0, 6)),
                ],
            ),
            entry(
                3,
                &[
                    ("S", (1, 1)),
                    ("C", (0, 1)),
                    ("Z", (0, 1)),
                    ("T", (0, 1)),
                    ("X", (0, 8)),
                    ("Y", (0, 6)),
                ],
            ),
            entry(
                3,
                &[
                    ("S", (1, 1)),
                    ("C", (1, 1)),
                    ("Z", (0, 1)),
                    ("T", (0, 1)),
                    ("X", (0, 8)),
                    ("Y", (0, 6)),
                ],
            ),
        ];

        let series = build_czi_series(&entries, xml, &[PathBuf::from("sample.czi")]).unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].metadata.size_c, 6);
        assert_eq!(series[0].metadata.logical_channel_count(), 2);
        assert_eq!(series[0].metadata.image_count, 2);
        assert!(series[0].metadata.is_rgb);
        assert_eq!(
            series[0].metadata.channel_metadata[0].name.as_deref(),
            Some("GFP")
        );
        assert_eq!(series[0].metadata.physical_size_x_um, Some(0.5));
        assert_eq!(series[0].metadata.physical_size_y_um, Some(0.6));
        assert_eq!(series[0].metadata.time_increment_seconds, Some(2.5));
        assert_eq!(
            series[0].metadata.objective_model.as_deref(),
            Some("Plan-Apo")
        );
        assert_eq!(series[0].metadata.objective_na, Some(1.4));
        assert_eq!(
            series[0].metadata.plane_metadata[0].position_x_um,
            Some(1.0)
        );
        assert_eq!(
            series[1].metadata.plane_metadata[0].position_x_um,
            Some(4.0)
        );
    }
}
