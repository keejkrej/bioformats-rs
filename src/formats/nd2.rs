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
    ChannelMetadata, DimensionOrder, ImageMetadata, LookupTable, MetadataValue, PlaneMetadata,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{validate_region, FormatReader};
use crate::snapshot::ReaderSnapshot;
use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};

pub const ND2_MAGIC: [u8; 4] = [0xDA, 0xCE, 0xBE, 0x0A];

const MAX_CHUNK_NAME_BYTES: u64 = 1024 * 1024;
const MAX_METADATA_CHUNK_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nd2Chunk {
    name: String,
    data_offset: u64,
    data_length: u64,
}

fn scan_chunks(file: &mut BufReader<File>) -> std::io::Result<Vec<Nd2Chunk>> {
    let mut chunks = Vec::new();
    let file_len = file.get_ref().metadata()?.len();
    let mut search_from = 0_u64;

    while let Some(chunk_start) = find_next_magic(file, search_from, file_len)? {
        let header_start = chunk_start
            .checked_add(ND2_MAGIC.len() as u64)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "ND2 chunk offset overflow")
            })?;
        file.seek(SeekFrom::Start(header_start))?;

        let mut name_len_bytes = [0_u8; 4];
        let mut data_len_bytes = [0_u8; 8];
        if file.read_exact(&mut name_len_bytes).is_err()
            || file.read_exact(&mut data_len_bytes).is_err()
        {
            break;
        }
        let name_len = u64::from(u32::from_le_bytes(name_len_bytes));
        let data_len = u64::from_le_bytes(data_len_bytes);
        let data_offset = chunk_start
            .checked_add(16)
            .and_then(|offset| offset.checked_add(name_len));
        let chunk_end = data_offset.and_then(|offset| offset.checked_add(data_len));
        let Some((data_offset, chunk_end)) = data_offset.zip(chunk_end) else {
            search_from = chunk_start.saturating_add(1);
            continue;
        };
        if name_len == 0
            || name_len > MAX_CHUNK_NAME_BYTES
            || data_offset > file_len
            || chunk_end > file_len
        {
            search_from = chunk_start.saturating_add(1);
            continue;
        }

        let name_len_usize = usize::try_from(name_len).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ND2 chunk name does not fit in memory",
            )
        })?;
        let mut name_bytes = allocate_bytes(name_len_usize, "ND2 chunk name")?;
        file.read_exact(&mut name_bytes)?;
        let logical_name_len = name_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name_bytes.len());
        let name = String::from_utf8_lossy(&name_bytes[..logical_name_len]).to_string();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            search_from = chunk_start.saturating_add(1);
            continue;
        }

        chunks.push(Nd2Chunk {
            name,
            data_offset,
            data_length: data_len,
        });
        search_from = chunk_end;
    }

    Ok(chunks)
}

fn find_next_magic(
    file: &mut BufReader<File>,
    start: u64,
    file_len: u64,
) -> std::io::Result<Option<u64>> {
    if file_len.saturating_sub(start) < ND2_MAGIC.len() as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(start))?;
    let mut window = [0_u8; 4];
    file.read_exact(&mut window)?;
    let mut position = start;
    loop {
        if window == ND2_MAGIC {
            return Ok(Some(position));
        }
        if position
            .checked_add(ND2_MAGIC.len() as u64)
            .is_none_or(|end| end >= file_len)
        {
            return Ok(None);
        }
        let mut next = [0_u8; 1];
        file.read_exact(&mut next)?;
        window.copy_within(1.., 0);
        window[3] = next[0];
        position += 1;
    }
}

fn read_chunk_data(file: &mut BufReader<File>, chunk: &Nd2Chunk) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(chunk.data_offset))?;
    let data_length = usize::try_from(chunk.data_length).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ND2 chunk data does not fit in memory",
        )
    })?;
    let mut data = allocate_bytes(data_length, "ND2 chunk payload")?;
    file.read_exact(&mut data)?;
    Ok(data)
}

fn allocate_bytes(length: usize, context: &str) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            format!("cannot allocate {length} bytes for {context}: {error}"),
        )
    })?;
    bytes.resize(length, 0);
    Ok(bytes)
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

fn image_sequence_number(name: &str) -> Option<u32> {
    name.strip_prefix("ImageDataSeq|")?
        .strip_suffix('!')?
        .parse()
        .ok()
}

#[derive(Debug, Default)]
struct Nd2MetadataModel {
    size_x: Option<u32>,
    size_y: Option<u32>,
    row_bytes: Option<u32>,
    logical_channels: Option<u32>,
    size_z: Option<u32>,
    size_t: Option<u32>,
    series_count: Option<u32>,
    storage_bits: Option<u8>,
    significant_bits: Option<u8>,
    compression: Option<Nd2Compression>,
    acquisition_order: Vec<Nd2Axis>,
    experiment_roots: u32,
    singleton_c_after_z: bool,
    acquisition_candidates: Vec<Nd2AcquisitionCandidate>,
    physical_size_x_um: Option<f64>,
    physical_size_y_um: Option<f64>,
    physical_size_z_um: Option<f64>,
    time_increment_seconds: Option<f64>,
    objective_model: Option<String>,
    objective_magnification: Option<f64>,
    channel_metadata: Vec<ChannelMetadata>,
    channel_colors: Vec<Nd2ChannelColor>,
    exposure_times_seconds: Vec<f64>,
    timepoints_seconds: Vec<f64>,
    positions_x_um: Vec<f64>,
    positions_y_um: Vec<f64>,
    positions_z_um: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Nd2Compression {
    Raw,
    Zlib,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nd2Axis {
    Z,
    T,
    Series,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nd2AcquisitionSource {
    BinaryLv,
    Xml,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Nd2AcquisitionCandidate {
    size_z: u32,
    size_t: u32,
    series_count: u32,
    acquisition_order: Vec<Nd2Axis>,
    source: Nd2AcquisitionSource,
    singleton_c_after_z: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Nd2ChannelColor {
    name: String,
    color: u32,
}

#[derive(Debug, Clone)]
enum LvValue {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Text(String),
}

impl LvValue {
    fn as_u32(&self) -> Option<u32> {
        match self {
            Self::Bool(value) => Some(u32::from(*value)),
            Self::Signed(value) => u32::try_from(*value).ok(),
            Self::Unsigned(value) => u32::try_from(*value).ok(),
            Self::Float(value)
                if value.is_finite() && *value >= 0.0 && *value <= u32::MAX as f64 =>
            {
                Some(*value as u32)
            }
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
            Self::Signed(value) => Some(*value as f64),
            Self::Unsigned(value) => Some(*value as f64),
            Self::Float(value) => Some(*value),
            Self::Text(value) => value.parse().ok(),
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

fn take_bytes<'a>(
    data: &'a [u8],
    cursor: &mut usize,
    stop: usize,
    count: usize,
) -> Option<&'a [u8]> {
    let end = cursor.checked_add(count)?;
    if end > stop || end > data.len() {
        return None;
    }
    let bytes = &data[*cursor..end];
    *cursor = end;
    Some(bytes)
}

fn read_u32_le(data: &[u8], cursor: &mut usize, stop: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        take_bytes(data, cursor, stop, 4)?.try_into().ok()?,
    ))
}

fn read_u64_le(data: &[u8], cursor: &mut usize, stop: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        take_bytes(data, cursor, stop, 8)?.try_into().ok()?,
    ))
}

fn read_utf16_name(data: &[u8], cursor: &mut usize, stop: usize, units: usize) -> Option<String> {
    let byte_count = units.checked_mul(2)?;
    let bytes = take_bytes(data, cursor, stop, byte_count)?;
    let mut decoded = Vec::with_capacity(units);
    for pair in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        decoded.push(unit);
    }
    Some(String::from_utf16_lossy(&decoded))
}

fn read_utf16_cstring(data: &[u8], cursor: &mut usize, stop: usize) -> Option<String> {
    let mut decoded = Vec::new();
    loop {
        let bytes = take_bytes(data, cursor, stop, 2)?;
        let unit = u16::from_le_bytes([bytes[0], bytes[1]]);
        if unit == 0 {
            return Some(String::from_utf16_lossy(&decoded));
        }
        decoded.push(unit);
    }
}

fn parse_lv_sequence(
    data: &[u8],
    mut cursor: usize,
    stop: usize,
    depth: usize,
    values: &mut Vec<(String, LvValue)>,
    channel_colors: &mut Vec<Nd2ChannelColor>,
    acquisition_roots: &mut Vec<Vec<(String, LvValue)>>,
) {
    if depth > 64 || stop > data.len() {
        return;
    }
    // Nikon associates sDescription with the most recently encountered
    // uiColor in the same LV level. Keep this state local to the level so a
    // color in a parent or sibling container cannot leak into the pairing.
    let mut current_color = None;
    while cursor < stop {
        let record_start = cursor;
        let Some(header) = take_bytes(data, &mut cursor, stop, 2) else {
            return;
        };
        let value_type = header[0];
        let name_units = usize::from(header[1]);
        if !(1..=11).contains(&value_type) || name_units == 0 {
            return;
        }
        let Some(name) = read_utf16_name(data, &mut cursor, stop, name_units) else {
            return;
        };
        let value = match value_type {
            1 => {
                let Some(byte) = take_bytes(data, &mut cursor, stop, 1) else {
                    return;
                };
                Some(LvValue::Bool(byte[0] != 0))
            }
            2 => read_u32_le(data, &mut cursor, stop)
                .map(|value| LvValue::Signed(i64::from(value as i32))),
            3 => read_u32_le(data, &mut cursor, stop)
                .map(|value| LvValue::Unsigned(u64::from(value))),
            4 => read_u64_le(data, &mut cursor, stop).map(|value| LvValue::Signed(value as i64)),
            5 | 7 => read_u64_le(data, &mut cursor, stop).map(LvValue::Unsigned),
            6 => read_u64_le(data, &mut cursor, stop)
                .map(f64::from_bits)
                .map(LvValue::Float),
            8 => read_utf16_cstring(data, &mut cursor, stop).map(LvValue::Text),
            9 => {
                let Some(length) = read_u64_le(data, &mut cursor, stop) else {
                    return;
                };
                let Ok(length) = usize::try_from(length) else {
                    return;
                };
                if take_bytes(data, &mut cursor, stop, length).is_none() {
                    return;
                }
                None
            }
            10 => return,
            11 => {
                let Some(item_count) = read_u32_le(data, &mut cursor, stop) else {
                    return;
                };
                let Some(relative_end) = read_u64_le(data, &mut cursor, stop) else {
                    return;
                };
                let Ok(relative_end) = usize::try_from(relative_end) else {
                    return;
                };
                let Some(child_end) = record_start.checked_add(relative_end) else {
                    return;
                };
                if child_end < cursor || child_end > stop {
                    return;
                }
                if depth == 0 && matches!(name.as_str(), "SLxExperiment" | "RLxExperiment") {
                    let mut root_values = Vec::new();
                    parse_lv_sequence(
                        data,
                        cursor,
                        child_end,
                        depth + 1,
                        &mut root_values,
                        channel_colors,
                        acquisition_roots,
                    );
                    values.extend(root_values.iter().cloned());
                    acquisition_roots.push(root_values);
                } else {
                    parse_lv_sequence(
                        data,
                        cursor,
                        child_end,
                        depth + 1,
                        values,
                        channel_colors,
                        acquisition_roots,
                    );
                }
                let Some(index_bytes) = usize::try_from(item_count)
                    .ok()
                    .and_then(|count| count.checked_mul(8))
                else {
                    return;
                };
                let Some(after_index) = child_end.checked_add(index_bytes) else {
                    return;
                };
                if after_index > stop {
                    return;
                }
                cursor = after_index;
                None
            }
            _ => return,
        };
        if let Some(value) = value {
            if name == "uiColor" {
                current_color = value.as_u32();
            } else if name == "sDescription" {
                if let (Some(color), Some(description)) = (current_color, value.as_text()) {
                    upsert_channel_color(channel_colors, description, color);
                }
            }
            values.push((name, value));
        }
    }
}

fn upsert_channel_color(colors: &mut Vec<Nd2ChannelColor>, name: &str, color: u32) {
    if let Some(existing) = colors.iter_mut().find(|entry| entry.name == name) {
        existing.color = color;
    } else {
        colors.push(Nd2ChannelColor {
            name: name.to_owned(),
            color,
        });
    }
}

fn first_lv_u32(values: &[(String, LvValue)], names: &[&str]) -> Option<u32> {
    names.iter().find_map(|name| {
        values
            .iter()
            .find(|(candidate, _)| candidate == name)
            .and_then(|(_, value)| value.as_u32())
    })
}

fn first_lv_f64(values: &[(String, LvValue)], names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        values
            .iter()
            .find(|(candidate, _)| candidate == name)
            .and_then(|(_, value)| value.as_f64())
    })
}

fn first_lv_text(values: &[(String, LvValue)], names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        values
            .iter()
            .find(|(candidate, _)| candidate == name)
            .and_then(|(_, value)| value.as_text())
            .map(str::to_owned)
    })
}

fn collect_lv_f64(values: &[(String, LvValue)], names: &[&str]) -> Vec<f64> {
    values
        .iter()
        .filter(|(candidate, _)| names.contains(&candidate.as_str()))
        .filter_map(|(_, value)| value.as_f64())
        .collect()
}

fn collect_lv_text(values: &[(String, LvValue)], names: &[&str]) -> Vec<String> {
    values
        .iter()
        .filter(|(candidate, _)| names.contains(&candidate.as_str()))
        .filter_map(|(_, value)| value.as_text().map(str::to_owned))
        .collect()
}

fn parse_nd2_lv_metadata(values: &[(String, LvValue)]) -> Nd2MetadataModel {
    let mut metadata = Nd2MetadataModel::default();
    metadata.size_x = first_lv_u32(values, &["uiWidth", "uiCamPxlCountX"]);
    metadata.size_y = first_lv_u32(values, &["uiHeight", "uiCamPxlCountY"]);
    metadata.row_bytes = first_lv_u32(values, &["uiWidthBytes"]);
    metadata.logical_channels =
        first_lv_u32(values, &["uiComp", "ChannelCount"]).filter(|value| *value > 0);
    metadata.size_z = first_lv_u32(values, &["zCount", "uiZStackHome"]).filter(|value| *value > 0);
    metadata.size_t = first_lv_u32(values, &["timeCount", "TimeCount"]).filter(|value| *value > 0);
    metadata.series_count =
        first_lv_u32(values, &["XYCount", "SeriesCount"]).filter(|value| *value > 0);
    metadata.storage_bits = first_lv_u32(values, &["uiBpcInMemory", "uiBpc"])
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value > 0);
    metadata.significant_bits = first_lv_u32(values, &["uiBpcSignificant"])
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value > 0);
    let compression_mode = first_lv_f64(values, &["eCompression"]);
    let compression_parameter = first_lv_f64(values, &["dCompressionParam"]);
    metadata.compression = match (compression_mode, compression_parameter) {
        (Some(mode), Some(parameter)) if mode <= 0.0 && parameter >= 0.0 => {
            Some(Nd2Compression::Zlib)
        }
        (Some(_), _) => Some(Nd2Compression::Raw),
        _ => None,
    };
    metadata.acquisition_candidates = collect_lv_acquisition_candidates(values);
    if let Some(candidate) = metadata.acquisition_candidates.first() {
        metadata.acquisition_order = candidate.acquisition_order.clone();
        metadata.experiment_roots = 1;
    }

    let calibrations = collect_lv_f64(values, &["dCalibration"])
        .into_iter()
        .filter(|value| *value > 0.0)
        .collect::<Vec<_>>();
    metadata.physical_size_x_um = calibrations.first().copied();
    metadata.physical_size_y_um = calibrations.get(1).copied().or(metadata.physical_size_x_um);
    metadata.physical_size_z_um = first_lv_f64(values, &["dZStep"]).filter(|value| *value > 0.0);
    metadata.time_increment_seconds =
        first_lv_f64(values, &["TimeIncrement", "dTimeStep"]).filter(|value| *value > 0.0);
    metadata.objective_magnification =
        first_lv_f64(values, &["dObjectiveMag"]).filter(|value| *value > 0.0);
    metadata.objective_model = first_lv_text(values, &["sObjective"]);

    let names = collect_lv_text(values, &["sDescription"]);
    let excitation = collect_lv_f64(values, &["ExcitationWavelength", "ExWavelength"]);
    let emission = collect_lv_f64(values, &["EmissionWavelength", "EmWavelength"]);
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

    metadata.exposure_times_seconds = collect_lv_f64(values, &["dExposureTime"])
        .into_iter()
        .filter(|value| *value > 0.0)
        .map(|value| value / 1000.0)
        .collect();
    metadata.timepoints_seconds = collect_lv_f64(values, &["dTimeMSec", "TimeMSec", "dTime"])
        .into_iter()
        .map(|value| value / 1000.0)
        .collect();
    metadata.positions_x_um = collect_lv_f64(values, &["dPosX"]);
    metadata.positions_y_um = collect_lv_f64(values, &["dPosY"]);
    metadata.positions_z_um = collect_lv_f64(values, &["dPosZ"]);
    metadata
}

fn axis_from_experiment_type(value: u32) -> Option<Nd2Axis> {
    match value {
        1 | 8 => Some(Nd2Axis::T),
        2 => Some(Nd2Axis::Series),
        4 => Some(Nd2Axis::Z),
        _ => None,
    }
}

fn acquisition_candidates_from_counts(
    z_counts: &[u32],
    t_counts: &[u32],
    series_counts: &[u32],
    acquisition_order: &[Nd2Axis],
    source: Nd2AcquisitionSource,
) -> Vec<Nd2AcquisitionCandidate> {
    if acquisition_order.is_empty() {
        return Vec::new();
    }
    let z_counts = (!z_counts.is_empty()).then_some(z_counts).unwrap_or(&[1]);
    let t_counts = (!t_counts.is_empty()).then_some(t_counts).unwrap_or(&[1]);
    let series_counts = (!series_counts.is_empty())
        .then_some(series_counts)
        .unwrap_or(&[1]);
    let mut candidates = Vec::new();
    for &size_z in z_counts {
        for &size_t in t_counts {
            for &series_count in series_counts {
                let candidate = Nd2AcquisitionCandidate {
                    size_z,
                    size_t,
                    series_count,
                    acquisition_order: acquisition_order.to_vec(),
                    source,
                    singleton_c_after_z: false,
                };
                if !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
            }
        }
    }
    candidates
}

fn collect_lv_acquisition_candidates(values: &[(String, LvValue)]) -> Vec<Nd2AcquisitionCandidate> {
    let mut pending_axis = None;
    let mut outer_to_inner = Vec::new();
    let mut z_counts = Vec::new();
    let mut t_counts = Vec::new();
    let mut series_counts = Vec::new();
    for (name, value) in values {
        if name == "eType" {
            pending_axis = value.as_u32().and_then(axis_from_experiment_type);
            continue;
        }
        if name != "uiCount" {
            continue;
        }
        let Some(axis) = pending_axis.take() else {
            continue;
        };
        let Some(count) = value.as_u32().filter(|count| *count > 0) else {
            continue;
        };
        match axis {
            Nd2Axis::Z => extend_unique(&mut z_counts, [count]),
            Nd2Axis::T => extend_unique(&mut t_counts, [count]),
            Nd2Axis::Series => extend_unique(&mut series_counts, [count]),
        }
        if !outer_to_inner.contains(&axis) {
            outer_to_inner.push(axis);
        }
    }
    outer_to_inner.reverse();
    acquisition_candidates_from_counts(
        &z_counts,
        &t_counts,
        &series_counts,
        &outer_to_inner,
        Nd2AcquisitionSource::BinaryLv,
    )
}

fn extend_unique(target: &mut Vec<u32>, values: impl IntoIterator<Item = u32>) {
    for value in values {
        if value > 0 && !target.contains(&value) {
            target.push(value);
        }
    }
}

fn merge_channel_metadata(target: &mut Vec<ChannelMetadata>, fallback: Vec<ChannelMetadata>) {
    if target.len() < fallback.len() {
        target.resize(fallback.len(), ChannelMetadata::default());
    }
    for (index, fallback) in fallback.into_iter().enumerate() {
        let channel = &mut target[index];
        if channel.name.as_deref().is_none_or(str::is_empty) {
            channel.name = fallback.name.filter(|name| !name.is_empty());
        }
        channel.color = channel.color.or(fallback.color);
        channel.emission_wavelength_nm = channel
            .emission_wavelength_nm
            .or(fallback.emission_wavelength_nm);
        channel.excitation_wavelength_nm = channel
            .excitation_wavelength_nm
            .or(fallback.excitation_wavelength_nm);
    }
}

fn finalize_channel_colors(metadata: &mut Nd2MetadataModel) {
    let channel_count = metadata.logical_channels.unwrap_or(1).max(1) as usize;
    if metadata.channel_metadata.len() < channel_count {
        metadata
            .channel_metadata
            .resize(channel_count, ChannelMetadata::default());
    }
    let channel_colors = &metadata.channel_colors;
    for (index, channel) in metadata.channel_metadata[..channel_count]
        .iter_mut()
        .enumerate()
    {
        let named_color = channel
            .name
            .as_deref()
            .and_then(|name| channel_colors.iter().find(|entry| entry.name == name));
        let color = if named_color.is_some() {
            named_color
        } else if channel.name.as_deref().is_none_or(str::is_empty) {
            channel_colors
                .iter()
                .filter(|entry| !entry.name.is_empty())
                .nth(index)
        } else {
            None
        };
        if let Some(color) = color {
            if channel.name.as_deref().is_none_or(str::is_empty) && !color.name.is_empty() {
                channel.name = Some(color.name.clone());
            }
            channel.color = Some(color.color);
        }
    }
}

fn nd2_lookup_table(color: u32, pixel_type: PixelType) -> Option<LookupTable> {
    if color == 0 {
        return None;
    }
    let entries = match pixel_type {
        PixelType::Uint8 => 256,
        PixelType::Uint16 => 65_536,
        _ => return None,
    };
    let red_max = color & 0xff;
    let green_max = (color >> 8) & 0xff;
    let blue_max = (color >> 16) & 0xff;
    let ramp = |maximum: u32| {
        (0..entries)
            .map(|index| {
                // Match ND2Reader's double-precision calculation exactly;
                // replacing this with integer division changes a handful of
                // entries because Java truncates floating-point roundoff.
                let scale = index as f64 / 255.0;
                (maximum as f64 * scale) as u16
            })
            .collect::<Vec<_>>()
    };
    Some(LookupTable {
        red: ramp(red_max),
        green: ramp(green_max),
        blue: ramp(blue_max),
    })
}

fn merge_metadata(target: &mut Nd2MetadataModel, fallback: Nd2MetadataModel) {
    target.size_x = target.size_x.or(fallback.size_x);
    target.size_y = target.size_y.or(fallback.size_y);
    target.row_bytes = target.row_bytes.or(fallback.row_bytes);
    target.logical_channels = target.logical_channels.or(fallback.logical_channels);
    target.size_z = target.size_z.or(fallback.size_z);
    target.size_t = target.size_t.or(fallback.size_t);
    target.series_count = target.series_count.or(fallback.series_count);
    target.storage_bits = target.storage_bits.or(fallback.storage_bits);
    target.significant_bits = target.significant_bits.or(fallback.significant_bits);
    target.compression = target.compression.or(fallback.compression);
    if target.acquisition_order.is_empty() {
        target.acquisition_order = fallback.acquisition_order.clone();
    }
    target.experiment_roots = target.experiment_roots.max(fallback.experiment_roots);
    for candidate in fallback.acquisition_candidates {
        if !target.acquisition_candidates.contains(&candidate) {
            target.acquisition_candidates.push(candidate);
        }
    }
    target.physical_size_x_um = target.physical_size_x_um.or(fallback.physical_size_x_um);
    target.physical_size_y_um = target.physical_size_y_um.or(fallback.physical_size_y_um);
    target.physical_size_z_um = target.physical_size_z_um.or(fallback.physical_size_z_um);
    target.time_increment_seconds = target
        .time_increment_seconds
        .or(fallback.time_increment_seconds);
    target.objective_model = target.objective_model.take().or(fallback.objective_model);
    target.objective_magnification = target
        .objective_magnification
        .or(fallback.objective_magnification);
    merge_channel_metadata(&mut target.channel_metadata, fallback.channel_metadata);
    if target.channel_colors.is_empty() {
        target.channel_colors = fallback.channel_colors;
    }
    if target.exposure_times_seconds.is_empty() {
        target.exposure_times_seconds = fallback.exposure_times_seconds;
    }
    if target.timepoints_seconds.is_empty() {
        target.timepoints_seconds = fallback.timepoints_seconds;
    }
    if target.positions_x_um.is_empty() {
        target.positions_x_um = fallback.positions_x_um;
    }
    if target.positions_y_um.is_empty() {
        target.positions_y_um = fallback.positions_y_um;
    }
    if target.positions_z_um.is_empty() {
        target.positions_z_um = fallback.positions_z_um;
    }
}

fn apply_image_attributes(target: &mut Nd2MetadataModel, attributes: &Nd2MetadataModel) {
    target.size_x = attributes.size_x.or(target.size_x);
    target.size_y = attributes.size_y.or(target.size_y);
    target.row_bytes = attributes.row_bytes.or(target.row_bytes);
    target.logical_channels = attributes.logical_channels.or(target.logical_channels);
    target.storage_bits = attributes.storage_bits.or(target.storage_bits);
    target.significant_bits = attributes.significant_bits.or(target.significant_bits);
    target.compression = attributes.compression.or(target.compression);
}

fn first_text_value(document: &Document<'_>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        document
            .descendants()
            .find(|node| node.is_element() && node.tag_name().name() == *name)
            .and_then(|node| node.attribute("value").or_else(|| node.text()))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn collect_text_values(document: &Document<'_>, names: &[&str]) -> Vec<String> {
    document
        .descendants()
        .filter(|node| node.is_element() && names.contains(&node.tag_name().name()))
        .filter_map(|node| node.attribute("value").or_else(|| node.text()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn xml_node_value<'a, 'input>(node: Node<'a, 'input>) -> Option<&'a str> {
    node.attribute("value")
        .or_else(|| node.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_nd2_color(value: &str) -> Option<u32> {
    value
        .parse::<u32>()
        .ok()
        .or_else(|| value.parse::<i32>().ok().map(|value| value as u32))
}

fn collect_xml_channel_colors(document: &Document<'_>) -> Vec<Nd2ChannelColor> {
    let mut raw_colors = Vec::new();
    let mut dye_names = Vec::new();
    for node in document.descendants().filter(|node| node.is_element()) {
        let tag = node.tag_name().name();
        if tag.ends_with("ChannelColor") {
            let Some(channel_index) = tag.find("Channel") else {
                continue;
            };
            let Some(color) = xml_node_value(node).and_then(parse_nd2_color) else {
                continue;
            };
            let prefix = &tag[..channel_index];
            if let Some(existing) = raw_colors
                .iter_mut()
                .find(|(candidate, _)| candidate == prefix)
            {
                existing.1 = color;
            } else {
                raw_colors.push((prefix.to_owned(), color));
            }
        } else if tag.ends_with("DyeName") {
            let channel_index = tag.find("Channel").unwrap_or(0);
            if let Some(name) = xml_node_value(node) {
                let prefix = &tag[..channel_index];
                if let Some(existing) = dye_names
                    .iter_mut()
                    .find(|(candidate, _)| candidate == prefix)
                {
                    existing.1 = name.to_owned();
                } else {
                    dye_names.push((prefix.to_owned(), name.to_owned()));
                }
            }
        }
    }

    let mut colors = Vec::new();
    for (prefix, color) in raw_colors {
        let name = dye_names
            .iter()
            .find(|(candidate, _)| candidate == &prefix)
            .map(|(_, name)| name.as_str())
            .unwrap_or(&prefix);
        upsert_channel_color(&mut colors, name, color);
    }
    colors
}

fn loop_count(document: &Document<'_>, runtime_suffixes: &[&str]) -> Option<u32> {
    loop_counts(document, runtime_suffixes).into_iter().next()
}

fn loop_counts(document: &Document<'_>, runtime_suffixes: &[&str]) -> Vec<u32> {
    document
        .descendants()
        .filter_map(|node| {
            let runtime = node.attribute("runtype")?;
            if !runtime_suffixes
                .iter()
                .any(|suffix| runtime.ends_with(suffix))
            {
                return None;
            }
            node.children()
                .find(|child| child.is_element() && child.tag_name().name() == "uiCount")
                .and_then(|child| child.attribute("value").or_else(|| child.text()))
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value > 0)
        })
        .collect()
}

fn direct_child_value<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
        .and_then(|child| child.attribute("value").or_else(|| child.text()))
}

fn axis_from_runtime(runtime: &str) -> Option<Nd2Axis> {
    if runtime.ends_with("ZStackLoop") {
        Some(Nd2Axis::Z)
    } else if runtime.ends_with("NETimeLoop") || runtime.ends_with("TimeLoop") {
        Some(Nd2Axis::T)
    } else if runtime.ends_with("XYPosLoop") {
        Some(Nd2Axis::Series)
    } else {
        None
    }
}

fn experiment_axis(experiment: Node<'_, '_>) -> Option<Nd2Axis> {
    if let Some(axis) = direct_child_value(experiment, "eType")
        .and_then(|value| value.parse::<u32>().ok())
        .and_then(axis_from_experiment_type)
    {
        return Some(axis);
    }

    let loop_parameters = experiment
        .children()
        .find(|child| child.is_element() && child.tag_name().name() == "uLoopPars")?;
    loop_parameters
        .descendants()
        .find_map(|node| node.attribute("runtype").and_then(axis_from_runtime))
}

fn experiment_axis_counts(experiment: Node<'_, '_>, axis: Nd2Axis) -> Vec<u32> {
    let Some(loop_parameters) = experiment
        .children()
        .find(|child| child.is_element() && child.tag_name().name() == "uLoopPars")
    else {
        return Vec::new();
    };
    let mut counts = Vec::new();
    for count in loop_parameters.descendants().filter_map(|node| {
        if node.attribute("runtype").and_then(axis_from_runtime) != Some(axis) {
            return None;
        }
        node.children()
            .find(|child| child.is_element() && child.tag_name().name() == "uiCount")
            .and_then(|child| child.attribute("value").or_else(|| child.text()))
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
    }) {
        extend_unique(&mut counts, [count]);
    }
    counts
}

fn next_experiment<'a, 'input>(experiment: Node<'a, 'input>) -> Option<Node<'a, 'input>> {
    experiment
        .children()
        .find(|child| child.is_element() && child.tag_name().name() == "ppNextLevelEx")?
        .descendants()
        .find(|node| node.is_element() && node.attribute("runtype") == Some("RLxExperiment"))
}

fn acquisition_candidates_for_experiment_root(
    mut experiment: Node<'_, '_>,
) -> Vec<Nd2AcquisitionCandidate> {
    let mut outer_to_inner = Vec::new();
    let mut z_counts = Vec::new();
    let mut t_counts = Vec::new();
    let mut series_counts = Vec::new();
    for _ in 0..16 {
        if let Some(axis) = experiment_axis(experiment) {
            if !outer_to_inner.contains(&axis) {
                outer_to_inner.push(axis);
            }
            let counts = experiment_axis_counts(experiment, axis);
            match axis {
                Nd2Axis::Z => extend_unique(&mut z_counts, counts),
                Nd2Axis::T => extend_unique(&mut t_counts, counts),
                Nd2Axis::Series => extend_unique(&mut series_counts, counts),
            }
        }
        let Some(next) = next_experiment(experiment) else {
            break;
        };
        experiment = next;
    }
    outer_to_inner.reverse();
    acquisition_candidates_from_counts(
        &z_counts,
        &t_counts,
        &series_counts,
        &outer_to_inner,
        Nd2AcquisitionSource::Xml,
    )
}

fn collect_xml_acquisition_candidates(
    document: &Document<'_>,
) -> (u32, Vec<Nd2AcquisitionCandidate>) {
    let roots = document
        .descendants()
        .filter(|node| {
            node.is_element()
                && node.attribute("runtype") == Some("RLxExperiment")
                && !node.ancestors().skip(1).any(|ancestor| {
                    ancestor.is_element() && ancestor.attribute("runtype") == Some("RLxExperiment")
                })
        })
        .collect::<Vec<_>>();
    let root_count = u32::try_from(roots.len()).unwrap_or(u32::MAX);
    let mut candidates = Vec::new();
    for root in roots {
        for candidate in acquisition_candidates_for_experiment_root(root) {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    if root_count > 1 {
        for candidate in &mut candidates {
            candidate.singleton_c_after_z = true;
        }
    }
    (root_count, candidates)
}

fn parse_nd2_text_metadata(fragments: &[String]) -> Nd2MetadataModel {
    let xml = fragments
        .iter()
        .filter(|fragment| fragment.contains('<'))
        .map(|fragment| {
            let fragment = fragment.trim();
            fragment
                .strip_prefix("<?xml")
                .and_then(|rest| rest.find("?>").map(|end| &rest[end + 2..]))
                .unwrap_or(fragment)
        })
        .collect::<String>();
    if xml.trim().is_empty() {
        return Nd2MetadataModel::default();
    }
    let wrapped = format!("<ND2>{xml}</ND2>");
    let Ok(document) = Document::parse(&wrapped) else {
        return Nd2MetadataModel::default();
    };

    let mut metadata = Nd2MetadataModel::default();
    metadata.channel_colors = collect_xml_channel_colors(&document);
    let (experiment_roots, acquisition_candidates) = collect_xml_acquisition_candidates(&document);
    metadata.experiment_roots = experiment_roots;
    metadata.acquisition_order = acquisition_candidates
        .first()
        .map(|candidate| candidate.acquisition_order.clone())
        .unwrap_or_default();
    metadata.acquisition_candidates = acquisition_candidates;
    metadata.size_x = first_text_value(&document, &["uiWidth", "uiCamPxlCountX"])
        .and_then(|value| value.parse::<u32>().ok());
    metadata.size_y = first_text_value(&document, &["uiHeight", "uiCamPxlCountY"])
        .and_then(|value| value.parse::<u32>().ok());
    metadata.row_bytes =
        first_text_value(&document, &["uiWidthBytes"]).and_then(|value| value.parse::<u32>().ok());
    metadata.logical_channels = first_text_value(&document, &["ChannelCount", "uiComp"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0);
    metadata.size_z = first_text_value(&document, &["zCount", "uiZStackHome"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .or_else(|| loop_count(&document, &["ZStackLoop"]));
    metadata.size_t = first_text_value(&document, &["timeCount", "TimeCount"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .or_else(|| loop_count(&document, &["NETimeLoop", "TimeLoop"]));
    metadata.series_count = first_text_value(&document, &["XYCount", "SeriesCount"])
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .or_else(|| loop_count(&document, &["XYPosLoop"]));
    metadata.storage_bits = first_text_value(&document, &["uiBpcInMemory", "uiBpc"])
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value > 0);
    metadata.significant_bits = first_text_value(&document, &["uiBpcSignificant"])
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value > 0);
    let compression_mode =
        first_text_value(&document, &["eCompression"]).and_then(|value| value.parse::<f64>().ok());
    let compression_parameter = first_text_value(&document, &["dCompressionParam"])
        .and_then(|value| value.parse::<f64>().ok());
    metadata.compression = match (compression_mode, compression_parameter) {
        (Some(mode), Some(parameter)) if mode <= 0.0 && parameter >= 0.0 => {
            Some(Nd2Compression::Zlib)
        }
        (Some(_), _) => Some(Nd2Compression::Raw),
        _ => None,
    };

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

    metadata
}

fn acquisition_candidate_plane_count(candidate: &Nd2AcquisitionCandidate) -> u64 {
    u64::from(candidate.size_z) * u64::from(candidate.size_t) * u64::from(candidate.series_count)
}

/// Resolve duplicated or planned acquisition metadata atomically from one
/// experiment root. Counts from separate roots are never combined, and the
/// selected root's axis order travels with its dimensions.
fn reconcile_acquisition_dimensions(
    metadata: &mut Nd2MetadataModel,
    physical_count: usize,
) -> Result<()> {
    let physical_count =
        u32::try_from(physical_count).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    if physical_count == 0 {
        return Ok(());
    }
    if !metadata.acquisition_candidates.is_empty() {
        for source in [Nd2AcquisitionSource::BinaryLv, Nd2AcquisitionSource::Xml] {
            let mut matches = Vec::new();
            for candidate in &metadata.acquisition_candidates {
                if candidate.source == source
                    && acquisition_candidate_plane_count(candidate) == u64::from(physical_count)
                    && !matches.contains(candidate)
                {
                    matches.push(candidate.clone());
                }
            }
            let selected = match matches.as_slice() {
                [] => continue,
                [candidate] => candidate,
                _ => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "ND2: multiple {source:?} acquisition roots account for {physical_count} physical planes with conflicting dimensions or axis order"
                    )))
                }
            };
            metadata.size_z = Some(selected.size_z);
            metadata.size_t = Some(selected.size_t);
            metadata.series_count = Some(selected.series_count);
            metadata.acquisition_order = selected.acquisition_order.clone();
            metadata.singleton_c_after_z = selected.singleton_c_after_z;
            return Ok(());
        }
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "ND2: no complete acquisition root accounts for {physical_count} physical planes"
        )));
    }

    let current = u64::from(metadata.size_z.unwrap_or(1).max(1))
        * u64::from(metadata.size_t.unwrap_or(1).max(1))
        * u64::from(metadata.series_count.unwrap_or(1).max(1));
    if current == u64::from(physical_count) {
        Ok(())
    } else {
        Err(BioFormatsError::UnsupportedFormat(format!(
            "ND2: metadata describes {current} physical planes but the file contains {physical_count}"
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Nd2Plane {
    chunk_index: usize,
    component: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nd2Series {
    metadata: ImageMetadata,
    planes: Vec<Nd2Plane>,
    stored_components: u32,
    row_bytes: usize,
    compression: Option<Nd2Compression>,
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

fn checked_plane_size(
    width: u32,
    height: u32,
    samples: u32,
    bytes_per_sample: usize,
) -> Result<usize> {
    usize::try_from(width)
        .ok()
        .and_then(|value| value.checked_mul(height as usize))
        .and_then(|value| value.checked_mul(samples as usize))
        .and_then(|value| value.checked_mul(bytes_per_sample))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)
}

fn copy_plane_bytes(data: &[u8], context: &str) -> Result<Vec<u8>> {
    if data.len() > isize::MAX as usize {
        return Err(BioFormatsError::PlaneByteCountOverflow);
    }
    let mut copy = Vec::new();
    copy.try_reserve_exact(data.len()).map_err(|error| {
        BioFormatsError::InvalidData(format!("ND2: cannot allocate {context}: {error}"))
    })?;
    copy.extend_from_slice(data);
    Ok(copy)
}

fn is_zlib_stream(data: &[u8]) -> bool {
    if data.len() < 2 {
        return false;
    }
    let cmf = data[0];
    let flg = data[1];
    cmf & 0x0f == 8 && cmf >> 4 <= 7 && (u16::from(cmf) * 256 + u16::from(flg)) % 31 == 0
}

fn decompress_zlib_limited(data: &[u8], maximum_length: usize) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;

    let limit = maximum_length
        .checked_add(1)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let mut output = Vec::new();
    output.try_reserve_exact(limit).map_err(|error| {
        BioFormatsError::InvalidData(format!("ND2: cannot allocate decompressed plane: {error}"))
    })?;
    ZlibDecoder::new(data)
        .take(limit as u64)
        .read_to_end(&mut output)
        .map_err(|error| {
            BioFormatsError::Codec(format!("ND2 zlib decompression failed: {error}"))
        })?;
    if output.len() > maximum_length {
        return Err(BioFormatsError::InvalidData(format!(
            "ND2: decompressed plane exceeds {maximum_length} bytes"
        )));
    }
    Ok(output)
}

fn decode_physical_plane(data: &[u8], series: &Nd2Series) -> Result<Vec<u8>> {
    let payload = data.get(8..).ok_or_else(|| {
        BioFormatsError::Format("ND2: ImageDataSeq payload is missing its 8-byte prefix".into())
    })?;
    if detect_jpeg2000(payload) {
        return Err(BioFormatsError::UnsupportedFormat(
            "ND2: JPEG2000 compression not yet supported".into(),
        ));
    }

    let height = usize::try_from(series.metadata.size_y)
        .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    let padded_length = series
        .row_bytes
        .checked_mul(height)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let tight_row_bytes = usize::try_from(series.metadata.size_x)
        .ok()
        .and_then(|width| width.checked_mul(series.stored_components as usize))
        .and_then(|samples| samples.checked_mul(series.metadata.pixel_type.bytes_per_sample()))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let tight_length = tight_row_bytes
        .checked_mul(height)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let maximum_length = padded_length.max(tight_length);

    let compressed = series.compression == Some(Nd2Compression::Zlib) || is_zlib_stream(payload);
    let decoded = if compressed {
        decompress_zlib_limited(payload, maximum_length)?
    } else {
        if payload.len() != tight_length && payload.len() != padded_length {
            return Err(BioFormatsError::Format(format!(
                "ND2: raw physical plane has {} bytes; expected {tight_length} or {padded_length}",
                payload.len()
            )));
        }
        copy_plane_bytes(payload, "raw physical plane")?
    };
    let decoded_row_bytes = if decoded.len() == padded_length {
        series.row_bytes
    } else if decoded.len() == tight_length {
        tight_row_bytes
    } else {
        return Err(BioFormatsError::Format(format!(
            "ND2: decoded physical plane has {} bytes; expected {tight_length} or {padded_length}",
            decoded.len()
        )));
    };

    if decoded_row_bytes == tight_row_bytes {
        return Ok(decoded);
    }

    let mut tight = Vec::new();
    tight.try_reserve_exact(tight_length).map_err(|error| {
        BioFormatsError::InvalidData(format!("ND2: cannot allocate unpadded plane: {error}"))
    })?;
    for row in decoded.chunks_exact(decoded_row_bytes) {
        tight.extend_from_slice(&row[..tight_row_bytes]);
    }
    Ok(tight)
}

fn extract_component(data: &[u8], series: &Nd2Series, component: u32) -> Result<Vec<u8>> {
    if component >= series.stored_components {
        return Err(BioFormatsError::Format(format!(
            "ND2: component {component} is outside {} stored components",
            series.stored_components
        )));
    }
    let bytes_per_sample = series.metadata.pixel_type.bytes_per_sample();
    let expected_input_length = checked_plane_size(
        series.metadata.size_x,
        series.metadata.size_y,
        series.stored_components,
        bytes_per_sample,
    )?;
    if data.len() != expected_input_length {
        return Err(BioFormatsError::Format(format!(
            "ND2: physical plane has {} bytes; expected {expected_input_length}",
            data.len()
        )));
    }
    if series.stored_components == 1 {
        return copy_plane_bytes(data, "single-component plane");
    }

    let pixel_stride = (series.stored_components as usize)
        .checked_mul(bytes_per_sample)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    if !data.len().is_multiple_of(pixel_stride) {
        return Err(BioFormatsError::Format(
            "ND2: physical plane does not contain whole interleaved pixels".into(),
        ));
    }
    let output_length = checked_plane_size(
        series.metadata.size_x,
        series.metadata.size_y,
        1,
        bytes_per_sample,
    )?;
    let mut output = Vec::new();
    output.try_reserve_exact(output_length).map_err(|error| {
        BioFormatsError::InvalidData(format!("ND2: cannot allocate channel plane: {error}"))
    })?;
    let component_start = component as usize * bytes_per_sample;
    for pixel in data.chunks_exact(pixel_stride) {
        output.extend_from_slice(&pixel[component_start..component_start + bytes_per_sample]);
    }
    Ok(output)
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
        Self::collect_metadata_fragments_where(file, chunks, |_| true)
    }

    fn collect_metadata_fragments_where(
        file: &mut BufReader<File>,
        chunks: &[Nd2Chunk],
        predicate: impl Fn(&Nd2Chunk) -> bool,
    ) -> Vec<String> {
        chunks
            .iter()
            .filter(|chunk| is_textual_metadata_chunk(&chunk.name))
            .filter(|chunk| chunk.data_length <= MAX_METADATA_CHUNK_BYTES)
            .filter(|chunk| predicate(chunk))
            .filter_map(|chunk| read_chunk_data(file, chunk).ok())
            .filter(|data| looks_like_xml(data))
            .filter_map(|data| String::from_utf8(data).ok())
            .collect()
    }

    fn collect_lv_values(
        file: &mut BufReader<File>,
        chunks: &[Nd2Chunk],
    ) -> (
        Vec<(String, LvValue)>,
        Vec<Nd2ChannelColor>,
        Vec<Nd2AcquisitionCandidate>,
        u32,
    ) {
        Self::collect_lv_values_where(file, chunks, |_| true)
    }

    fn collect_lv_values_where(
        file: &mut BufReader<File>,
        chunks: &[Nd2Chunk],
        predicate: impl Fn(&Nd2Chunk) -> bool,
    ) -> (
        Vec<(String, LvValue)>,
        Vec<Nd2ChannelColor>,
        Vec<Nd2AcquisitionCandidate>,
        u32,
    ) {
        let mut values = Vec::new();
        let mut channel_colors = Vec::new();
        let mut acquisition_candidates = Vec::new();
        let mut experiment_roots = 0_u32;
        for chunk in chunks
            .iter()
            .filter(|chunk| is_textual_metadata_chunk(&chunk.name))
            .filter(|chunk| chunk.data_length <= MAX_METADATA_CHUNK_BYTES)
            .filter(|chunk| predicate(chunk))
        {
            let Ok(data) = read_chunk_data(file, chunk) else {
                continue;
            };
            let stop = data.len();
            let mut chunk_values = Vec::new();
            let mut chunk_roots = Vec::new();
            parse_lv_sequence(
                &data,
                0,
                stop,
                0,
                &mut chunk_values,
                &mut channel_colors,
                &mut chunk_roots,
            );
            if chunk.name.starts_with("ImageMetadataLV") {
                if chunk_roots.is_empty() {
                    chunk_roots.push(chunk_values.clone());
                }
                for root_values in chunk_roots {
                    let root_candidates = collect_lv_acquisition_candidates(&root_values);
                    if !root_candidates.is_empty() {
                        experiment_roots = experiment_roots.saturating_add(1);
                    }
                    for candidate in root_candidates {
                        if !acquisition_candidates.contains(&candidate) {
                            acquisition_candidates.push(candidate);
                        }
                    }
                }
            }
            values.extend(chunk_values);
        }
        if experiment_roots > 1 {
            for candidate in &mut acquisition_candidates {
                candidate.singleton_c_after_z = true;
            }
        }
        (
            values,
            channel_colors,
            acquisition_candidates,
            experiment_roots,
        )
    }

    fn build_series(model: &Nd2MetadataModel, image_chunks: &[usize]) -> Result<Vec<Nd2Series>> {
        let series_count = model.series_count.unwrap_or(1).max(1);
        let logical_channels = model.logical_channels.unwrap_or(1).max(1);
        let size_x = model
            .size_x
            .filter(|value| *value > 0)
            .ok_or_else(|| BioFormatsError::Format("ND2: missing or zero image width".into()))?;
        let size_y = model
            .size_y
            .filter(|value| *value > 0)
            .ok_or_else(|| BioFormatsError::Format("ND2: missing or zero image height".into()))?;
        let storage_bits = model.storage_bits.ok_or_else(|| {
            BioFormatsError::Format("ND2: missing uiBpcInMemory pixel storage width".into())
        })?;
        let pixel_type = match storage_bits {
            8 => PixelType::Uint8,
            16 => PixelType::Uint16,
            32 => PixelType::Uint32,
            bits => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "ND2: unsupported {bits}-bit pixel storage"
                )))
            }
        };
        let bits_per_pixel = model.significant_bits.unwrap_or(storage_bits);
        if bits_per_pixel == 0 || bits_per_pixel > storage_bits {
            return Err(BioFormatsError::Format(format!(
                "ND2: invalid significant bit count {bits_per_pixel} for {storage_bits}-bit storage"
            )));
        }
        let bytes_per_sample = pixel_type.bytes_per_sample();
        let size_z = model.size_z.unwrap_or(1).max(1);
        let size_t = model.size_t.unwrap_or(1).max(1);
        let physical_plane_count = size_z
            .checked_mul(size_t)
            .and_then(|count| count.checked_mul(series_count))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let physical_plane_count = usize::try_from(physical_plane_count)
            .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        if image_chunks.len() != physical_plane_count {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "ND2: acquisition loop describes {physical_plane_count} physical planes but the file contains {} ImageDataSeq chunks",
                image_chunks.len()
            )));
        }

        let mut acquisition_order = model.acquisition_order.clone();
        acquisition_order.retain(|axis| match axis {
            Nd2Axis::Z => size_z > 1,
            Nd2Axis::T => size_t > 1,
            Nd2Axis::Series => series_count > 1,
        });
        let varying_axes = [
            (Nd2Axis::Z, size_z),
            (Nd2Axis::T, size_t),
            (Nd2Axis::Series, series_count),
        ]
        .into_iter()
        .filter_map(|(axis, length)| (length > 1).then_some(axis))
        .collect::<Vec<_>>();
        for axis in &varying_axes {
            if !acquisition_order.contains(axis) {
                if varying_axes.len() == 1 {
                    acquisition_order.push(*axis);
                } else {
                    return Err(BioFormatsError::UnsupportedFormat(
                        "ND2: acquisition loop order is missing for a multidimensional dataset"
                            .into(),
                    ));
                }
            }
        }

        let tight_row_bytes = usize::try_from(size_x)
            .ok()
            .and_then(|width| width.checked_mul(logical_channels as usize))
            .and_then(|samples| samples.checked_mul(bytes_per_sample))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let row_bytes = model
            .row_bytes
            .map(|value| {
                usize::try_from(value).map_err(|_| BioFormatsError::PlaneByteCountOverflow)
            })
            .transpose()?
            .unwrap_or(tight_row_bytes);
        if row_bytes < tight_row_bytes {
            return Err(BioFormatsError::Format(format!(
                "ND2: uiWidthBytes {row_bytes} is smaller than the {tight_row_bytes}-byte stored pixel row"
            )));
        }
        row_bytes
            .checked_mul(size_y as usize)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;

        let image_count = size_z
            .checked_mul(size_t)
            .and_then(|count| count.checked_mul(logical_channels))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let z_position = acquisition_order
            .iter()
            .position(|axis| *axis == Nd2Axis::Z);
        let t_position = acquisition_order
            .iter()
            .position(|axis| *axis == Nd2Axis::T);
        let time_before_z = matches!((t_position, z_position), (Some(t), Some(z)) if t < z);
        let dimension_order = if time_before_z {
            DimensionOrder::XYCTZ
        } else if model.singleton_c_after_z && size_z > 1 && size_t > 1 && logical_channels == 1 {
            // Bio-Formats' repeated preliminary/final experiment path inserts
            // the singleton C axis after Z rather than before it.
            DimensionOrder::XYZCT
        } else if logical_channels > 1 || size_t > 1 {
            DimensionOrder::XYCZT
        } else {
            // Bio-Formats' ND2 default inserts the missing axes as Z, C, T.
            DimensionOrder::XYZCT
        };

        let image_count_usize =
            usize::try_from(image_count).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        let is_indexed = !model.channel_colors.is_empty()
            && model
                .channel_colors
                .iter()
                .any(|entry| entry.color != 0 && entry.color != 0x00ff_ffff);
        // ImageMetadata currently has one LUT slot rather than Java's
        // last-opened-channel lookup. Populate it only where there is one
        // logical channel, for which the two models are equivalent.
        let lookup_table = (is_indexed && logical_channels == 1)
            .then(|| {
                model
                    .channel_metadata
                    .first()
                    .and_then(|channel| channel.color)
                    .and_then(|color| nd2_lookup_table(color, pixel_type))
            })
            .flatten();
        let series_count_usize =
            usize::try_from(series_count).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        let mut plane_maps = Vec::new();
        plane_maps
            .try_reserve_exact(series_count_usize)
            .map_err(|error| {
                BioFormatsError::InvalidData(format!("ND2: cannot allocate series map: {error}"))
            })?;
        for _ in 0..series_count_usize {
            let mut map = Vec::new();
            map.try_reserve_exact(image_count_usize).map_err(|error| {
                BioFormatsError::InvalidData(format!(
                    "ND2: cannot allocate logical plane map: {error}"
                ))
            })?;
            map.resize(image_count_usize, None);
            plane_maps.push(map);
        }

        for (sequence, chunk_index) in image_chunks.iter().enumerate() {
            let mut remaining = sequence;
            let mut z = 0_u32;
            let mut t = 0_u32;
            let mut series_index = 0_u32;
            for axis in &acquisition_order {
                let length = match axis {
                    Nd2Axis::Z => size_z,
                    Nd2Axis::T => size_t,
                    Nd2Axis::Series => series_count,
                } as usize;
                let coordinate = remaining % length;
                remaining /= length;
                match axis {
                    Nd2Axis::Z => z = coordinate as u32,
                    Nd2Axis::T => t = coordinate as u32,
                    Nd2Axis::Series => series_index = coordinate as u32,
                }
            }
            if remaining != 0 {
                return Err(BioFormatsError::Format(
                    "ND2: image sequence index exceeds acquisition loop dimensions".into(),
                ));
            }
            let series_map = plane_maps
                .get_mut(series_index as usize)
                .ok_or_else(|| BioFormatsError::Format("ND2: invalid series coordinate".into()))?;
            for component in 0..logical_channels {
                let plane_position = if time_before_z {
                    size_t
                        .checked_mul(z)
                        .and_then(|offset| offset.checked_add(t))
                } else {
                    size_z
                        .checked_mul(t)
                        .and_then(|offset| offset.checked_add(z))
                }
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let logical_index = logical_channels
                    .checked_mul(plane_position)
                    .and_then(|offset| offset.checked_add(component))
                    .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
                let slot = series_map.get_mut(logical_index as usize).ok_or_else(|| {
                    BioFormatsError::Format("ND2: invalid logical plane index".into())
                })?;
                if slot.is_some() {
                    return Err(BioFormatsError::Format(
                        "ND2: acquisition loop maps multiple chunks to one plane".into(),
                    ));
                }
                *slot = Some(Nd2Plane {
                    chunk_index: *chunk_index,
                    component,
                });
            }
        }

        let mut series = Vec::new();
        series
            .try_reserve_exact(series_count_usize)
            .map_err(|error| {
                BioFormatsError::InvalidData(format!(
                    "ND2: cannot allocate series metadata: {error}"
                ))
            })?;
        for (series_index, plane_map) in plane_maps.into_iter().enumerate() {
            let planes = plane_map
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    BioFormatsError::Format(
                        "ND2: acquisition loop left one or more logical planes unmapped".into(),
                    )
                })?;
            let mut metadata = ImageMetadata {
                size_x,
                size_y,
                size_z,
                size_c: logical_channels,
                size_t,
                pixel_type,
                bits_per_pixel,
                samples_per_pixel: 1,
                image_count,
                dimension_order,
                is_rgb: false,
                is_interleaved: false,
                is_indexed,
                is_false_color: true,
                is_little_endian: true,
                resolution_count: 1,
                series_metadata: HashMap::new(),
                lookup_table: lookup_table.clone(),
                physical_size_x_um: model.physical_size_x_um,
                physical_size_y_um: model.physical_size_y_um,
                physical_size_z_um: model.physical_size_z_um,
                time_increment_seconds: model.time_increment_seconds,
                acquisition_timestamp: None,
                objective_model: model.objective_model.clone(),
                objective_magnification: model.objective_magnification,
                objective_na: None,
                channel_metadata: if model.channel_metadata.len() >= logical_channels as usize {
                    model.channel_metadata[..logical_channels as usize].to_vec()
                } else {
                    model.channel_metadata.clone()
                },
                plane_metadata: Vec::new(),
                used_files: Vec::new(),
            };
            metadata.series_metadata.insert(
                "nd2_chunks".into(),
                MetadataValue::Int((planes.len() / logical_channels as usize) as i64),
            );
            metadata.series_metadata.insert(
                "nd2_series_index".into(),
                MetadataValue::Int(series_index as i64),
            );
            metadata.plane_metadata = (0..image_count)
                .map(|plane_index| {
                    let (z, c, t) = metadata.get_zct_coords(plane_index);
                    let physical_index = plane_index as usize / logical_channels as usize;
                    PlaneMetadata {
                        z,
                        c,
                        t,
                        delta_t_seconds: model
                            .timepoints_seconds
                            .get(physical_index)
                            .copied()
                            .or_else(|| {
                                metadata.time_increment_seconds.map(|step| step * t as f64)
                            }),
                        position_x_um: model.positions_x_um.get(series_index).copied(),
                        position_y_um: model.positions_y_um.get(series_index).copied(),
                        position_z_um: model
                            .positions_z_um
                            .get(physical_index)
                            .copied()
                            .or_else(|| model.positions_z_um.get(series_index).copied()),
                    }
                })
                .collect();
            series.push(Nd2Series {
                metadata,
                planes,
                stored_components: logical_channels,
                row_bytes,
                compression: model.compression,
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
        let mut numbered_image_chunks = chunks
            .iter()
            .enumerate()
            .filter_map(|(index, chunk)| {
                image_sequence_number(&chunk.name).map(|sequence| (sequence, index))
            })
            .collect::<Vec<_>>();
        if numbered_image_chunks.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2: no modern ImageDataSeq chunks were found".into(),
            ));
        }
        numbered_image_chunks.sort_unstable_by_key(|(sequence, _)| *sequence);
        let first_sequence = numbered_image_chunks[0].0;
        if first_sequence > 1
            || numbered_image_chunks
                .windows(2)
                .any(|pair| pair[1].0 != pair[0].0 + 1)
        {
            return Err(BioFormatsError::Format(
                "ND2: ImageDataSeq chunk numbers are not contiguous".into(),
            ));
        }
        let image_chunks = numbered_image_chunks
            .into_iter()
            .map(|(_, chunk_index)| chunk_index)
            .collect::<Vec<_>>();

        let fragments = Self::collect_metadata_fragments(&mut reader, &chunks);
        let mut metadata = parse_nd2_text_metadata(&fragments);
        let attribute_fragments =
            Self::collect_metadata_fragments_where(&mut reader, &chunks, |chunk| {
                chunk.name.starts_with("ImageAttributes")
            });
        let text_attributes = parse_nd2_text_metadata(&attribute_fragments);
        apply_image_attributes(&mut metadata, &text_attributes);
        let (lv_values, lv_channel_colors, lv_candidates, lv_experiment_roots) =
            Self::collect_lv_values(&mut reader, &chunks);
        let mut lv_metadata = parse_nd2_lv_metadata(&lv_values);
        lv_metadata.channel_colors = lv_channel_colors;
        lv_metadata.acquisition_order = lv_candidates
            .first()
            .map(|candidate| candidate.acquisition_order.clone())
            .unwrap_or_default();
        lv_metadata.acquisition_candidates = lv_candidates;
        lv_metadata.experiment_roots = lv_experiment_roots;
        merge_metadata(&mut metadata, lv_metadata);
        let (attribute_values, _, _, _) =
            Self::collect_lv_values_where(&mut reader, &chunks, |chunk| {
                chunk.name.starts_with("ImageAttributesLV")
            });
        let attributes = parse_nd2_lv_metadata(&attribute_values);
        apply_image_attributes(&mut metadata, &attributes);
        reconcile_acquisition_dimensions(&mut metadata, image_chunks.len())?;
        finalize_channel_colors(&mut metadata);

        if metadata.size_x.unwrap_or(0) == 0 || metadata.size_y.unwrap_or(0) == 0 {
            return Err(BioFormatsError::Format(
                "ND2: missing or zero image dimensions in ImageAttributesLV".into(),
            ));
        }

        let series = Self::build_series(&metadata, &image_chunks)?;
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
        let plane = series
            .planes
            .get(plane_index as usize)
            .cloned()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let chunk = self
            .chunks
            .get(plane.chunk_index)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let data = read_chunk_data(file, chunk).map_err(BioFormatsError::Io)?;
        let physical = decode_physical_plane(&data, &series)?;
        extract_component(&physical, &series, plane.component)
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
        let bytes_per_sample = series.metadata.pixel_type.bytes_per_sample();
        let row_bytes = usize::try_from(series.metadata.size_x)
            .ok()
            .and_then(|width| width.checked_mul(bytes_per_sample))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let out_row = usize::try_from(w)
            .ok()
            .and_then(|width| width.checked_mul(bytes_per_sample))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let output_length = usize::try_from(h)
            .ok()
            .and_then(|height| height.checked_mul(out_row))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let mut out = Vec::new();
        out.try_reserve_exact(output_length).map_err(|error| {
            BioFormatsError::InvalidData(format!("ND2: cannot allocate image region: {error}"))
        })?;
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let start = x as usize * bytes_per_sample;
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
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn lv_record_header(value_type: u8, name: &str) -> Vec<u8> {
        let name = name.encode_utf16().collect::<Vec<_>>();
        let mut record = vec![value_type, u8::try_from(name.len()).unwrap()];
        for unit in name {
            record.extend_from_slice(&unit.to_le_bytes());
        }
        record
    }

    fn lv_u32(name: &str, value: u32) -> Vec<u8> {
        let mut record = lv_record_header(3, name);
        record.extend_from_slice(&value.to_le_bytes());
        record
    }

    fn lv_text(name: &str, value: &str) -> Vec<u8> {
        let mut record = lv_record_header(8, name);
        for unit in value.encode_utf16().chain([0]) {
            record.extend_from_slice(&unit.to_le_bytes());
        }
        record
    }

    fn lv_level(name: &str, children: &[Vec<u8>]) -> Vec<u8> {
        let mut record = lv_record_header(11, name);
        record.extend_from_slice(&0_u32.to_le_bytes());
        let end_offset_position = record.len();
        record.extend_from_slice(&0_u64.to_le_bytes());
        for child in children {
            record.extend_from_slice(child);
        }
        let relative_end = u64::try_from(record.len()).unwrap();
        record[end_offset_position..end_offset_position + 8]
            .copy_from_slice(&relative_end.to_le_bytes());
        record
    }

    fn acquisition_root_xml(
        outer_axis: Nd2Axis,
        outer_count: u32,
        inner_axis: Nd2Axis,
        inner_count: u32,
    ) -> String {
        fn axis_xml(axis: Nd2Axis) -> (u32, &'static str) {
            match axis {
                Nd2Axis::Z => (4, "RLxExperiment.RLxExpZStackLoop"),
                Nd2Axis::T => (1, "RLxExperiment.RLxExpTimeLoop"),
                Nd2Axis::Series => (2, "RLxExperiment.RLxExpXYPosLoop"),
            }
        }

        let (outer_type, outer_runtime) = axis_xml(outer_axis);
        let (inner_type, inner_runtime) = axis_xml(inner_axis);
        format!(
            r#"<no_name runtype="RLxExperiment">
  <eType value="{outer_type}"/>
  <uLoopPars><no_name runtype="{outer_runtime}"><uiCount value="{outer_count}"/></no_name></uLoopPars>
  <ppNextLevelEx><no_name runtype="RLxExperiment">
    <eType value="{inner_type}"/>
    <uLoopPars><no_name runtype="{inner_runtime}"><uiCount value="{inner_count}"/></no_name></uLoopPars>
  </no_name></ppNextLevelEx>
</no_name>"#
        )
    }

    fn parse_acquisition_roots(roots: &[String]) -> Nd2MetadataModel {
        parse_nd2_text_metadata(&[format!("<variant>{}</variant>", roots.concat())])
    }

    #[test]
    fn pairs_binary_lv_channel_colors_only_within_their_level() {
        let data = [
            lv_level("color_only", &[lv_u32("uiColor", 0x0000_00ff)]),
            lv_level(
                "description_only",
                &[lv_text("sDescription", "must not inherit red")],
            ),
            lv_level(
                "paired",
                &[
                    lv_u32("uiColor", 0x00ff_1e00),
                    lv_text("sDescription", "405/488/561/633nm"),
                ],
            ),
        ]
        .concat();
        let mut values = Vec::new();
        let mut colors = Vec::new();
        let mut acquisition_roots = Vec::new();

        parse_lv_sequence(
            &data,
            0,
            data.len(),
            0,
            &mut values,
            &mut colors,
            &mut acquisition_roots,
        );

        assert_eq!(
            colors,
            [Nd2ChannelColor {
                name: "405/488/561/633nm".to_owned(),
                color: 0x00ff_1e00,
            }]
        );
    }

    #[test]
    fn pairs_xml_channel_colors_with_their_dye_names() {
        let metadata = parse_nd2_text_metadata(&[r#"
<Metadata>
  <a0ChannelColor value="255"/>
  <a0ChannelColor value="16719360"/>
  <a0ChannelDyeName value="stale"/>
  <a0ChannelDyeName value="405/488/561/633nm"/>
  <phaseChannelColor value="16777215"/>
</Metadata>
"#
        .to_owned()]);

        assert_eq!(
            metadata.channel_colors,
            [
                Nd2ChannelColor {
                    name: "405/488/561/633nm".to_owned(),
                    color: 0x00ff_1e00,
                },
                Nd2ChannelColor {
                    name: "phase".to_owned(),
                    color: 0x00ff_ffff,
                },
            ]
        );
    }

    #[test]
    fn builds_the_java_compatible_u16_channel_lookup_table() {
        let mut model = Nd2MetadataModel {
            size_x: Some(1),
            size_y: Some(1),
            row_bytes: Some(2),
            logical_channels: Some(1),
            size_z: Some(1),
            size_t: Some(1),
            series_count: Some(1),
            storage_bits: Some(16),
            significant_bits: Some(16),
            channel_colors: vec![Nd2ChannelColor {
                name: "405/488/561/633nm".to_owned(),
                color: 0x00ff_1e00,
            }],
            ..Nd2MetadataModel::default()
        };
        finalize_channel_colors(&mut model);

        let series = Nd2Reader::build_series(&model, &[0]).unwrap().remove(0);
        assert!(series.metadata.is_indexed);
        assert!(series.metadata.is_false_color);
        assert_eq!(
            series.metadata.channel_metadata[0].name.as_deref(),
            Some("405/488/561/633nm")
        );
        assert_eq!(series.metadata.channel_metadata[0].color, Some(0x00ff_1e00));
        let lookup = series.metadata.lookup_table.unwrap();
        assert_eq!(
            (lookup.red.len(), lookup.green.len(), lookup.blue.len()),
            (65_536, 65_536, 65_536)
        );
        for index in 0..65_536 {
            assert_eq!(lookup.red[index], 0, "red[{index}]");
            let scale = index as f64 / 255.0;
            assert_eq!(lookup.green[index], (30.0 * scale) as u16, "green[{index}]");
            assert_eq!(lookup.blue[index], (255.0 * scale) as u16, "blue[{index}]");
        }
        assert_eq!(lookup.green[2_091], 245);
    }

    #[test]
    fn maps_shared_components_to_logical_channel_planes() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(4),
            row_bytes: Some(12),
            logical_channels: Some(3),
            size_z: Some(1),
            size_t: Some(2),
            series_count: Some(1),
            storage_bits: Some(8),
            significant_bits: Some(8),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[10, 11]).unwrap();
        assert_eq!(series.len(), 1);
        assert!(!series[0].metadata.is_rgb);
        assert_eq!(series[0].metadata.size_c, 3);
        assert_eq!(series[0].metadata.image_count, 6);
        assert_eq!(series[0].metadata.samples_per_pixel, 1);
        assert_eq!(
            series[0].planes,
            vec![
                Nd2Plane {
                    chunk_index: 10,
                    component: 0
                },
                Nd2Plane {
                    chunk_index: 10,
                    component: 1
                },
                Nd2Plane {
                    chunk_index: 10,
                    component: 2
                },
                Nd2Plane {
                    chunk_index: 11,
                    component: 0
                },
                Nd2Plane {
                    chunk_index: 11,
                    component: 1
                },
                Nd2Plane {
                    chunk_index: 11,
                    component: 2
                },
            ]
        );
    }

    #[test]
    fn rejects_physical_plane_count_mismatch() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(4),
            row_bytes: Some(12),
            logical_channels: Some(3),
            size_z: Some(1),
            size_t: Some(2),
            series_count: Some(1),
            storage_bits: Some(8),
            significant_bits: Some(8),
            ..Nd2MetadataModel::default()
        };
        let error = Nd2Reader::build_series(&metadata, &[0]).unwrap_err();
        assert!(matches!(error, BioFormatsError::UnsupportedFormat(_)));
    }

    #[test]
    fn maps_interleaved_multi_position_acquisitions() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(4),
            row_bytes: Some(4),
            logical_channels: Some(1),
            size_z: Some(1),
            size_t: Some(2),
            series_count: Some(2),
            acquisition_order: vec![Nd2Axis::Series, Nd2Axis::T],
            storage_bits: Some(8),
            significant_bits: Some(8),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[10, 11, 12, 13]).unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].planes[0].chunk_index, 10);
        assert_eq!(series[0].planes[1].chunk_index, 12);
        assert_eq!(series[1].planes[0].chunk_index, 11);
        assert_eq!(series[1].planes[1].chunk_index, 13);
    }

    #[test]
    fn rejects_multidimensional_data_without_loop_order() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(4),
            row_bytes: Some(4),
            logical_channels: Some(1),
            size_z: Some(2),
            size_t: Some(2),
            storage_bits: Some(8),
            significant_bits: Some(8),
            ..Nd2MetadataModel::default()
        };
        let error = Nd2Reader::build_series(&metadata, &[0, 1, 2, 3]).unwrap_err();
        assert!(matches!(error, BioFormatsError::UnsupportedFormat(_)));
    }

    #[test]
    fn parses_xml_attribute_values_and_nested_loop_order() {
        let metadata = parse_nd2_text_metadata(&[r#"
<?xml version="1.0" encoding="UTF-8"?>
<variant>
  <no_name runtype="RLxExperiment">
    <eType value="8"/>
    <uLoopPars><no_name runtype="RLxExperiment.RLxExpNETimeLoop"><uiCount value="2"/></no_name></uLoopPars>
    <ppNextLevelEx><no_name runtype="RLxExperiment">
      <eType value="4"/>
      <uLoopPars><no_name runtype="RLxExperiment.RLxExpZStackLoop"><uiCount value="3"/></no_name></uLoopPars>
    </no_name></ppNextLevelEx>
  </no_name>
  <uiWidth value="7"/><uiWidthBytes value="16"/><uiHeight value="5"/>
  <uiComp value="1"/><uiBpcInMemory value="16"/><uiBpcSignificant value="12"/>
</variant>
"#
        .to_string()]);
        assert_eq!((metadata.size_x, metadata.size_y), (Some(7), Some(5)));
        assert_eq!(metadata.row_bytes, Some(16));
        assert_eq!((metadata.size_z, metadata.size_t), (Some(3), Some(2)));
        assert_eq!(metadata.storage_bits, Some(16));
        assert_eq!(metadata.significant_bits, Some(12));
        assert_eq!(metadata.acquisition_order, vec![Nd2Axis::Z, Nd2Axis::T]);
    }

    #[test]
    fn reconciles_ne_time_periods_against_stored_plane_count() {
        let mut metadata = parse_nd2_text_metadata(&[r#"
<variant>
  <timeCount value="1"/>
  <no_name runtype="RLxExperiment">
    <eType value="8"/>
    <uLoopPars><no_name runtype="RLxExperiment.RLxExpNETimeLoop">
      <uiCount value="6"/><pPeriod>
        <_00 runtype="RLxExperiment.RLxExpTimeLoop"><uiCount value="4"/></_00>
        <_01 runtype="RLxExperiment.RLxExpTimeLoop"><uiCount value="11"/></_01>
      </pPeriod>
    </no_name></uLoopPars>
    <ppNextLevelEx><no_name runtype="RLxExperiment">
      <eType value="4"/>
      <uLoopPars><no_name runtype="RLxExperiment.RLxExpZStackLoop"><uiCount value="5"/></no_name></uLoopPars>
      <ppNextLevelEx><no_name runtype="RLxExperiment">
        <eType value="6"/>
        <uLoopPars><no_name runtype="RLxExperiment.RLxExpSpectLoop"><uiCount value="2"/></no_name></uLoopPars>
      </no_name></ppNextLevelEx>
    </no_name></ppNextLevelEx>
  </no_name>
</variant>
"#
        .to_string()]);

        reconcile_acquisition_dimensions(&mut metadata, 20).unwrap();
        assert_eq!((metadata.size_z, metadata.size_t), (Some(5), Some(4)));
        assert_eq!(metadata.acquisition_order, vec![Nd2Axis::Z, Nd2Axis::T]);
    }

    #[test]
    fn does_not_cartesian_mix_dimensions_from_different_experiment_roots() {
        let mut metadata = parse_acquisition_roots(&[
            acquisition_root_xml(Nd2Axis::T, 5, Nd2Axis::Z, 2),
            acquisition_root_xml(Nd2Axis::T, 4, Nd2Axis::Z, 3),
        ]);

        let error = reconcile_acquisition_dimensions(&mut metadata, 8).unwrap_err();
        assert!(matches!(error, BioFormatsError::UnsupportedFormat(_)));
        assert_ne!((metadata.size_z, metadata.size_t), (Some(2), Some(4)));
    }

    #[test]
    fn keeps_binary_lv_experiment_roots_separate() {
        let data = [
            lv_level(
                "SLxExperiment",
                &[
                    lv_u32("eType", 1),
                    lv_u32("uiCount", 5),
                    lv_u32("eType", 4),
                    lv_u32("uiCount", 2),
                ],
            ),
            lv_level(
                "SLxExperiment",
                &[
                    lv_u32("eType", 1),
                    lv_u32("uiCount", 4),
                    lv_u32("eType", 4),
                    lv_u32("uiCount", 3),
                ],
            ),
        ]
        .concat();
        let mut values = Vec::new();
        let mut colors = Vec::new();
        let mut roots = Vec::new();
        parse_lv_sequence(
            &data,
            0,
            data.len(),
            0,
            &mut values,
            &mut colors,
            &mut roots,
        );
        assert_eq!(roots.len(), 2);

        let mut metadata = Nd2MetadataModel::default();
        for root in roots {
            metadata
                .acquisition_candidates
                .extend(collect_lv_acquisition_candidates(&root));
        }
        let error = reconcile_acquisition_dimensions(&mut metadata, 8).unwrap_err();
        assert!(matches!(error, BioFormatsError::UnsupportedFormat(_)));
    }

    #[test]
    fn rejects_conflicting_whole_roots_even_when_the_stale_root_already_matches() {
        let mut metadata = parse_acquisition_roots(&[
            acquisition_root_xml(Nd2Axis::T, 6, Nd2Axis::Z, 2),
            acquisition_root_xml(Nd2Axis::T, 4, Nd2Axis::Z, 3),
        ]);

        let error = reconcile_acquisition_dimensions(&mut metadata, 12).unwrap_err();
        assert!(matches!(error, BioFormatsError::UnsupportedFormat(_)));
    }

    #[test]
    fn selected_root_carries_its_acquisition_order_into_dimension_order() {
        let mut metadata = parse_acquisition_roots(&[
            acquisition_root_xml(Nd2Axis::T, 5, Nd2Axis::Z, 2),
            acquisition_root_xml(Nd2Axis::Z, 2, Nd2Axis::T, 4),
        ]);

        reconcile_acquisition_dimensions(&mut metadata, 8).unwrap();
        assert_eq!((metadata.size_z, metadata.size_t), (Some(2), Some(4)));
        assert_eq!(metadata.acquisition_order, vec![Nd2Axis::T, Nd2Axis::Z]);

        metadata.size_x = Some(1);
        metadata.size_y = Some(1);
        metadata.row_bytes = Some(2);
        metadata.logical_channels = Some(1);
        metadata.storage_bits = Some(16);
        metadata.significant_bits = Some(16);
        let chunks = (0..8).collect::<Vec<_>>();
        let series = Nd2Reader::build_series(&metadata, &chunks).unwrap();
        assert_eq!(series[0].metadata.dimension_order, DimensionOrder::XYCTZ);
    }

    #[test]
    fn reconciles_stale_and_final_experiment_fragments() {
        let mut metadata = parse_nd2_text_metadata(&[
            r#"<variant><no_name runtype="RLxExperiment"><eType value="1"/><uLoopPars><no_name runtype="RLxExperiment.RLxExpTimeLoop"><uiCount value="61"/></no_name></uLoopPars><ppNextLevelEx><no_name runtype="RLxExperiment"><eType value="4"/><uLoopPars><no_name runtype="RLxExperiment.RLxExpZStackLoop"><uiCount value="13"/></no_name></uLoopPars></no_name></ppNextLevelEx></no_name></variant>"#.to_string(),
            r#"<variant><no_name runtype="RLxExperiment"><eType value="1"/><uLoopPars><no_name runtype="RLxExperiment.RLxExpTimeLoop"><uiCount value="11"/></no_name></uLoopPars><ppNextLevelEx><no_name runtype="RLxExperiment"><eType value="4"/><uLoopPars><no_name runtype="RLxExperiment.RLxExpZStackLoop"><uiCount value="13"/></no_name></uLoopPars></no_name></ppNextLevelEx></no_name></variant>"#.to_string(),
        ]);

        reconcile_acquisition_dimensions(&mut metadata, 143).unwrap();
        assert_eq!((metadata.size_z, metadata.size_t), (Some(13), Some(11)));
        assert_eq!(metadata.experiment_roots, 2);
        metadata.size_x = Some(2);
        metadata.size_y = Some(2);
        metadata.row_bytes = Some(4);
        metadata.logical_channels = Some(1);
        metadata.storage_bits = Some(16);
        metadata.significant_bits = Some(14);
        let chunks = (0..143).collect::<Vec<_>>();
        let series = Nd2Reader::build_series(&metadata, &chunks).unwrap();
        assert_eq!(series[0].metadata.dimension_order, DimensionOrder::XYZCT);
    }

    #[test]
    fn derives_a_binary_lv_axis_when_only_the_final_experiment_was_stored() {
        let values = vec![
            ("eType".to_string(), LvValue::Unsigned(1)),
            ("uiCount".to_string(), LvValue::Unsigned(0)),
            ("eType".to_string(), LvValue::Unsigned(4)),
            ("uiCount".to_string(), LvValue::Unsigned(17)),
        ];
        let mut metadata = parse_nd2_lv_metadata(&values);

        reconcile_acquisition_dimensions(&mut metadata, 17).unwrap();
        assert_eq!((metadata.size_z, metadata.size_t), (Some(17), Some(1)));
        assert_eq!(metadata.acquisition_order, vec![Nd2Axis::Z]);
    }

    #[test]
    fn removes_raw_row_padding_and_extracts_interleaved_components() {
        let metadata = Nd2MetadataModel {
            size_x: Some(2),
            size_y: Some(2),
            row_bytes: Some(8),
            logical_channels: Some(3),
            storage_bits: Some(8),
            significant_bits: Some(8),
            compression: Some(Nd2Compression::Raw),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[0]).unwrap().remove(0);
        let mut chunk = vec![0; 8];
        chunk.extend_from_slice(&[1, 10, 100, 2, 20, 200, 0xaa, 0xbb]);
        chunk.extend_from_slice(&[3, 30, 130, 4, 40, 140, 0xcc, 0xdd]);
        let physical = decode_physical_plane(&chunk, &series).unwrap();
        assert_eq!(physical, [1, 10, 100, 2, 20, 200, 3, 30, 130, 4, 40, 140]);
        assert_eq!(
            extract_component(&physical, &series, 1).unwrap(),
            [10, 20, 30, 40]
        );
    }

    #[test]
    fn rejects_malformed_raw_and_single_component_lengths_before_copying() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(1),
            row_bytes: Some(4),
            logical_channels: Some(1),
            storage_bits: Some(8),
            significant_bits: Some(8),
            compression: Some(Nd2Compression::Raw),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[0]).unwrap().remove(0);
        let mut oversized_chunk = vec![0; 8];
        oversized_chunk.extend_from_slice(&[1, 2, 3, 4, 5]);

        assert!(matches!(
            decode_physical_plane(&oversized_chunk, &series),
            Err(BioFormatsError::Format(_))
        ));
        assert!(matches!(
            extract_component(&[1, 2, 3, 4, 5], &series, 0),
            Err(BioFormatsError::Format(_))
        ));
    }

    #[test]
    fn decodes_zlib_after_the_image_prefix() {
        let metadata = Nd2MetadataModel {
            size_x: Some(4),
            size_y: Some(1),
            row_bytes: Some(4),
            logical_channels: Some(1),
            storage_bits: Some(8),
            significant_bits: Some(8),
            compression: Some(Nd2Compression::Zlib),
            ..Nd2MetadataModel::default()
        };
        let series = Nd2Reader::build_series(&metadata, &[0]).unwrap().remove(0);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&[1, 2, 3, 4]).unwrap();
        let mut chunk = vec![0; 8];
        chunk.extend_from_slice(&encoder.finish().unwrap());
        assert_eq!(
            decode_physical_plane(&chunk, &series).unwrap(),
            [1, 2, 3, 4]
        );
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
  <uiBpcInMemory>16</uiBpcInMemory>
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
        assert_eq!(metadata.storage_bits, Some(16));
        assert_eq!(metadata.significant_bits, Some(16));
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
