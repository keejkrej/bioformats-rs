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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::fs::{self, File};
#[cfg(test)]
use std::io::BufReader;

use crate::common::codec::{decompress_lzw_limited, decompress_zstd_limited};
use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{
    ChannelMetadata, DimensionOrder, ImageMetadata, MetadataValue, PlaneMetadata,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{validate_region, FormatReader};
use crate::snapshot::ReaderSnapshot;
use crate::source::{SourceHandle, SourceInfo, SourceInput};
use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CziPixelInfo {
    pixel_type: PixelType,
    samples_per_pixel: u32,
    rgb: bool,
    bgr_order: bool,
}

fn czi_pixel_info(code: i32) -> Result<CziPixelInfo> {
    let info = match code {
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
        10 | 11 => {
            return Err(BioFormatsError::UnsupportedFormat(
                "CZI: complex pixel data is not supported".into(),
            ));
        }
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
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "CZI: unknown pixel type {code}"
            )));
        }
    };
    Ok(info)
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
    #[serde(default)]
    full_resolution: bool,
    dims: HashMap<String, (i32, i32)>,
}

fn invalid_czi(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn try_zeroed_czi(length: usize, context: &str) -> std::io::Result<Vec<u8>> {
    if length > isize::MAX as usize {
        return Err(invalid_czi(format!("CZI {context} does not fit in memory")));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|error| {
        invalid_czi(format!(
            "cannot allocate {length}-byte CZI {context}: {error}"
        ))
    })?;
    bytes.resize(length, 0);
    Ok(bytes)
}

fn segment_used_size(header: &[u8]) -> std::io::Result<u64> {
    if header.len() < SEG_HEADER {
        return Err(invalid_czi("truncated CZI segment header"));
    }
    let allocated = read_u64(header, 16);
    let used = read_u64(header, 24);
    Ok(if used == 0 { allocated } else { used })
}

fn validate_segment_range(
    segment_position: u64,
    used_size: u64,
    file_len: u64,
    context: &str,
) -> std::io::Result<()> {
    let segment_end = segment_position
        .checked_add(SEG_HEADER as u64)
        .and_then(|position| position.checked_add(used_size))
        .ok_or_else(|| invalid_czi(format!("CZI {context} segment range overflows")))?;
    if segment_end > file_len {
        return Err(invalid_czi(format!(
            "CZI {context} segment ends at {segment_end}, beyond file length {file_len}"
        )));
    }
    Ok(())
}

fn parse_dir_entry(data: &[u8]) -> std::io::Result<DirEntry> {
    if data.len() < 32 {
        return Err(invalid_czi("truncated CZI directory entry"));
    }
    let pixel_type = read_i32(data, 2);
    let file_position = read_i64(data, 6);
    let compression = read_i32(data, 18);
    let pyramid_type = data[22];
    let dim_count = usize::try_from(read_i32(data, 28))
        .map_err(|_| invalid_czi("negative CZI directory dimension count"))?;
    let expected = dim_count
        .checked_mul(20)
        .and_then(|bytes| bytes.checked_add(32))
        .ok_or_else(|| invalid_czi("CZI directory entry size overflow"))?;
    if expected > data.len() {
        return Err(invalid_czi("truncated CZI directory dimensions"));
    }

    let mut dims = HashMap::new();
    dims.try_reserve(dim_count)
        .map_err(|error| invalid_czi(format!("cannot allocate CZI dimensions: {error}")))?;
    let dim_array_start = 32;
    let mut full_resolution = pyramid_type == 0;
    for i in 0..dim_count {
        let off = dim_array_start + i * 20;
        let dim_name = std::str::from_utf8(&data[off..off + 4])
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let start = read_i32(data, off + 4);
        let size = read_i32(data, off + 8);
        let stored_size = read_i32(data, off + 16);
        if !dim_name.is_empty() {
            if size <= 0 {
                return Err(invalid_czi(format!(
                    "CZI {dim_name} dimension has non-positive size {size}"
                )));
            }
            if matches!(dim_name.as_str(), "Z" | "C" | "T")
                && (start < 0 || start.checked_add(size).is_none())
            {
                return Err(invalid_czi(format!(
                    "CZI {dim_name} dimension has invalid start {start} and size {size}"
                )));
            }
            if matches!(dim_name.as_str(), "X" | "Y") && stored_size != size {
                full_resolution = false;
            }
            dims.insert(dim_name, (start, size));
        }
    }

    Ok(DirEntry {
        pixel_type,
        file_position,
        compression,
        full_resolution,
        dims,
    })
}

struct CziParsedFile {
    meta_xml: String,
    entries: Vec<DirEntry>,
}

fn parse_czi_file<R: Read + Seek>(f: &mut R, file_len: u64) -> std::io::Result<CziParsedFile> {
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
    // FileHeader payload: versions/reserved (16), two 16-byte GUIDs (32),
    // file part (4), then the directory and metadata positions.
    let dir_position = read_u64(&fh, 52);
    let meta_position = read_u64(&fh, 60);

    let mut meta_xml = String::new();
    if meta_position > 0 {
        f.seek(SeekFrom::Start(meta_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        if !read_seg_type(&seg_hdr).starts_with("ZISRAWMETADATA") {
            return Err(invalid_czi(
                "CZI metadata pointer does not reference metadata",
            ));
        }
        let metadata_used_size = segment_used_size(&seg_hdr)?;
        validate_segment_range(meta_position, metadata_used_size, file_len, "metadata")?;
        let mut meta_body_hdr = vec![0u8; 256];
        f.read_exact(&mut meta_body_hdr)?;
        let xml_size = usize::try_from(read_i32(&meta_body_hdr, 0))
            .map_err(|_| invalid_czi("negative CZI metadata XML size"))?;
        let metadata_payload = 256_u64
            .checked_add(
                u64::try_from(xml_size).map_err(|_| invalid_czi("CZI XML size overflows u64"))?,
            )
            .ok_or_else(|| invalid_czi("CZI metadata payload size overflow"))?;
        if metadata_payload > metadata_used_size {
            return Err(invalid_czi("CZI XML exceeds its metadata segment"));
        }
        if xml_size > 0 {
            let mut xml_bytes = try_zeroed_czi(xml_size, "metadata XML")?;
            f.read_exact(&mut xml_bytes)?;
            meta_xml = String::from_utf8(xml_bytes)
                .map_err(|_| invalid_czi("CZI metadata XML is not valid UTF-8"))?;
        }
    }

    let mut entries = Vec::new();
    if dir_position > 0 {
        f.seek(SeekFrom::Start(dir_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        if !read_seg_type(&seg_hdr).starts_with("ZISRAWDIRECTORY") {
            return Err(invalid_czi(
                "CZI directory pointer does not reference a directory",
            ));
        }
        let used_size = segment_used_size(&seg_hdr)?;
        validate_segment_range(dir_position, used_size, file_len, "directory")?;
        let mut dir_hdr = vec![0u8; 128];
        f.read_exact(&mut dir_hdr)?;
        let entry_count = usize::try_from(read_i32(&dir_hdr, 0))
            .map_err(|_| invalid_czi("negative CZI directory entry count"))?;
        let mut remaining = used_size
            .checked_sub(128)
            .ok_or_else(|| invalid_czi("CZI directory payload is shorter than its header"))?;
        let maximum_entries = remaining / 32;
        if u64::try_from(entry_count).unwrap_or(u64::MAX) > maximum_entries {
            return Err(invalid_czi(
                "CZI directory entry count exceeds its segment size",
            ));
        }
        entries.try_reserve_exact(entry_count).map_err(|error| {
            invalid_czi(format!("cannot allocate CZI directory entries: {error}"))
        })?;
        for _ in 0..entry_count {
            if remaining < 32 {
                return Err(invalid_czi("truncated CZI directory entry list"));
            }
            let mut entry_buf = vec![0u8; 32];
            f.read_exact(&mut entry_buf)?;
            let dim_count = usize::try_from(read_i32(&entry_buf, 28))
                .map_err(|_| invalid_czi("negative CZI directory dimension count"))?;
            let dimension_bytes = dim_count
                .checked_mul(20)
                .ok_or_else(|| invalid_czi("CZI directory dimensions overflow"))?;
            let entry_size = u64::try_from(dimension_bytes)
                .ok()
                .and_then(|bytes| bytes.checked_add(32))
                .ok_or_else(|| invalid_czi("CZI directory entry size overflow"))?;
            if entry_size > remaining {
                return Err(invalid_czi("CZI directory entry exceeds segment size"));
            }
            entry_buf
                .try_reserve_exact(dimension_bytes)
                .map_err(|error| {
                    invalid_czi(format!("cannot allocate CZI directory dimensions: {error}"))
                })?;
            entry_buf.resize(32 + dimension_bytes, 0);
            f.read_exact(&mut entry_buf[32..])?;
            let entry = parse_dir_entry(&entry_buf)?;
            if entry.full_resolution {
                entries.push(entry);
            }
            remaining -= entry_size;
        }
    }

    Ok(CziParsedFile { meta_xml, entries })
}

fn decompress_subblock(data: &[u8], compression: i32, expected_len: usize) -> Result<Vec<u8>> {
    match compression {
        0 => {
            if data.len() != expected_len {
                return Err(BioFormatsError::InvalidData(format!(
                    "CZI raw subblock has {} bytes; expected {expected_len}",
                    data.len()
                )));
            }
            let mut output = Vec::new();
            output.try_reserve_exact(expected_len).map_err(|error| {
                BioFormatsError::InvalidData(format!(
                    "CZI raw output of {expected_len} bytes cannot be allocated: {error}"
                ))
            })?;
            output.extend_from_slice(data);
            Ok(output)
        }
        1 => {
            let mut dec = jpeg_decoder::Decoder::new(data);
            dec.set_max_decoding_buffer_size(expected_len);
            dec.decode()
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        2 => decompress_lzw_limited(data, expected_len),
        4 => Err(BioFormatsError::UnsupportedFormat(
            "CZI: JPEG-XR compression not yet supported".into(),
        )),
        5 => decompress_zstd_limited(data, expected_len),
        6 => {
            let (payload_offset, high_low_unpacking) = parse_zstd1_header(data)?;
            let decoded = decompress_zstd_limited(&data[payload_offset..], expected_len)?;
            if !high_low_unpacking {
                return Ok(decoded);
            }
            if !decoded.len().is_multiple_of(2) {
                return Err(BioFormatsError::InvalidData(
                    "CZI ZSTD-1 high/low-byte output has an odd length".into(),
                ));
            }
            let mut unpacked = Vec::new();
            unpacked.try_reserve_exact(decoded.len()).map_err(|error| {
                BioFormatsError::InvalidData(format!(
                    "cannot allocate CZI ZSTD-1 unpacking buffer: {error}"
                ))
            })?;
            let second_half = decoded.len() / 2;
            for offset in 0..second_half {
                unpacked.push(decoded[offset]);
                unpacked.push(decoded[second_half + offset]);
            }
            Ok(unpacked)
        }
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "CZI: unknown compression {}",
            compression
        ))),
    }
}

fn parse_zstd1_header(data: &[u8]) -> Result<(usize, bool)> {
    let mut position = 0_usize;
    let header_size = read_zstd1_varint(data, &mut position)?;
    if position > header_size || header_size > data.len() {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD-1 header exceeds its compressed block".into(),
        ));
    }

    let mut high_low_unpacking = false;
    while position < header_size {
        let chunk_id = read_zstd1_varint(data, &mut position)?;
        match chunk_id {
            1 => {
                if position >= header_size {
                    return Err(BioFormatsError::InvalidData(
                        "CZI ZSTD-1 byte-order chunk is truncated".into(),
                    ));
                }
                high_low_unpacking = data[position] & 1 == 1;
                position += 1;
            }
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "CZI ZSTD-1 header chunk {chunk_id} is not supported"
                )))
            }
        }
    }
    Ok((position, high_low_unpacking))
}

fn read_zstd1_varint(data: &[u8], position: &mut usize) -> Result<usize> {
    let first = *data
        .get(*position)
        .ok_or_else(|| BioFormatsError::InvalidData("truncated CZI ZSTD-1 varint".into()))?;
    *position += 1;
    if first & 0x80 == 0 {
        return Ok(usize::from(first));
    }

    let second = *data
        .get(*position)
        .ok_or_else(|| BioFormatsError::InvalidData("truncated CZI ZSTD-1 varint".into()))?;
    *position += 1;
    if second & 0x80 == 0 {
        return Ok((usize::from(second) << 7) | usize::from(first & 0x7f));
    }

    let third = *data
        .get(*position)
        .ok_or_else(|| BioFormatsError::InvalidData("truncated CZI ZSTD-1 varint".into()))?;
    *position += 1;
    Ok((usize::from(third) << 14) | (usize::from(second & 0x7f) << 7) | usize::from(first & 0x7f))
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
    bits_per_pixel: Option<u8>,
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

fn parse_czi_metadata(xml: &str) -> Result<CziMetadataModel> {
    if xml.trim().is_empty() {
        return Ok(CziMetadataModel::default());
    }
    let document = Document::parse(xml).map_err(|error| {
        BioFormatsError::Format(format!("CZI metadata XML is malformed: {error}"))
    })?;

    let mut metadata = CziMetadataModel::default();

    metadata.bits_per_pixel = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "ComponentBitCount")
        .and_then(|node| node.text())
        .and_then(|value| value.trim().parse::<u8>().ok())
        .filter(|value| *value > 0);

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

    Ok(metadata)
}

fn split_czi_part_stem(stem: &str) -> Option<(&str, usize)> {
    let without_close = stem.strip_suffix(')')?;
    let open = without_close.rfind('(')?;
    let index = without_close[open + 1..].parse::<usize>().ok()?;
    let base = without_close[..open]
        .strip_suffix(' ')
        .unwrap_or(&without_close[..open]);
    (!base.is_empty()).then_some((base, index))
}

fn file_stem_without_part(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(
        split_czi_part_stem(stem)
            .map(|(base, _)| base)
            .unwrap_or(stem)
            .to_string(),
    )
}

fn czi_part_index(path: &Path) -> usize {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(split_czi_part_stem)
        .map(|(_, index)| index)
        .unwrap_or(0)
}

#[cfg(test)]
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

fn is_czi_member_named(name: &str, base: &str) -> bool {
    let path = Path::new(name);
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("czi"))
        && file_stem_without_part(path).is_some_and(|candidate_base| candidate_base == base)
}

fn is_czi_master_named(name: &str, base: &str) -> bool {
    let path = Path::new(name);
    is_czi_member_named(name, base)
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem == base)
}

fn dim_start(entry: &DirEntry, key: &str) -> i32 {
    entry.dims.get(key).map(|(start, _)| *start).unwrap_or(0)
}

fn dim_extent(entry: &DirEntry, key: &str) -> Result<u32> {
    let Some((start, size)) = entry.dims.get(key) else {
        return Ok(1);
    };
    if *start < 0 || *size <= 0 {
        return Err(BioFormatsError::InvalidData(format!(
            "CZI {key} dimension has invalid start {start} or size {size}"
        )));
    }
    let end = start.checked_add(*size).ok_or_else(|| {
        BioFormatsError::InvalidData(format!("CZI {key} dimension extent overflows i32"))
    })?;
    u32::try_from(end)
        .map_err(|_| BioFormatsError::InvalidData(format!("CZI {key} dimension extent is invalid")))
}

fn dim_size(entry: &DirEntry, key: &str) -> Result<u32> {
    let Some((_, size)) = entry.dims.get(key) else {
        return Ok(1);
    };
    u32::try_from(*size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| {
            BioFormatsError::InvalidData(format!("CZI {key} dimension has invalid size {size}"))
        })
}

fn plane_priority(entry: &DirEntry) -> Result<(u64, bool)> {
    let area = u64::from(dim_size(entry, "X")?)
        .checked_mul(u64::from(dim_size(entry, "Y")?))
        .ok_or_else(|| BioFormatsError::InvalidData("CZI plane area overflows u64".into()))?;
    let origin = dim_start(entry, "X") == 0 && dim_start(entry, "Y") == 0;
    Ok((area, origin))
}

fn max_group_extent(entries: &[CziLocatedEntry], group: &[usize], key: &str) -> Result<u32> {
    group.iter().try_fold(1_u32, |maximum, index| {
        Ok(maximum.max(dim_extent(&entries[*index].entry, key)?))
    })
}

fn max_group_size(entries: &[CziLocatedEntry], group: &[usize], key: &str) -> Result<u32> {
    group.iter().try_fold(0_u32, |maximum, index| {
        Ok(maximum.max(dim_size(&entries[*index].entry, key)?))
    })
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
    for located in entries {
        for axis in ["R", "I", "H"] {
            if let Some((start, size)) = located.entry.dims.get(axis) {
                if *start != 0 || *size != 1 {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "CZI: non-singleton {axis} dimensions are not yet folded into Z/C/T"
                    )));
                }
            }
        }
    }

    let xml = parse_czi_metadata(metadata_xml)?;
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
    series.try_reserve_exact(grouped.len()).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate CZI series metadata: {error}"))
    })?;
    for ((scene_index, _, _, _), group) in grouped {
        let mut layout_entry_index = group[0];
        for index in group.iter().copied().skip(1) {
            if plane_priority(&entries[index].entry)?
                > plane_priority(&entries[layout_entry_index].entry)?
            {
                layout_entry_index = index;
            }
        }
        let layout_pixel_type = entries[layout_entry_index].entry.pixel_type;
        let pixel = czi_pixel_info(layout_pixel_type)?;
        let logical_channels = max_group_extent(entries, &group, "C")?;
        let size_z = max_group_extent(entries, &group, "Z")?;
        let size_t = max_group_extent(entries, &group, "T")?;
        let size_x = max_group_size(entries, &group, "X")?;
        let size_y = max_group_size(entries, &group, "Y")?;
        let image_count = logical_channels
            .checked_mul(size_z)
            .and_then(|count| count.checked_mul(size_t))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let size_c = logical_channels
            .checked_mul(if pixel.rgb {
                pixel.samples_per_pixel
            } else {
                1
            })
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;

        let mut metadata = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type: pixel.pixel_type,
            bits_per_pixel: xml
                .bits_per_pixel
                .unwrap_or((pixel.pixel_type.bytes_per_sample() * 8) as u8),
            samples_per_pixel: pixel.samples_per_pixel,
            image_count,
            dimension_order: DimensionOrder::XYCZT,
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
            dimension_order: DimensionOrder::XYCZT,
            ..ImageMetadata::default()
        };
        let image_count_usize =
            usize::try_from(image_count).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        let mut planes: Vec<Option<usize>> = Vec::new();
        planes
            .try_reserve_exact(image_count_usize)
            .map_err(|error| {
                BioFormatsError::InvalidData(format!("cannot allocate CZI plane map: {error}"))
            })?;
        planes.resize(image_count_usize, None);
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
                    if plane_priority(entry)? > plane_priority(current_entry)? {
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

        metadata
            .plane_metadata
            .try_reserve_exact(image_count_usize)
            .map_err(|error| {
                BioFormatsError::InvalidData(format!("cannot allocate CZI plane metadata: {error}"))
            })?;
        for plane_index in 0..image_count {
            let (z, c, t) = temp.get_zct_coords(plane_index);
            metadata.plane_metadata.push(PlaneMetadata {
                z,
                c,
                t,
                delta_t_seconds: metadata.time_increment_seconds.map(|step| step * t as f64),
                position_x_um: scene_position.0,
                position_y_um: scene_position.1,
                position_z_um: scene_position
                    .2
                    .or_else(|| metadata.physical_size_z_um.map(|step| step * z as f64)),
            });
        }

        let mut mapped_planes = Vec::new();
        mapped_planes
            .try_reserve_exact(image_count_usize)
            .map_err(|error| {
                BioFormatsError::InvalidData(format!("cannot allocate CZI plane list: {error}"))
            })?;
        for (plane_index, entry_index) in planes.into_iter().enumerate() {
            let entry_index = entry_index.ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "CZI series plane {plane_index} could not be mapped"
                ))
            })?;
            let entry_pixel_type = entries[entry_index].entry.pixel_type;
            if entry_pixel_type != layout_pixel_type {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "CZI series contains heterogeneous selected pixel types ({layout_pixel_type} and {entry_pixel_type}); splitting mixed pixel types is not yet supported"
                )));
            }
            mapped_planes.push(CziPlaneRef { entry_index });
        }

        series.push(CziSeries {
            metadata,
            planes: mapped_planes,
            samples_per_pixel: pixel.samples_per_pixel,
            bgr_order: pixel.bgr_order,
        });
    }

    Ok(series)
}

pub struct CziReader {
    path: Option<PathBuf>,
    used_files: Vec<PathBuf>,
    sources: Vec<SourceHandle>,
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
            sources: Vec::new(),
            entries: Vec::new(),
            meta_xml: String::new(),
            series: Vec::new(),
            current_series: 0,
        }
    }

    pub fn from_snapshot(snapshot: CziReaderSnapshot) -> Result<Self> {
        let sources = snapshot
            .used_files
            .iter()
            .map(|path| SourceInput::from_path(path)?.primary_handle())
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            path: Some(snapshot.path),
            used_files: snapshot.used_files,
            sources,
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
        let source = self
            .sources
            .get(located.file_index)
            .ok_or(BioFormatsError::NotInitialized)?;
        let file_len = source.info().len();
        let mut reader = source.cursor();

        let segment_position = u64::try_from(located.entry.file_position)
            .map_err(|_| BioFormatsError::InvalidData("negative CZI subblock position".into()))?;
        reader
            .seek(SeekFrom::Start(segment_position))
            .map_err(BioFormatsError::from)?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        reader
            .read_exact(&mut seg_hdr)
            .map_err(BioFormatsError::from)?;
        if !read_seg_type(&seg_hdr).starts_with("ZISRAWSUBBLOCK") {
            return Err(BioFormatsError::InvalidData(
                "CZI directory entry does not reference a subblock".into(),
            ));
        }
        let used_size = segment_used_size(&seg_hdr).map_err(BioFormatsError::from)?;
        validate_segment_range(segment_position, used_size, file_len, "subblock")
            .map_err(BioFormatsError::from)?;
        let mut subblock_hdr = vec![0u8; 16];
        reader
            .read_exact(&mut subblock_hdr)
            .map_err(BioFormatsError::from)?;
        let metadata_size = u64::try_from(read_i32(&subblock_hdr, 0)).map_err(|_| {
            BioFormatsError::InvalidData("negative CZI subblock metadata size".into())
        })?;
        let attachment_size = u64::try_from(read_i32(&subblock_hdr, 4)).map_err(|_| {
            BioFormatsError::InvalidData("negative CZI subblock attachment size".into())
        })?;
        let data_size_u64 = read_u64(&subblock_hdr, 8);
        let payload_size = 256u64
            .checked_add(metadata_size)
            .and_then(|size| size.checked_add(data_size_u64))
            .and_then(|size| size.checked_add(attachment_size))
            .ok_or_else(|| BioFormatsError::InvalidData("CZI subblock size overflow".into()))?;
        if payload_size > used_size {
            return Err(BioFormatsError::InvalidData(
                "CZI subblock contents exceed its segment size".into(),
            ));
        }
        let data_position = segment_position
            .checked_add(SEG_HEADER as u64)
            .and_then(|position| position.checked_add(256))
            .and_then(|position| position.checked_add(metadata_size))
            .ok_or_else(|| BioFormatsError::InvalidData("CZI data offset overflow".into()))?;
        let data_size = usize::try_from(data_size_u64)
            .map_err(|_| BioFormatsError::InvalidData("CZI pixel block is too large".into()))?;
        let data_end = data_position
            .checked_add(data_size_u64)
            .ok_or_else(|| BioFormatsError::InvalidData("CZI data range overflow".into()))?;
        if data_end > file_len {
            return Err(BioFormatsError::InvalidData(format!(
                "CZI pixel block ends at {data_end}, beyond file length {file_len}"
            )));
        }
        let expected = usize::try_from(series.metadata.size_x)
            .ok()
            .and_then(|width| {
                usize::try_from(series.metadata.size_y)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(series.samples_per_pixel as usize))
            .and_then(|samples| samples.checked_mul(series.metadata.pixel_type.bytes_per_sample()))
            .filter(|length| *length <= isize::MAX as usize)
            .ok_or_else(|| BioFormatsError::InvalidData("CZI plane size overflow".into()))?;
        if located.entry.compression == 0 && data_size != expected {
            return Err(BioFormatsError::InvalidData(format!(
                "CZI raw pixel block has {data_size} bytes; expected {expected}"
            )));
        }

        reader
            .seek(SeekFrom::Start(data_position))
            .map_err(BioFormatsError::from)?;

        let mut compressed = Vec::new();
        compressed.try_reserve_exact(data_size).map_err(|_| {
            BioFormatsError::InvalidData(format!(
                "CZI pixel block of {data_size} bytes cannot be allocated"
            ))
        })?;
        compressed.resize(data_size, 0);
        reader
            .read_exact(&mut compressed)
            .map_err(BioFormatsError::from)?;
        let mut raw = decompress_subblock(&compressed, located.entry.compression, expected)?;
        if raw.len() < expected {
            return Err(BioFormatsError::InvalidData(format!(
                "CZI subblock decoded to {} bytes; expected at least {expected}",
                raw.len()
            )));
        }
        raw.truncate(expected);
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
        self.set_source(SourceInput::from_path(path)?)
    }

    fn set_source(&mut self, input: SourceInput) -> Result<()> {
        let primary = input.primary_handle()?;
        let primary_identity = primary.info().identity().clone();
        let base = file_stem_without_part(Path::new(primary.info().name()));
        let mut sources = vec![primary.clone()];
        if let Some(base) = base.as_deref() {
            sources.extend(
                input.resolve_siblings_where(&primary, |name| is_czi_member_named(name, base))?,
            );
        }

        let mut identities = HashSet::new();
        sources.retain(|source| identities.insert(source.info().identity().clone()));
        if let Some(base) = base.as_deref() {
            sources.retain(|source| {
                source.info().identity() == &primary_identity
                    || is_czi_member_named(source.info().name(), base)
            });

            let first_identity = sources
                .iter()
                .find(|source| is_czi_master_named(source.info().name(), base))
                .map(|source| source.info().identity().clone())
                .unwrap_or_else(|| primary_identity.clone());
            sources.sort_by(|left, right| {
                let left_first = left.info().identity() == &first_identity;
                let right_first = right.info().identity() == &first_identity;
                right_first
                    .cmp(&left_first)
                    .then_with(|| {
                        czi_part_index(Path::new(left.info().name()))
                            .cmp(&czi_part_index(Path::new(right.info().name())))
                    })
                    .then_with(|| left.info().name().cmp(right.info().name()))
                    .then_with(|| left.info().identity().cmp(right.info().identity()))
            });
        }

        let used_files = sources
            .iter()
            .filter_map(|source| source.path().map(Path::to_path_buf))
            .collect::<Vec<_>>();
        let mut entries = Vec::new();
        let mut meta_xml = String::new();

        for (file_index, source) in sources.iter().enumerate() {
            let mut reader = source.cursor();
            let parsed =
                parse_czi_file(&mut reader, source.info().len()).map_err(BioFormatsError::from)?;
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
        self.path = sources
            .first()
            .and_then(SourceHandle::path)
            .map(Path::to_path_buf);
        self.used_files = used_files;
        self.sources = sources;
        self.entries = entries;
        self.meta_xml = meta_xml;
        self.series = series;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.used_files.clear();
        self.sources.clear();
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

    fn used_sources(&self) -> Vec<SourceInfo> {
        self.sources
            .iter()
            .map(|source| source.info().clone())
            .collect()
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
        validate_region(&self.current_series()?.metadata, x, y, w, h)?;
        let full = self.open_bytes(plane_index)?;
        let series = self.current_series()?;
        let samples = usize::try_from(series.samples_per_pixel)
            .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        let bytes_per_sample = series.metadata.pixel_type.bytes_per_sample();
        let row_bytes = usize::try_from(series.metadata.size_x)
            .ok()
            .and_then(|width| width.checked_mul(samples))
            .and_then(|count| count.checked_mul(bytes_per_sample))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let out_row = usize::try_from(w)
            .ok()
            .and_then(|width| width.checked_mul(samples))
            .and_then(|count| count.checked_mul(bytes_per_sample))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let output_length = usize::try_from(h)
            .ok()
            .and_then(|height| height.checked_mul(out_row))
            .filter(|length| *length <= isize::MAX as usize)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let x_start = usize::try_from(x)
            .ok()
            .and_then(|x| x.checked_mul(samples))
            .and_then(|count| count.checked_mul(bytes_per_sample))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let mut out = Vec::new();
        out.try_reserve_exact(output_length).map_err(|error| {
            BioFormatsError::InvalidData(format!(
                "CZI: cannot allocate image region ({output_length} bytes): {error}"
            ))
        })?;
        for row in 0..h as usize {
            let source_row = usize::try_from(y)
                .ok()
                .and_then(|y| y.checked_add(row))
                .and_then(|row| row.checked_mul(row_bytes))
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let start = source_row
                .checked_add(x_start)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let end = start
                .checked_add(out_row)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let source = full.get(start..end).ok_or_else(|| {
                BioFormatsError::InvalidData("CZI region exceeds decoded plane".into())
            })?;
            out.extend_from_slice(source);
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
        if self.sources.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        let path = self.path.clone().ok_or_else(|| {
            BioFormatsError::SnapshotUnsupported(
                "CZI reader initialized from application-provided sources".into(),
            )
        })?;
        Ok(ReaderSnapshot::CziReader(CziReaderSnapshot {
            path,
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
                full_resolution: true,
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
    fn distinguishes_base_blocks_from_czi_pyramid_storage() {
        let mut bytes = vec![0_u8; 52];
        bytes[28..32].copy_from_slice(&1_i32.to_le_bytes());
        bytes[32..36].copy_from_slice(b"X\0\0\0");
        bytes[36..40].copy_from_slice(&0_i32.to_le_bytes());
        bytes[40..44].copy_from_slice(&8_i32.to_le_bytes());
        bytes[48..52].copy_from_slice(&8_i32.to_le_bytes());
        assert!(parse_dir_entry(&bytes).unwrap().full_resolution);

        bytes[48..52].copy_from_slice(&4_i32.to_le_bytes());
        assert!(!parse_dir_entry(&bytes).unwrap().full_resolution);

        bytes[48..52].copy_from_slice(&8_i32.to_le_bytes());
        bytes[22] = 1;
        assert!(!parse_dir_entry(&bytes).unwrap().full_resolution);
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
    fn discovers_parts_without_a_space_before_the_index() {
        let dir = TempDir::new("czi_compact_parts");
        let master = dir.path.join("sample.czi");
        let part = dir.path.join("sample(1).czi");
        fs::write(&master, []).unwrap();
        fs::write(&part, []).unwrap();

        assert_eq!(discover_czi_files(&part), vec![master, part]);
    }

    #[test]
    fn malformed_nonempty_czi_metadata_is_an_error() {
        let entries = vec![entry(
            0,
            &[
                ("C", (0, 1)),
                ("Z", (0, 1)),
                ("T", (0, 1)),
                ("X", (0, 8)),
                ("Y", (0, 6)),
            ],
        )];
        assert!(matches!(
            build_czi_series(&entries, "<ImageDocument>", &[PathBuf::from("bad.czi")]),
            Err(BioFormatsError::Format(message)) if message.contains("malformed")
        ));
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
        assert_eq!(series[0].metadata.samples_per_pixel, 3);
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

    #[test]
    fn rejects_complex_pixels_like_java_bioformats() {
        let entries = vec![entry(
            10,
            &[
                ("C", (0, 1)),
                ("Z", (0, 1)),
                ("T", (0, 1)),
                ("X", (0, 8)),
                ("Y", (0, 6)),
            ],
        )];

        assert!(matches!(
            build_czi_series(&entries, "", &[PathBuf::from("complex.czi")]),
            Err(BioFormatsError::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn rejects_heterogeneous_selected_pixel_types() {
        let entries = vec![
            entry(
                0,
                &[
                    ("C", (0, 1)),
                    ("Z", (0, 1)),
                    ("T", (0, 1)),
                    ("X", (0, 8)),
                    ("Y", (0, 6)),
                ],
            ),
            entry(
                1,
                &[
                    ("C", (1, 1)),
                    ("Z", (0, 1)),
                    ("T", (0, 1)),
                    ("X", (0, 8)),
                    ("Y", (0, 6)),
                ],
            ),
        ];

        assert!(matches!(
            build_czi_series(&entries, "", &[PathBuf::from("mixed.czi")]),
            Err(BioFormatsError::UnsupportedFormat(message))
                if message.contains("heterogeneous selected pixel types")
        ));
    }

    #[test]
    fn rejects_unmodeled_rotation_illumination_and_phase_axes() {
        for axis in ["R", "I", "H"] {
            let entries = vec![entry(
                0,
                &[
                    (axis, (1, 1)),
                    ("C", (0, 1)),
                    ("Z", (0, 1)),
                    ("T", (0, 1)),
                    ("X", (0, 8)),
                    ("Y", (0, 6)),
                ],
            )];
            assert!(matches!(
                build_czi_series(&entries, "", &[PathBuf::from("extra-axis.czi")]),
                Err(BioFormatsError::UnsupportedFormat(message)) if message.contains(axis)
            ));
        }
    }

    #[test]
    fn rejects_overflowing_dimensions_and_bounded_zstd_output() {
        let entries = vec![entry(
            0,
            &[
                ("C", (i32::MAX, 1)),
                ("Z", (0, 1)),
                ("T", (0, 1)),
                ("X", (0, 1)),
                ("Y", (0, 1)),
            ],
        )];
        assert!(matches!(
            build_czi_series(&entries, "", &[PathBuf::from("overflow.czi")]),
            Err(BioFormatsError::InvalidData(_))
        ));

        let encoded = zstd::encode_all(&[9_u8; 1_024][..], 1).unwrap();
        assert!(matches!(
            decompress_subblock(&encoded, 5, 8),
            Err(BioFormatsError::Codec(_))
        ));
    }

    #[test]
    fn decodes_zstd1_header_and_high_low_byte_planes() {
        let separated_bytes = [1_u8, 3, 2, 4];
        let encoded = zstd::encode_all(&separated_bytes[..], 1).unwrap();
        let mut block = vec![3_u8, 1, 1];
        block.extend_from_slice(&encoded);

        assert_eq!(decompress_subblock(&block, 6, 4).unwrap(), [1, 2, 3, 4]);
        assert!(decompress_subblock(&[4, 1, 1], 6, 4).is_err());
        assert!(matches!(
            decompress_subblock(&[1, 2], 0, 1),
            Err(BioFormatsError::InvalidData(message)) if message.contains("raw subblock")
        ));
    }

    #[test]
    fn rejects_segment_size_beyond_file_before_reading_payload() {
        for (name, pointer_offset, segment_type) in [
            ("metadata", 60_usize, b"ZISRAWMETADATA".as_slice()),
            ("directory", 52_usize, b"ZISRAWDIRECTORY".as_slice()),
        ] {
            let dir = TempDir::new(&format!("czi_{name}_range"));
            let path = dir.path.join("truncated.czi");
            let segment_position = (SEG_HEADER + 80) as u64;
            let mut bytes = vec![0_u8; SEG_HEADER + 80 + SEG_HEADER];
            bytes[..10].copy_from_slice(b"ZISRAWFILE");
            bytes[SEG_HEADER + pointer_offset..SEG_HEADER + pointer_offset + 8]
                .copy_from_slice(&segment_position.to_le_bytes());
            let segment_start = segment_position as usize;
            bytes[segment_start..segment_start + segment_type.len()].copy_from_slice(segment_type);
            bytes[segment_start + 16..segment_start + 24]
                .copy_from_slice(&(2_u64 << 30).to_le_bytes());
            bytes[segment_start + 24..segment_start + 32]
                .copy_from_slice(&(2_u64 << 30).to_le_bytes());
            fs::write(&path, bytes).unwrap();

            let file = File::open(path).unwrap();
            let file_len = file.metadata().unwrap().len();
            let error = match parse_czi_file(&mut BufReader::new(file), file_len) {
                Err(error) => error,
                Ok(_) => panic!("out-of-file CZI {name} segment was accepted"),
            };
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("beyond file length"));
        }
    }

    #[test]
    fn rejects_directory_count_before_reserving_entries() {
        let dir = TempDir::new("czi_directory_count");
        let path = dir.path.join("oversized-count.czi");
        let segment_position = (SEG_HEADER + 80) as u64;
        let used_size = 128_u64;
        let mut bytes = vec![0_u8; SEG_HEADER + 80 + SEG_HEADER + used_size as usize];
        bytes[..10].copy_from_slice(b"ZISRAWFILE");
        bytes[SEG_HEADER + 52..SEG_HEADER + 60].copy_from_slice(&segment_position.to_le_bytes());
        let segment_start = segment_position as usize;
        bytes[segment_start..segment_start + 15].copy_from_slice(b"ZISRAWDIRECTORY");
        bytes[segment_start + 16..segment_start + 24].copy_from_slice(&used_size.to_le_bytes());
        bytes[segment_start + 24..segment_start + 32].copy_from_slice(&used_size.to_le_bytes());
        let directory_header = segment_start + SEG_HEADER;
        bytes[directory_header..directory_header + 4].copy_from_slice(&i32::MAX.to_le_bytes());
        fs::write(&path, bytes).unwrap();

        let file = File::open(path).unwrap();
        let file_len = file.metadata().unwrap().len();
        let error = match parse_czi_file(&mut BufReader::new(file), file_len) {
            Err(error) => error,
            Ok(_) => panic!("oversized CZI directory count was accepted"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("entry count exceeds"));
    }
}
