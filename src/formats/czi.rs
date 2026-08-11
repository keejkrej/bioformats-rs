//! Zeiss CZI (ZISRAWFILE) format reader.
//!
//! This reader ports a pragmatic subset of Bio-Formats' dataset modelling:
//! - explicit logical channel vs. RGB sample separation
//! - multi-series grouping across scene/acquisition/angle dimensions
//! - mosaic tile composition and stored-size-aware pyramid resolutions
//! - multi-file dataset discovery
//! - typed metadata extraction from the CZI metadata XML
//!
//! Supported compressions: Uncompressed, JPEG (new-style), LZW, Zstd.
//! JPEG-XR is detected but not decoded.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    dimension_count: u32,
    #[serde(default)]
    full_resolution: bool,
    #[serde(default)]
    stored_size_x: Option<i32>,
    #[serde(default)]
    stored_size_y: Option<i32>,
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
    // The legacy pyramid-type byte has no reliable semantic meaning in the
    // CZI specification. Stored-vs-logical dimensions are authoritative.
    let mut full_resolution = true;
    let mut stored_size_x = None;
    let mut stored_size_y = None;
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
            if matches!(dim_name.as_str(), "Z" | "C" | "T") && start < 0 {
                return Err(invalid_czi(format!(
                    "CZI {dim_name} dimension has invalid start {start} and size {size}"
                )));
            }
            if start.checked_add(size).is_none() {
                return Err(invalid_czi(format!(
                    "CZI {dim_name} dimension start {start} and size {size} overflow"
                )));
            }
            if matches!(dim_name.as_str(), "X" | "Y") {
                if stored_size <= 0 {
                    return Err(invalid_czi(format!(
                        "CZI {dim_name} dimension has non-positive stored size {stored_size}"
                    )));
                }
                if stored_size != size {
                    full_resolution = false;
                }
                if dim_name == "X" {
                    stored_size_x = Some(stored_size);
                } else {
                    stored_size_y = Some(stored_size);
                }
            }
            dims.insert(dim_name, (start, size));
        }
    }

    Ok(DirEntry {
        pixel_type,
        file_position,
        compression,
        dimension_count: u32::try_from(dim_count)
            .map_err(|_| invalid_czi("CZI dimension count exceeds u32"))?,
        full_resolution,
        stored_size_x,
        stored_size_y,
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
            entries.push(entry);
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

fn decompress_czi_jpeg(
    data: &[u8],
    stored_width: u32,
    stored_height: u32,
    samples_per_pixel: u32,
    pixel_type: PixelType,
) -> Result<Vec<u8>> {
    let padded_width = stored_width
        .checked_add(1)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let padded_height = stored_height
        .checked_add(1)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let expected_len = czi_byte_len(stored_width, stored_height, samples_per_pixel, pixel_type)?;
    let padded_len = czi_byte_len(padded_width, padded_height, samples_per_pixel, pixel_type)?;

    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder.set_max_decoding_buffer_size(padded_len);
    let decoded = decoder
        .decode()
        .map_err(|error| BioFormatsError::Codec(error.to_string()))?;
    let info = decoder.info().ok_or_else(|| {
        BioFormatsError::InvalidData("CZI JPEG decoder returned no image dimensions".into())
    })?;
    let decoded_width = u32::from(info.width);
    let decoded_height = u32::from(info.height);
    let pixel_stride = usize::try_from(samples_per_pixel)
        .ok()
        .and_then(|samples| samples.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    if info.pixel_format.pixel_bytes() != pixel_stride {
        return Err(BioFormatsError::InvalidData(format!(
            "CZI JPEG decoded with {} bytes per pixel; expected {pixel_stride}",
            info.pixel_format.pixel_bytes()
        )));
    }

    if decoded_width == stored_width && decoded_height == stored_height {
        if decoded.len() != expected_len {
            return Err(BioFormatsError::InvalidData(format!(
                "CZI JPEG decoded to {} bytes; expected {expected_len}",
                decoded.len()
            )));
        }
        return Ok(decoded);
    }
    if decoded_width != padded_width || decoded_height != padded_height {
        return Err(BioFormatsError::InvalidData(format!(
            "CZI JPEG decoded to {decoded_width}x{decoded_height}; expected {stored_width}x{stored_height} or the vendor-padded {padded_width}x{padded_height} layout"
        )));
    }
    if decoded.len() != padded_len {
        return Err(BioFormatsError::InvalidData(format!(
            "padded CZI JPEG decoded to {} bytes; expected {padded_len}",
            decoded.len()
        )));
    }

    let source_row_bytes = usize::try_from(padded_width)
        .ok()
        .and_then(|width| width.checked_mul(pixel_stride))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let target_row_bytes = usize::try_from(stored_width)
        .ok()
        .and_then(|width| width.checked_mul(pixel_stride))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let mut cropped = Vec::new();
    cropped.try_reserve_exact(expected_len).map_err(|error| {
        BioFormatsError::InvalidData(format!(
            "cannot allocate {expected_len}-byte CZI JPEG tile: {error}"
        ))
    })?;
    for row in
        0..usize::try_from(stored_height).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?
    {
        let source_start = row
            .checked_mul(source_row_bytes)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let source_end = source_start
            .checked_add(target_row_bytes)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        cropped.extend_from_slice(&decoded[source_start..source_end]);
    }
    Ok(cropped)
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
struct CziTileRef {
    entry_index: usize,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy)]
struct CziRegion {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CziPlaneRef {
    tiles: Vec<CziTileRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CziResolutionLevel {
    metadata: ImageMetadata,
    planes: Vec<CziPlaneRef>,
    scale: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CziSeries {
    // Convenient native-resolution summary for low-level metadata consumers.
    // Active reads use `resolutions`.
    metadata: ImageMetadata,
    resolutions: Vec<CziResolutionLevel>,
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

fn max_group_extent(entries: &[CziLocatedEntry], group: &[usize], key: &str) -> Result<u32> {
    group.iter().try_fold(1_u32, |maximum, index| {
        Ok(maximum.max(dim_extent(&entries[*index].entry, key)?))
    })
}

fn stored_dim_size(entry: &DirEntry, key: &str) -> Result<u32> {
    let logical_size = dim_size(entry, key)?;
    let stored_size = match key {
        "X" => entry.stored_size_x,
        "Y" => entry.stored_size_y,
        _ => None,
    };
    let Some(stored_size) = stored_size else {
        return Ok(logical_size);
    };
    u32::try_from(stored_size)
        .ok()
        .filter(|size| *size > 0)
        .ok_or_else(|| {
            BioFormatsError::InvalidData(format!(
                "CZI {key} dimension has invalid stored size {stored_size}"
            ))
        })
}

fn spatial_dimension(entry: &DirEntry, key: &str) -> Result<(i32, u32, u32)> {
    let Some((start, _)) = entry.dims.get(key) else {
        return Err(BioFormatsError::InvalidData(format!(
            "CZI subblock is missing its {key} dimension"
        )));
    };
    Ok((*start, dim_size(entry, key)?, stored_dim_size(entry, key)?))
}

fn rounded_scale(logical: u32, stored: u32, axis: &str) -> Result<u32> {
    if stored == 0 || logical < stored {
        return Err(BioFormatsError::InvalidData(format!(
            "CZI {axis} pyramid scale has invalid logical/stored sizes {logical}/{stored}"
        )));
    }
    let rounded = u64::from(logical)
        .checked_add(u64::from(stored) / 2)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?
        / u64::from(stored);
    u32::try_from(rounded)
        .ok()
        .filter(|scale| *scale > 0)
        .ok_or_else(|| {
            BioFormatsError::InvalidData(format!(
                "CZI {axis} pyramid scale for {logical}/{stored} is invalid"
            ))
        })
}

fn stored_size_matches_scale(logical: u32, stored: u32, scale: u32) -> bool {
    if scale == 0 {
        return false;
    }
    let floor = (logical / scale).max(1);
    let ceil = logical
        .checked_add(scale - 1)
        .map(|value| (value / scale).max(1));
    ceil.is_some_and(|ceil| stored >= floor && stored <= ceil)
}

fn entry_resolution_scale(entry: &DirEntry) -> Result<u32> {
    let (_, logical_x, stored_x) = spatial_dimension(entry, "X")?;
    let (_, logical_y, stored_y) = spatial_dimension(entry, "Y")?;
    let scale_x = rounded_scale(logical_x, stored_x, "X")?;
    let scale_y = rounded_scale(logical_y, stored_y, "Y")?;
    if scale_x == scale_y {
        return Ok(scale_x);
    }

    // Edge tiles can quantize to a different rounded ratio on the shorter
    // axis. The larger logical axis is the more reliable level indicator, but
    // accept it only when both stored sizes are a floor/ceil realization of
    // that common scale; genuinely anisotropic storage still fails.
    let candidate = match logical_x.cmp(&logical_y) {
        std::cmp::Ordering::Greater => scale_x,
        std::cmp::Ordering::Less => scale_y,
        std::cmp::Ordering::Equal => 0,
    };
    if candidate > 0
        && stored_size_matches_scale(logical_x, stored_x, candidate)
        && stored_size_matches_scale(logical_y, stored_y, candidate)
    {
        return Ok(candidate);
    }
    Err(BioFormatsError::InvalidData(format!(
        "CZI pyramid scale is inconsistent between X ({scale_x}) and Y ({scale_y})"
    )))
}

fn is_power_of(scale: u32, factor: u32) -> bool {
    if scale < factor || factor < 2 {
        return false;
    }
    let mut remaining = scale;
    while remaining.is_multiple_of(factor) {
        remaining /= factor;
    }
    remaining == 1
}

fn pyramid_base_factor(scale: u32) -> Option<u32> {
    [2, 3]
        .into_iter()
        .find(|factor| is_power_of(scale, *factor))
}

fn group_entries_by_resolution(
    entries: &[CziLocatedEntry],
    group: &[usize],
) -> Result<BTreeMap<u32, Vec<usize>>> {
    let mut entries_by_scale = BTreeMap::<u32, Vec<usize>>::new();
    let mut reduced = Vec::<(usize, u32, u64, u32)>::new();
    reduced.try_reserve_exact(group.len()).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate CZI pyramid scale map: {error}"))
    })?;

    for index in group.iter().copied() {
        let entry = &entries[index].entry;
        let raw_scale = entry_resolution_scale(entry)?;
        if entry.full_resolution {
            if raw_scale != 1 {
                return Err(BioFormatsError::InvalidData(format!(
                    "CZI native subblock has reduced stored dimensions at scale {raw_scale}"
                )));
            }
            entries_by_scale.entry(1).or_default().push(index);
            continue;
        }

        let (_, _, stored_x) = spatial_dimension(entry, "X")?;
        let (_, _, stored_y) = spatial_dimension(entry, "Y")?;
        let stored_area = u64::from(stored_x)
            .checked_mul(u64::from(stored_y))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        reduced.push((index, raw_scale, stored_area, stored_x.max(stored_y)));
    }
    if reduced.is_empty() {
        return Ok(entries_by_scale);
    }

    // CZI pyramids use one factor (2 or 3) for the complete series. Choose
    // the largest stored tile as the least quantized seed; small edge tiles
    // can otherwise round to the other factor (for example 4/2 beside
    // 100/33 in a factor-3 layer).
    let (_, _, base_factor) = reduced
        .iter()
        .filter_map(|(_, raw_scale, stored_area, stored_axis)| {
            pyramid_base_factor(*raw_scale)
                .map(|factor| ((*stored_area, *stored_axis), *raw_scale, factor))
        })
        .max_by_key(|(size, _, _)| *size)
        .ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "CZI reduced-size subblocks do not identify a 2x or 3x resolution factor".into(),
            )
        })?;

    let mut established_scales = BTreeSet::new();
    for (_, raw_scale, _, _) in &reduced {
        if is_power_of(*raw_scale, base_factor) {
            established_scales.insert(*raw_scale);
        }
    }
    if established_scales.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "CZI pyramid has no identifiable resolution levels".into(),
        ));
    }

    for (index, raw_scale, _, _) in reduced {
        let entry = &entries[index].entry;
        let (_, logical_x, stored_x) = spatial_dimension(entry, "X")?;
        let (_, logical_y, stored_y) = spatial_dimension(entry, "Y")?;
        let mut candidates = established_scales.iter().copied().filter(|scale| {
            stored_size_matches_scale(logical_x, stored_x, *scale)
                && stored_size_matches_scale(logical_y, stored_y, *scale)
        });
        let Some(scale) = candidates.next() else {
            return Err(BioFormatsError::InvalidData(format!(
                "CZI pyramid subblock scale {raw_scale} does not fit any series resolution level"
            )));
        };
        if candidates.next().is_some() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "CZI pyramid subblock with logical/stored size {logical_x}x{logical_y}/{stored_x}x{stored_y} is ambiguous across multiple resolution levels"
            )));
        }
        entries_by_scale.entry(scale).or_default().push(index);
    }

    Ok(entries_by_scale)
}

type CziNativeTileSignature = (i32, i32, u32, u32, u32, u32);

fn split_identical_mosaic_layouts(
    entries: &[CziLocatedEntry],
    group: &[usize],
) -> Result<Vec<(Option<i32>, Vec<usize>)>> {
    let mut by_mosaic = BTreeMap::<i32, Vec<usize>>::new();
    for index in group.iter().copied() {
        by_mosaic
            .entry(dim_start(&entries[index].entry, "M"))
            .or_default()
            .push(index);
    }
    if by_mosaic.len() <= 1 {
        return Ok(vec![(None, group.to_vec())]);
    }

    let mut reference_layout: Option<BTreeSet<CziNativeTileSignature>> = None;
    for indexes in by_mosaic.values() {
        let mut layout = BTreeSet::new();
        for index in indexes.iter().copied() {
            let entry = &entries[index].entry;
            if !entry.full_resolution {
                continue;
            }
            let (x, logical_width, stored_width) = spatial_dimension(entry, "X")?;
            let (y, logical_height, stored_height) = spatial_dimension(entry, "Y")?;
            layout.insert((
                x,
                y,
                logical_width,
                logical_height,
                stored_width,
                stored_height,
            ));
        }
        if layout.is_empty() {
            return Ok(vec![(None, group.to_vec())]);
        }
        if let Some(reference) = &reference_layout {
            if reference != &layout {
                return Ok(vec![(None, group.to_vec())]);
            }
        } else {
            reference_layout = Some(layout);
        }
    }

    Ok(by_mosaic
        .into_iter()
        .map(|(mosaic, indexes)| (Some(mosaic), indexes))
        .collect())
}

#[derive(Debug, Clone, Copy)]
struct PendingCziTile {
    entry_index: usize,
    logical_x: i32,
    logical_y: i32,
    stored_width: u32,
    stored_height: u32,
}

struct PendingCziLevel {
    scale: u32,
    size_x: u32,
    size_y: u32,
    planes: Vec<CziPlaneRef>,
    subblock_count: usize,
}

#[derive(Debug, Clone, Copy)]
struct CziPlaneGeometry {
    min_x: i32,
    min_y: i32,
    max_x: i64,
    max_y: i64,
}

fn entry_plane_index(entry: &DirEntry, coordinate_metadata: &ImageMetadata) -> Result<usize> {
    let z = u32::try_from(dim_start(entry, "Z")).map_err(|_| {
        BioFormatsError::InvalidData("CZI Z coordinate must not be negative".into())
    })?;
    let c = u32::try_from(dim_start(entry, "C")).map_err(|_| {
        BioFormatsError::InvalidData("CZI C coordinate must not be negative".into())
    })?;
    let t = u32::try_from(dim_start(entry, "T")).map_err(|_| {
        BioFormatsError::InvalidData("CZI T coordinate must not be negative".into())
    })?;
    let plane_index = coordinate_metadata.checked_index(z, c, t).ok_or_else(|| {
        BioFormatsError::InvalidData(format!(
            "CZI coordinates Z={z}, C={c}, T={t} do not map to a plane"
        ))
    })?;
    usize::try_from(plane_index).map_err(|_| BioFormatsError::PlaneByteCountOverflow)
}

fn build_base_plane_geometry(
    entries: &[CziLocatedEntry],
    native_entries: &[usize],
    coordinate_metadata: &ImageMetadata,
) -> Result<Vec<CziPlaneGeometry>> {
    let plane_count = usize::try_from(coordinate_metadata.image_count)
        .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    let mut common_geometry: Option<CziPlaneGeometry> = None;

    for entry_index in native_entries.iter().copied() {
        let entry = &entries[entry_index].entry;
        entry_plane_index(entry, coordinate_metadata)?;
        let (logical_x, logical_width, _) = spatial_dimension(entry, "X")?;
        let (logical_y, logical_height, _) = spatial_dimension(entry, "Y")?;
        let max_x = i64::from(logical_x)
            .checked_add(i64::from(logical_width))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let max_y = i64::from(logical_y)
            .checked_add(i64::from(logical_height))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        common_geometry = Some(match common_geometry {
            Some(current) => CziPlaneGeometry {
                min_x: current.min_x.min(logical_x),
                min_y: current.min_y.min(logical_y),
                max_x: current.max_x.max(max_x),
                max_y: current.max_y.max(max_y),
            },
            None => CziPlaneGeometry {
                min_x: logical_x,
                min_y: logical_y,
                max_x,
                max_y,
            },
        });
    }

    let common_geometry = common_geometry
        .ok_or_else(|| BioFormatsError::Format("CZI native scale has no tile geometry".into()))?;
    let mut geometry = Vec::new();
    geometry.try_reserve_exact(plane_count).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate CZI base geometry: {error}"))
    })?;
    geometry.resize(plane_count, common_geometry);
    Ok(geometry)
}

fn scaled_canvas_extent(minimum: i32, maximum: i64, scale: u32, axis: &str) -> Result<u32> {
    let logical_extent = maximum
        .checked_sub(i64::from(minimum))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| {
            BioFormatsError::InvalidData(format!("CZI mosaic {axis} extent is invalid"))
        })?;
    u32::try_from(logical_extent / u64::from(scale))
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            BioFormatsError::InvalidData(format!(
                "CZI pyramid scale {scale} produces an invalid {axis} canvas"
            ))
        })
}

fn build_czi_level(
    entries: &[CziLocatedEntry],
    level_entries: &[usize],
    scale: u32,
    coordinate_metadata: &ImageMetadata,
    base_geometry: &[CziPlaneGeometry],
) -> Result<PendingCziLevel> {
    if scale == 0 {
        return Err(BioFormatsError::InvalidData(
            "CZI pyramid resolution has a zero scale".into(),
        ));
    }
    let plane_count = usize::try_from(coordinate_metadata.image_count)
        .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    let mut pending_planes = Vec::<Vec<PendingCziTile>>::new();
    pending_planes
        .try_reserve_exact(plane_count)
        .map_err(|error| {
            BioFormatsError::InvalidData(format!("cannot allocate CZI plane map: {error}"))
        })?;
    pending_planes.resize_with(plane_count, Vec::new);

    for entry_index in level_entries.iter().copied() {
        let entry = &entries[entry_index].entry;
        let plane_index = entry_plane_index(entry, coordinate_metadata)?;
        let (logical_x, _, stored_width) = spatial_dimension(entry, "X")?;
        let (logical_y, _, stored_height) = spatial_dimension(entry, "Y")?;
        let plane = pending_planes.get_mut(plane_index).ok_or_else(|| {
            BioFormatsError::InvalidData("CZI plane index exceeds its plane map".into())
        })?;
        plane.try_reserve(1).map_err(|error| {
            BioFormatsError::InvalidData(format!("cannot allocate CZI tile list: {error}"))
        })?;
        plane.push(PendingCziTile {
            entry_index,
            logical_x,
            logical_y,
            stored_width,
            stored_height,
        });
    }

    let mut size_x = 0_u32;
    let mut size_y = 0_u32;
    let mut planes = Vec::new();
    planes.try_reserve_exact(plane_count).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate CZI plane list: {error}"))
    })?;
    for (plane_index, pending_tiles) in pending_planes.into_iter().enumerate() {
        let geometry = base_geometry.get(plane_index).ok_or_else(|| {
            BioFormatsError::InvalidData(format!("CZI plane {plane_index} has no native geometry"))
        })?;
        let plane_size_x = scaled_canvas_extent(geometry.min_x, geometry.max_x, scale, "X")?;
        let plane_size_y = scaled_canvas_extent(geometry.min_y, geometry.max_y, scale, "Y")?;
        size_x = size_x.max(plane_size_x);
        size_y = size_y.max(plane_size_y);

        let mut tiles = Vec::new();
        tiles
            .try_reserve_exact(pending_tiles.len())
            .map_err(|error| {
                BioFormatsError::InvalidData(format!("cannot allocate CZI tile set: {error}"))
            })?;
        for tile in pending_tiles {
            let normalized_x = i64::from(tile.logical_x)
                .checked_sub(i64::from(geometry.min_x))
                .and_then(|value| u64::try_from(value).ok())
                .ok_or_else(|| {
                    BioFormatsError::InvalidData("CZI tile X origin is invalid".into())
                })?;
            let normalized_y = i64::from(tile.logical_y)
                .checked_sub(i64::from(geometry.min_y))
                .and_then(|value| u64::try_from(value).ok())
                .ok_or_else(|| {
                    BioFormatsError::InvalidData("CZI tile Y origin is invalid".into())
                })?;
            let x = u32::try_from(normalized_x / u64::from(scale))
                .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
            let y = u32::try_from(normalized_y / u64::from(scale))
                .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
            x.checked_add(tile.stored_width)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            y.checked_add(tile.stored_height)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            tiles.push(CziTileRef {
                entry_index: tile.entry_index,
                x,
                y,
                width: tile.stored_width,
                height: tile.stored_height,
            });
        }
        planes.push(CziPlaneRef { tiles });
    }

    Ok(PendingCziLevel {
        scale,
        size_x,
        size_y,
        planes,
        subblock_count: level_entries.len(),
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

fn czi_byte_len(
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    pixel_type: PixelType,
) -> Result<usize> {
    let length = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| {
            usize::try_from(samples_per_pixel)
                .ok()
                .and_then(|samples| pixels.checked_mul(samples))
        })
        .and_then(|samples| samples.checked_mul(pixel_type.bytes_per_sample()))
        .filter(|length| *length <= isize::MAX as usize)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    Ok(length)
}

fn subblock_header_size(entry: &DirEntry) -> Result<u64> {
    let directory_bytes = u64::from(entry.dimension_count)
        .checked_mul(20)
        .and_then(|bytes| bytes.checked_add(32))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    16_u64
        .checked_add(directory_bytes)
        .map(|bytes| bytes.max(256))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)
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
        for axis in ["Z", "C", "T"] {
            if dim_size(&located.entry, axis)? != 1 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "CZI: subblocks spanning multiple {axis} positions are not supported"
                )));
            }
        }
    }

    let xml = parse_czi_metadata(metadata_xml)?;
    let mut grouped = BTreeMap::<(i32, i32, i32), Vec<usize>>::new();
    for (index, entry) in entries.iter().enumerate() {
        let key = (
            dim_start(&entry.entry, "S"),
            dim_start(&entry.entry, "B"),
            dim_start(&entry.entry, "V"),
        );
        grouped.entry(key).or_default().push(index);
    }

    let mut series = Vec::new();
    series.try_reserve_exact(grouped.len()).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate CZI series metadata: {error}"))
    })?;
    for ((scene_index, _, _), coarse_group) in grouped {
        let coarse_logical_channels = max_group_extent(entries, &coarse_group, "C")?;
        let coarse_size_z = max_group_extent(entries, &coarse_group, "Z")?;
        let coarse_size_t = max_group_extent(entries, &coarse_group, "T")?;
        let mosaic_groups = split_identical_mosaic_layouts(entries, &coarse_group)?;
        series.try_reserve(mosaic_groups.len()).map_err(|error| {
            BioFormatsError::InvalidData(format!("cannot grow CZI series metadata: {error}"))
        })?;
        for (mosaic_index, group) in mosaic_groups {
            let layout_pixel_type = entries[group[0]].entry.pixel_type;
            let pixel = czi_pixel_info(layout_pixel_type)?;
            for index in group.iter().copied().skip(1) {
                let entry_pixel_type = entries[index].entry.pixel_type;
                let entry_pixel = czi_pixel_info(entry_pixel_type)?;
                if entry_pixel != pixel {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                    "CZI series contains heterogeneous selected pixel types ({layout_pixel_type} and {entry_pixel_type}) whose layouts differ; splitting mixed pixel types is not yet supported"
                )));
                }
            }
            let (logical_channels, size_z, size_t) = if mosaic_index.is_some() {
                (coarse_logical_channels, coarse_size_z, coarse_size_t)
            } else {
                (
                    max_group_extent(entries, &group, "C")?,
                    max_group_extent(entries, &group, "Z")?,
                    max_group_extent(entries, &group, "T")?,
                )
            };
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

            let coordinate_metadata = ImageMetadata {
                size_z,
                size_c: logical_channels,
                size_t,
                image_count,
                dimension_order: DimensionOrder::XYCZT,
                ..ImageMetadata::default()
            };
            let entries_by_scale = group_entries_by_resolution(entries, &group)?;
            if entries_by_scale.first_key_value().map(|(scale, _)| *scale) != Some(1) {
                return Err(BioFormatsError::UnsupportedFormat(
                    "CZI pyramid does not contain a native scale-1 level".into(),
                ));
            }
            let base_geometry = build_base_plane_geometry(
                entries,
                entries_by_scale
                    .get(&1)
                    .expect("native CZI scale was checked above"),
                &coordinate_metadata,
            )?;
            let mut pending_levels = Vec::new();
            pending_levels
                .try_reserve_exact(entries_by_scale.len())
                .map_err(|error| {
                    BioFormatsError::InvalidData(format!(
                        "cannot allocate CZI resolution levels: {error}"
                    ))
                })?;
            for (scale, level_entries) in entries_by_scale {
                pending_levels.push(build_czi_level(
                    entries,
                    &level_entries,
                    scale,
                    &coordinate_metadata,
                    &base_geometry,
                )?);
            }
            let resolution_count = u32::try_from(pending_levels.len())
                .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;

            let mut metadata = ImageMetadata {
                size_x: pending_levels[0].size_x,
                size_y: pending_levels[0].size_y,
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
                resolution_count,
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
                MetadataValue::Int(
                    i64::try_from(group.len())
                        .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?,
                ),
            );
            metadata.series_metadata.insert(
                "czi_scene_index".into(),
                MetadataValue::Int(scene_index as i64),
            );
            if let Some(mosaic_index) = mosaic_index {
                metadata.series_metadata.insert(
                    "czi_mosaic_index".into(),
                    MetadataValue::Int(i64::from(mosaic_index)),
                );
            }

            let image_count_usize = usize::try_from(image_count)
                .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
            let scene_position = usize::try_from(scene_index)
                .ok()
                .and_then(|index| xml.scene_positions.get(index).copied())
                .unwrap_or((None, None, None));

            metadata
                .plane_metadata
                .try_reserve_exact(image_count_usize)
                .map_err(|error| {
                    BioFormatsError::InvalidData(format!(
                        "cannot allocate CZI plane metadata: {error}"
                    ))
                })?;
            for plane_index in 0..image_count {
                let (z, c, t) = coordinate_metadata.get_zct_coords(plane_index);
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

            let mut resolutions = Vec::new();
            resolutions
                .try_reserve_exact(pending_levels.len())
                .map_err(|error| {
                    BioFormatsError::InvalidData(format!(
                        "cannot allocate CZI resolution metadata: {error}"
                    ))
                })?;
            for pending in pending_levels {
                let mut level_metadata = metadata.clone();
                level_metadata.size_x = pending.size_x;
                level_metadata.size_y = pending.size_y;
                level_metadata.series_metadata.insert(
                    "czi_subblocks".into(),
                    MetadataValue::Int(
                        i64::try_from(pending.subblock_count)
                            .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?,
                    ),
                );
                level_metadata.series_metadata.insert(
                    "czi_resolution_scale".into(),
                    MetadataValue::Int(i64::from(pending.scale)),
                );
                resolutions.push(CziResolutionLevel {
                    metadata: level_metadata,
                    planes: pending.planes,
                    scale: pending.scale,
                });
            }

            series.push(CziSeries {
                metadata: resolutions[0].metadata.clone(),
                resolutions,
                samples_per_pixel: pixel.samples_per_pixel,
                bgr_order: pixel.bgr_order,
            });
        }
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
    current_resolution: usize,
    flattened_resolutions: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CziReaderSnapshot {
    pub path: PathBuf,
    pub used_files: Vec<PathBuf>,
    pub entries: Vec<CziLocatedEntry>,
    pub meta_xml: String,
    pub series: Vec<CziSeries>,
    pub current_series: usize,
    pub current_resolution: usize,
    pub flattened_resolutions: bool,
}

impl CziReaderSnapshot {
    pub(crate) fn retarget_primary_path(&mut self, path: &Path) {
        let old_primary = self.path.clone();
        let old_base = file_stem_without_part(&old_primary);
        let new_base = file_stem_without_part(path);
        let retarget = |member: &Path| {
            if member == old_primary {
                return path.to_path_buf();
            }
            if let (Some(old_base), Some(new_base), Some(member_base), Some(member_stem)) = (
                old_base.as_deref(),
                new_base.as_deref(),
                file_stem_without_part(member),
                member.file_stem().and_then(|stem| stem.to_str()),
            ) {
                if member_base == old_base {
                    let suffix = &member_stem[old_base.len()..];
                    if let (Some(new_parent), Some(extension)) = (path.parent(), member.extension())
                    {
                        let mut filename = format!("{new_base}{suffix}");
                        filename.push('.');
                        filename.push_str(&extension.to_string_lossy());
                        return new_parent.join(filename);
                    }
                }
            }
            match (
                old_primary.parent(),
                path.parent(),
                old_primary
                    .parent()
                    .and_then(|parent| member.strip_prefix(parent).ok()),
            ) {
                (Some(_), Some(new_parent), Some(relative)) => new_parent.join(relative),
                _ => member.to_path_buf(),
            }
        };

        self.used_files = self
            .used_files
            .iter()
            .map(|member| retarget(member))
            .collect();
        for series in &mut self.series {
            series.metadata.used_files = series
                .metadata
                .used_files
                .iter()
                .map(|member| retarget(member))
                .collect();
            for resolution in &mut series.resolutions {
                resolution.metadata.used_files = resolution
                    .metadata
                    .used_files
                    .iter()
                    .map(|member| retarget(member))
                    .collect();
            }
        }
        self.path = path.to_path_buf();
    }
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
            current_resolution: 0,
            flattened_resolutions: true,
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
            current_resolution: snapshot.current_resolution,
            flattened_resolutions: snapshot.flattened_resolutions,
        })
    }

    fn active_indices(&self) -> Result<(usize, usize)> {
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if self.flattened_resolutions {
            let mut exposed = 0_usize;
            for (series_index, series) in self.series.iter().enumerate() {
                let next = exposed
                    .checked_add(series.resolutions.len())
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                if self.current_series < next {
                    return Ok((series_index, self.current_series - exposed));
                }
                exposed = next;
            }
            return Err(BioFormatsError::SeriesOutOfRange(self.current_series));
        }

        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::SeriesOutOfRange(self.current_series))?;
        if self.current_resolution >= series.resolutions.len() {
            return Err(BioFormatsError::ResolutionOutOfRange {
                series: self.current_series,
                resolution: self.current_resolution,
            });
        }
        Ok((self.current_series, self.current_resolution))
    }

    fn active_level(&self) -> Result<(&CziSeries, &CziResolutionLevel)> {
        let (series_index, resolution_index) = self.active_indices()?;
        let series = &self.series[series_index];
        let resolution = series.resolutions.get(resolution_index).ok_or(
            BioFormatsError::ResolutionOutOfRange {
                series: series_index,
                resolution: resolution_index,
            },
        )?;
        Ok((series, resolution))
    }

    fn read_tile(&self, tile: &CziTileRef, series: &CziSeries) -> Result<Vec<u8>> {
        let located = self.entries.get(tile.entry_index).ok_or_else(|| {
            BioFormatsError::InvalidData(format!(
                "CZI tile references missing directory entry {}",
                tile.entry_index
            ))
        })?;
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
        let header_size = subblock_header_size(&located.entry)?;
        let payload_size = header_size
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
            .and_then(|position| position.checked_add(header_size))
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
        let expected = czi_byte_len(
            tile.width,
            tile.height,
            series.samples_per_pixel,
            series.metadata.pixel_type,
        )?;
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
        let mut raw = if located.entry.compression == 1 {
            decompress_czi_jpeg(
                &compressed,
                tile.width,
                tile.height,
                series.samples_per_pixel,
                series.metadata.pixel_type,
            )?
        } else {
            decompress_subblock(&compressed, located.entry.compression, expected)?
        };
        if raw.len() != expected {
            return Err(BioFormatsError::InvalidData(format!(
                "CZI subblock decoded to {} bytes; expected {expected}",
                raw.len()
            )));
        }
        if series.bgr_order {
            bgr_to_rgb_in_place(
                &mut raw,
                series.samples_per_pixel,
                series.metadata.pixel_type.bytes_per_sample(),
            );
        }
        Ok(raw)
    }

    fn compose_region(
        &self,
        series_index: usize,
        resolution_index: usize,
        plane_index: u32,
        region: CziRegion,
    ) -> Result<Vec<u8>> {
        let CziRegion {
            x,
            y,
            width,
            height,
        } = region;
        let series = self
            .series
            .get(series_index)
            .ok_or(BioFormatsError::SeriesOutOfRange(series_index))?;
        let resolution = series.resolutions.get(resolution_index).ok_or(
            BioFormatsError::ResolutionOutOfRange {
                series: series_index,
                resolution: resolution_index,
            },
        )?;
        validate_region(&resolution.metadata, x, y, width, height)?;
        let plane = resolution
            .planes
            .get(
                usize::try_from(plane_index)
                    .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?,
            )
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;

        let bytes_per_sample = resolution.metadata.pixel_type.bytes_per_sample();
        let pixel_stride = usize::try_from(series.samples_per_pixel)
            .ok()
            .and_then(|samples| samples.checked_mul(bytes_per_sample))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let output_length = czi_byte_len(
            width,
            height,
            series.samples_per_pixel,
            resolution.metadata.pixel_type,
        )?;
        let fill = if resolution.metadata.is_rgb && series.resolutions.len() > 1 {
            255
        } else {
            0
        };
        let mut output = Vec::new();
        output.try_reserve_exact(output_length).map_err(|error| {
            BioFormatsError::InvalidData(format!(
                "cannot allocate {output_length}-byte CZI output region: {error}"
            ))
        })?;
        output.resize(output_length, fill);

        let request_right = x
            .checked_add(width)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let request_bottom = y
            .checked_add(height)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let output_row_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(pixel_stride))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;

        // Tile references preserve directory order.  Copying in that order
        // makes later subblocks win wherever tiles overlap, matching Java.
        for tile in &plane.tiles {
            let tile_right = tile
                .x
                .checked_add(tile.width)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let tile_bottom = tile
                .y
                .checked_add(tile.height)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let intersection_left = x.max(tile.x);
            let intersection_top = y.max(tile.y);
            let intersection_right = request_right.min(tile_right);
            let intersection_bottom = request_bottom.min(tile_bottom);
            if intersection_left >= intersection_right || intersection_top >= intersection_bottom {
                continue;
            }

            let tile_bytes = self.read_tile(tile, series)?;
            let copy_width = intersection_right - intersection_left;
            let copy_height = intersection_bottom - intersection_top;
            let copy_bytes = usize::try_from(copy_width)
                .ok()
                .and_then(|width| width.checked_mul(pixel_stride))
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let tile_row_bytes = usize::try_from(tile.width)
                .ok()
                .and_then(|width| width.checked_mul(pixel_stride))
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let source_x_bytes = usize::try_from(intersection_left - tile.x)
                .ok()
                .and_then(|offset| offset.checked_mul(pixel_stride))
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            let destination_x_bytes = usize::try_from(intersection_left - x)
                .ok()
                .and_then(|offset| offset.checked_mul(pixel_stride))
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;

            for row in 0..copy_height {
                let source_y = intersection_top
                    .checked_sub(tile.y)
                    .and_then(|value| value.checked_add(row))
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let destination_y = intersection_top
                    .checked_sub(y)
                    .and_then(|value| value.checked_add(row))
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let source_start = usize::try_from(source_y)
                    .ok()
                    .and_then(|row| row.checked_mul(tile_row_bytes))
                    .and_then(|offset| offset.checked_add(source_x_bytes))
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let source_end = source_start
                    .checked_add(copy_bytes)
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let destination_start = usize::try_from(destination_y)
                    .ok()
                    .and_then(|row| row.checked_mul(output_row_bytes))
                    .and_then(|offset| offset.checked_add(destination_x_bytes))
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let destination_end = destination_start
                    .checked_add(copy_bytes)
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let source = tile_bytes.get(source_start..source_end).ok_or_else(|| {
                    BioFormatsError::InvalidData(
                        "CZI tile intersection exceeds decoded pixels".into(),
                    )
                })?;
                let destination = output
                    .get_mut(destination_start..destination_end)
                    .ok_or_else(|| {
                        BioFormatsError::InvalidData(
                            "CZI tile intersection exceeds its output region".into(),
                        )
                    })?;
                destination.copy_from_slice(source);
            }
        }
        Ok(output)
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
        let primary_path = primary.path().map(Path::to_path_buf);
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
        self.path = primary_path;
        self.used_files = used_files;
        self.sources = sources;
        self.entries = entries;
        self.meta_xml = meta_xml;
        self.series = series;
        self.current_series = 0;
        self.current_resolution = 0;
        self.flattened_resolutions = true;
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
        self.current_resolution = 0;
        self.flattened_resolutions = true;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.series.is_empty() {
            1
        } else if self.flattened_resolutions {
            self.series.iter().fold(0_usize, |count, series| {
                count.saturating_add(series.resolutions.len())
            })
        } else {
            self.series.len()
        }
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if self.flattened_resolutions {
            let total = self.series.iter().try_fold(0_usize, |count, entry| {
                count
                    .checked_add(entry.resolutions.len())
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)
            })?;
            if series >= total {
                return Err(BioFormatsError::SeriesOutOfRange(series));
            }
            self.current_series = series;
            return Ok(());
        }
        if series >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        self.current_resolution = 0;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.active_level().expect("set_id not called").1.metadata
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
        let (series_index, resolution_index) = self.active_indices()?;
        let metadata = &self.series[series_index].resolutions[resolution_index].metadata;
        if plane_index >= metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.compose_region(
            series_index,
            resolution_index,
            plane_index,
            CziRegion {
                x: 0,
                y: 0,
                width: metadata.size_x,
                height: metadata.size_y,
            },
        )
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let (series_index, resolution_index) = self.active_indices()?;
        self.compose_region(
            series_index,
            resolution_index,
            plane_index,
            CziRegion {
                x,
                y,
                width: w,
                height: h,
            },
        )
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (series_index, active_resolution) = self.active_indices()?;
        let resolution_count = self.series[series_index].resolutions.len();
        let thumbnail_resolution = resolution_count.checked_sub(1).ok_or_else(|| {
            BioFormatsError::InvalidData("CZI series exposes no resolutions".into())
        })?;
        let metadata = &self.series[series_index].resolutions[thumbnail_resolution].metadata;
        if plane_index >= metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let thumbnail_has_tiles = usize::try_from(plane_index)
            .ok()
            .and_then(|plane| {
                self.series[series_index].resolutions[thumbnail_resolution]
                    .planes
                    .get(plane)
            })
            .is_some_and(|plane| !plane.tiles.is_empty());
        if resolution_count > 1 && thumbnail_has_tiles {
            match self.compose_region(
                series_index,
                thumbnail_resolution,
                plane_index,
                CziRegion {
                    x: 0,
                    y: 0,
                    width: metadata.size_x,
                    height: metadata.size_y,
                },
            ) {
                Ok(bytes) => return Ok(bytes),
                Err(BioFormatsError::UnsupportedFormat(_)) => {}
                Err(error) => return Err(error),
            }
        }
        let fallback_resolution = if active_resolution == thumbnail_resolution {
            0
        } else {
            active_resolution
        };
        let metadata = &self.series[series_index].resolutions[fallback_resolution].metadata;
        let (thumb_w, thumb_h) = (metadata.size_x.min(256), metadata.size_y.min(256));
        let (thumb_x, thumb_y) = (
            (metadata.size_x - thumb_w) / 2,
            (metadata.size_y - thumb_h) / 2,
        );
        self.compose_region(
            series_index,
            fallback_resolution,
            plane_index,
            CziRegion {
                x: thumb_x,
                y: thumb_y,
                width: thumb_w,
                height: thumb_h,
            },
        )
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
            current_resolution: self.current_resolution,
            flattened_resolutions: self.flattened_resolutions,
        }))
    }

    fn resolution_count(&self) -> usize {
        if self.flattened_resolutions || self.series.is_empty() {
            return 1;
        }
        self.series
            .get(self.current_series)
            .map(|series| series.resolutions.len())
            .unwrap_or(1)
    }

    fn set_flattened_resolutions(&mut self, flattened: bool) -> Result<()> {
        if flattened == self.flattened_resolutions {
            return Ok(());
        }
        if self.series.is_empty() {
            self.flattened_resolutions = flattened;
            self.current_series = 0;
            self.current_resolution = 0;
            return Ok(());
        }

        if flattened {
            let root_series = self.current_series;
            let resolution = self.current_resolution;
            let active = self
                .series
                .get(root_series)
                .ok_or(BioFormatsError::SeriesOutOfRange(root_series))?;
            if resolution >= active.resolutions.len() {
                return Err(BioFormatsError::ResolutionOutOfRange {
                    series: root_series,
                    resolution,
                });
            }
            let preceding =
                self.series[..root_series]
                    .iter()
                    .try_fold(0_usize, |count, series| {
                        count
                            .checked_add(series.resolutions.len())
                            .ok_or(BioFormatsError::PlaneByteCountOverflow)
                    })?;
            self.current_series = preceding
                .checked_add(resolution)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            self.current_resolution = 0;
        } else {
            let (root_series, resolution) = self.active_indices()?;
            self.current_series = root_series;
            self.current_resolution = resolution;
        }
        self.flattened_resolutions = flattened;
        Ok(())
    }

    fn flattened_resolutions(&self) -> bool {
        self.flattened_resolutions
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.flattened_resolutions {
            if level == 0 {
                return Ok(());
            }
            return Err(BioFormatsError::ResolutionOutOfRange {
                series: self.current_series,
                resolution: level,
            });
        }
        let count = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::SeriesOutOfRange(self.current_series))?
            .resolutions
            .len();
        if level >= count {
            return Err(BioFormatsError::ResolutionOutOfRange {
                series: self.current_series,
                resolution: level,
            });
        }
        self.current_resolution = level;
        Ok(())
    }

    fn resolution(&self) -> usize {
        if self.flattened_resolutions {
            0
        } else {
            self.current_resolution
        }
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
                dimension_count: 0,
                full_resolution: true,
                stored_size_x: None,
                stored_size_y: None,
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
        assert!(parse_dir_entry(&bytes).unwrap().full_resolution);
    }

    #[test]
    fn expands_subblock_header_for_large_dimension_directories() {
        let mut located = entry(
            0,
            &[
                ("X", (0, 1)),
                ("Y", (0, 1)),
                ("Z", (0, 1)),
                ("C", (0, 1)),
                ("T", (0, 1)),
            ],
        );
        located.entry.dimension_count = 10;
        assert_eq!(subblock_header_size(&located.entry).unwrap(), 256);
        located.entry.dimension_count = 11;
        assert_eq!(subblock_header_size(&located.entry).unwrap(), 268);
    }

    #[test]
    fn infers_level_scale_from_the_larger_quantized_axis() {
        let mut located = entry(
            0,
            &[
                ("X", (0, 4)),
                ("Y", (0, 100)),
                ("Z", (0, 1)),
                ("C", (0, 1)),
                ("T", (0, 1)),
            ],
        );
        located.entry.stored_size_x = Some(2);
        located.entry.stored_size_y = Some(33);
        assert_eq!(entry_resolution_scale(&located.entry).unwrap(), 3);
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
