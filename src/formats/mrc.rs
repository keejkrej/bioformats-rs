/*
 * Ported from OME Bio-Formats' MRCReader.java.
 *
 * Copyright (C) 2005 - 2017 Open Microscopy Environment:
 *   - Board of Regents of the University of Wisconsin-Madison
 *   - Glencoe Software, Inc.
 *   - University of Dundee
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 2 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
 * General Public License for more details.
 */

//! Medical Research Council (MRC) microscopy volume reader.
//!
//! MRC rows have their origin at the lower-left. This reader exposes the same
//! top-down row order as the other readers in this crate while reading only
//! the requested rows and columns from the file.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{destination_prefix, try_zeroed_bytes, validate_region, FormatReader};

const HEADER_SIZE: usize = 1024;
const EXT_HEADER_SIZE_OFFSET: usize = 92;
const EXT_HEADER_TYPE_OFFSET: usize = 104;
const IMOD_STAMP_OFFSET: usize = 152;
const IMOD_FLAGS_OFFSET: usize = 156;
const ENDIANNESS_OFFSET: usize = 212;
const IMOD_STAMP: i32 = 1_146_047_817; // little-endian bytes `IMOD`

const MRC_SUFFIXES: &[&str] = &["mrc", "st", "ali", "map", "rec", "mrcs"];
const SERIES_TYPES: &[&str] = &["mono", "tilt", "tilts", "lina", "lins"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn read_i16(self, bytes: &[u8]) -> i16 {
        let bytes = [bytes[0], bytes[1]];
        match self {
            Self::Little => i16::from_le_bytes(bytes),
            Self::Big => i16::from_be_bytes(bytes),
        }
    }

    fn read_i32(self, bytes: &[u8]) -> i32 {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::Little => i32::from_le_bytes(bytes),
            Self::Big => i32::from_be_bytes(bytes),
        }
    }

    fn read_f32(self, bytes: &[u8]) -> f32 {
        let bits = match self {
            Self::Little => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            Self::Big => u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        };
        f32::from_bits(bits)
    }
}

#[derive(Debug, Clone, Copy)]
struct ModeInfo {
    pixel_type: PixelType,
    samples_per_pixel: u32,
    bytes_per_sample: usize,
    rgb: bool,
}

fn mode_info(mode: i32, imod_signed: bool) -> Option<ModeInfo> {
    let (pixel_type, samples_per_pixel, rgb) = match mode {
        0 if imod_signed => (PixelType::Int8, 1, false),
        0 => (PixelType::Uint8, 1, false),
        1 => (PixelType::Int16, 1, false),
        2 => (PixelType::Float32, 1, false),
        // These mappings intentionally match Bio-Formats' MRCReader.
        3 => (PixelType::Uint32, 1, false),
        4 => (PixelType::Float64, 1, false),
        6 => (PixelType::Uint16, 1, false),
        16 => (PixelType::Uint8, 3, true),
        _ => return None,
    };
    Some(ModeInfo {
        pixel_type,
        samples_per_pixel,
        bytes_per_sample: pixel_type.bytes_per_sample(),
        rgb,
    })
}

#[derive(Debug, Clone, Copy)]
struct ParsedHeader {
    order: ByteOrder,
    width: u32,
    height: u32,
    planes: u32,
    mode: i32,
    mode_info: ModeInfo,
    data_offset: u64,
    row_bytes: u64,
    plane_bytes: u64,
}

impl ParsedHeader {
    fn decode(header: &[u8], order: ByteOrder, file_len: Option<u64>) -> Option<Self> {
        if header.len() < HEADER_SIZE {
            return None;
        }

        let width = positive_u32(order.read_i32(&header[0..4]))?;
        let height = positive_u32(order.read_i32(&header[4..8]))?;
        let planes = positive_u32(order.read_i32(&header[8..12]))?;
        let mode = order.read_i32(&header[12..16]);
        let imod = order.read_i32(&header[IMOD_STAMP_OFFSET..IMOD_STAMP_OFFSET + 4]) == IMOD_STAMP;
        let imod_flags = order.read_i32(&header[IMOD_FLAGS_OFFSET..IMOD_FLAGS_OFFSET + 4]);
        let mode_info = mode_info(mode, imod && imod_flags & 1 != 0)?;
        let extended_header_size =
            u64::try_from(order.read_i32(&header[EXT_HEADER_SIZE_OFFSET..96])).ok()?;
        let data_offset = (HEADER_SIZE as u64).checked_add(extended_header_size)?;
        let bytes_per_pixel = u64::from(mode_info.samples_per_pixel)
            .checked_mul(u64::try_from(mode_info.bytes_per_sample).ok()?)?;
        let row_bytes = u64::from(width).checked_mul(bytes_per_pixel)?;
        let plane_bytes = row_bytes.checked_mul(u64::from(height))?;
        let data_bytes = plane_bytes.checked_mul(u64::from(planes))?;
        let data_end = data_offset.checked_add(data_bytes)?;
        if file_len.is_some_and(|length| data_end > length) {
            return None;
        }

        Some(Self {
            order,
            width,
            height,
            planes,
            mode,
            mode_info,
            data_offset,
            row_bytes,
            plane_bytes,
        })
    }
}

fn positive_u32(value: i32) -> Option<u32> {
    (value > 0).then_some(value as u32)
}

fn looks_like_other_format(header: &[u8]) -> bool {
    if header.starts_with(b"ZISRAWFILE")
        || header.starts_with(b"DCIMG")
        || header.starts_with(b"MATRIX70v")
        || header.starts_with(b"MATRIX72v")
    {
        return true;
    }
    if header.len() < 8 {
        return false;
    }
    let first = u32::from_le_bytes(header[0..4].try_into().unwrap_or([0; 4]));
    let second = u32::from_le_bytes(header[4..8].try_into().unwrap_or([0; 4]));
    first == 0xdace_be0a || first == 0x0abe_ceda || second == 0x6a50_2020
}

fn order_candidates(header: &[u8]) -> [ByteOrder; 2] {
    match header.get(ENDIANNESS_OFFSET).copied() {
        Some(17) => [ByteOrder::Big, ByteOrder::Little],
        // New little-endian headers use 68; old headers default to little.
        _ => [ByteOrder::Little, ByteOrder::Big],
    }
}

fn parse_header(header: &[u8], file_len: u64) -> Result<ParsedHeader> {
    if header.len() < HEADER_SIZE {
        return Err(BioFormatsError::Format(format!(
            "MRC header is truncated: expected {HEADER_SIZE} bytes, got {}",
            header.len()
        )));
    }
    if looks_like_other_format(header) {
        return Err(BioFormatsError::Format(
            "file has a non-MRC container signature".into(),
        ));
    }

    for order in order_candidates(header) {
        if let Some(parsed) = ParsedHeader::decode(header, order, Some(file_len)) {
            return Ok(parsed);
        }
    }

    let unsupported_mode = order_candidates(header).into_iter().find_map(|order| {
        let width = order.read_i32(&header[0..4]);
        let height = order.read_i32(&header[4..8]);
        let planes = order.read_i32(&header[8..12]);
        let mode = order.read_i32(&header[12..16]);
        let ext = order.read_i32(&header[EXT_HEADER_SIZE_OFFSET..96]);
        (width > 0 && height > 0 && planes > 0 && ext >= 0 && mode_info(mode, false).is_none())
            .then_some(mode)
    });
    if let Some(mode) = unsupported_mode {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "MRC pixel mode {mode} is not supported"
        )));
    }

    Err(BioFormatsError::Format(
        "invalid, overflowing, or truncated MRC dimensions/data".into(),
    ))
}

fn metadata_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn physical_size_um(cell_length: f32, grid_size: i32, agar: bool) -> Option<f64> {
    if grid_size <= 0 || !cell_length.is_finite() || cell_length <= 0.0 {
        return None;
    }
    let value = f64::from(cell_length) / f64::from(grid_size);
    let value_um = if agar { value } else { value / 10_000.0 };
    (value_um.is_finite() && value_um > 0.0).then_some(value_um)
}

fn insert_float(metadata: &mut HashMap<String, MetadataValue>, key: &str, value: f32) {
    metadata.insert(key.to_string(), MetadataValue::Float(f64::from(value)));
}

fn build_metadata(header: &[u8], parsed: ParsedHeader, path: &Path) -> ImageMetadata {
    let order = parsed.order;
    let read_i32 = |offset: usize| order.read_i32(&header[offset..offset + 4]);
    let read_i16 = |offset: usize| order.read_i16(&header[offset..offset + 2]);
    let read_f32 = |offset: usize| order.read_f32(&header[offset..offset + 4]);

    let mx = read_i32(28);
    let my = read_i32(32);
    let mz = read_i32(36);
    let xlen = read_f32(40);
    let ylen = read_f32(44);
    let zlen = read_f32(48);
    let minimum = read_f32(76);
    let maximum = read_f32(80);
    let mean = read_f32(84);
    let ispg = read_i32(88);
    let ext_type = metadata_string(&header[EXT_HEADER_TYPE_OFFSET..108]);
    let agar = ext_type == "AGAR";

    let mut pixel_type = parsed.mode_info.pixel_type;
    // Bio-Formats compensates for EMAN2 files that label unsigned 16-bit data
    // as signed mode 1 based on the declared range.
    if pixel_type == PixelType::Int16
        && (f64::from(maximum) > 32_767.5 || f64::from(minimum) < -32_767.5)
    {
        pixel_type = PixelType::Uint16;
    }

    let mut series_metadata = HashMap::new();
    series_metadata.insert(
        "MRC mode".into(),
        MetadataValue::Int(i64::from(parsed.mode)),
    );
    series_metadata.insert("Grid size (X)".into(), MetadataValue::Int(i64::from(mx)));
    series_metadata.insert("Grid size (Y)".into(), MetadataValue::Int(i64::from(my)));
    series_metadata.insert("Grid size (Z)".into(), MetadataValue::Int(i64::from(mz)));
    insert_float(&mut series_metadata, "Cell size (X)", xlen);
    insert_float(&mut series_metadata, "Cell size (Y)", ylen);
    insert_float(&mut series_metadata, "Cell size (Z)", zlen);
    insert_float(&mut series_metadata, "Alpha angle", read_f32(52));
    insert_float(&mut series_metadata, "Beta angle", read_f32(56));
    insert_float(&mut series_metadata, "Gamma angle", read_f32(60));
    insert_float(&mut series_metadata, "Minimum pixel value", minimum);
    insert_float(&mut series_metadata, "Maximum pixel value", maximum);
    insert_float(&mut series_metadata, "Mean pixel value", mean);
    series_metadata.insert("ISPG".into(), MetadataValue::Int(i64::from(ispg)));
    series_metadata.insert("Is data cube".into(), MetadataValue::Bool(ispg == 1));
    series_metadata.insert(
        "Extended header size".into(),
        MetadataValue::Int((parsed.data_offset - HEADER_SIZE as u64) as i64),
    );
    series_metadata.insert(
        "Extended header type".into(),
        MetadataValue::String(ext_type.clone()),
    );

    let idtype = read_i16(160);
    let series_type = usize::try_from(idtype)
        .ok()
        .and_then(|index| SERIES_TYPES.get(index))
        .copied()
        .unwrap_or("unknown");
    series_metadata.insert(
        "Series type".into(),
        MetadataValue::String(series_type.into()),
    );
    for (key, offset) in [
        ("Lens", 162),
        ("ND1", 164),
        ("ND2", 166),
        ("VD1", 168),
        ("VD2", 170),
    ] {
        series_metadata.insert(key.into(), MetadataValue::Int(i64::from(read_i16(offset))));
    }
    for index in 0..6 {
        insert_float(
            &mut series_metadata,
            &format!("Angle {}", index + 1),
            read_f32(172 + index * 4),
        );
    }
    let useful_labels = read_i32(220);
    series_metadata.insert(
        "Number of useful labels".into(),
        MetadataValue::Int(i64::from(useful_labels)),
    );
    for index in 0..usize::try_from(useful_labels).unwrap_or(0).min(10) {
        let offset = 224 + index * 80;
        let label = metadata_string(&header[offset..offset + 80]);
        if !label.is_empty() {
            series_metadata.insert(format!("Label {}", index + 1), MetadataValue::String(label));
        }
    }

    ImageMetadata {
        size_x: parsed.width,
        size_y: parsed.height,
        size_z: parsed.planes,
        size_c: parsed.mode_info.samples_per_pixel,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (parsed.mode_info.bytes_per_sample * 8) as u8,
        samples_per_pixel: parsed.mode_info.samples_per_pixel,
        image_count: parsed.planes,
        dimension_order: if parsed.mode_info.rgb {
            DimensionOrder::XYCZT
        } else {
            DimensionOrder::XYZTC
        },
        is_rgb: parsed.mode_info.rgb,
        is_interleaved: true,
        is_indexed: false,
        is_false_color: false,
        is_little_endian: parsed.order == ByteOrder::Little,
        resolution_count: 1,
        series_metadata,
        physical_size_x_um: physical_size_um(xlen, mx, agar),
        physical_size_y_um: physical_size_um(ylen, my, agar),
        physical_size_z_um: physical_size_um(zlen, mz, agar),
        used_files: vec![path.to_path_buf()],
        ..ImageMetadata::default()
    }
}

pub struct MrcReader {
    file: Option<BufReader<File>>,
    path: Option<PathBuf>,
    metadata: Option<ImageMetadata>,
    parsed: Option<ParsedHeader>,
}

impl MrcReader {
    pub fn new() -> Self {
        Self {
            file: None,
            path: None,
            metadata: None,
            parsed: None,
        }
    }

    fn initialized(&self) -> Result<(ParsedHeader, &ImageMetadata)> {
        let parsed = self.parsed.ok_or(BioFormatsError::NotInitialized)?;
        let metadata = self
            .metadata
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        Ok((parsed, metadata))
    }

    fn read_region_into(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        destination: &mut [u8],
    ) -> Result<usize> {
        let (parsed, metadata) = self.initialized()?;
        validate_region(metadata, x, y, width, height)?;
        if plane_index >= parsed.planes {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let bytes_per_pixel = usize::try_from(parsed.mode_info.samples_per_pixel)
            .ok()
            .and_then(|samples| samples.checked_mul(parsed.mode_info.bytes_per_sample))
            .ok_or_else(|| BioFormatsError::InvalidData("MRC pixel size overflow".into()))?;
        let output_row_bytes = usize::try_from(width)
            .ok()
            .and_then(|value| value.checked_mul(bytes_per_pixel))
            .ok_or_else(|| BioFormatsError::InvalidData("MRC region row size overflow".into()))?;
        let output_len = usize::try_from(height)
            .ok()
            .and_then(|value| value.checked_mul(output_row_bytes))
            .ok_or_else(|| BioFormatsError::InvalidData("MRC region size overflow".into()))?;
        let x_offset = u64::from(x)
            .checked_mul(u64::try_from(bytes_per_pixel).map_err(|_| {
                BioFormatsError::InvalidData("MRC horizontal offset overflow".into())
            })?)
            .ok_or_else(|| BioFormatsError::InvalidData("MRC horizontal offset overflow".into()))?;
        let plane_offset = parsed
            .data_offset
            .checked_add(
                u64::from(plane_index)
                    .checked_mul(parsed.plane_bytes)
                    .ok_or_else(|| {
                        BioFormatsError::InvalidData("MRC plane offset overflow".into())
                    })?,
            )
            .ok_or_else(|| BioFormatsError::InvalidData("MRC plane offset overflow".into()))?;

        let output = destination_prefix(destination, output_len)?;
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        for output_row in 0..height {
            let source_row = parsed
                .height
                .checked_sub(1)
                .and_then(|last| last.checked_sub(y))
                .and_then(|first| first.checked_sub(output_row))
                .ok_or_else(|| BioFormatsError::InvalidData("MRC row offset underflow".into()))?;
            let source_offset = plane_offset
                .checked_add(
                    u64::from(source_row)
                        .checked_mul(parsed.row_bytes)
                        .ok_or_else(|| {
                            BioFormatsError::InvalidData("MRC row offset overflow".into())
                        })?,
                )
                .and_then(|offset| offset.checked_add(x_offset))
                .ok_or_else(|| BioFormatsError::InvalidData("MRC row offset overflow".into()))?;
            file.seek(SeekFrom::Start(source_offset))?;
            let destination = usize::try_from(output_row)
                .ok()
                .and_then(|row| row.checked_mul(output_row_bytes))
                .ok_or_else(|| BioFormatsError::InvalidData("MRC output offset overflow".into()))?;
            file.read_exact(&mut output[destination..destination + output_row_bytes])?;
        }
        Ok(output_len)
    }

    fn read_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        let (parsed, metadata) = self.initialized()?;
        validate_region(metadata, x, y, width, height)?;
        if plane_index >= parsed.planes {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bytes_per_pixel = usize::try_from(parsed.mode_info.samples_per_pixel)
            .ok()
            .and_then(|samples| samples.checked_mul(parsed.mode_info.bytes_per_sample))
            .ok_or_else(|| BioFormatsError::InvalidData("MRC pixel size overflow".into()))?;
        let output_len = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(bytes_per_pixel))
            .and_then(|row| usize::try_from(height).ok()?.checked_mul(row))
            .ok_or_else(|| BioFormatsError::InvalidData("MRC region size overflow".into()))?;
        let mut output = try_zeroed_bytes(output_len, "MRC region buffer")?;
        self.read_region_into(plane_index, x, y, width, height, &mut output)?;
        Ok(output)
    }
}

impl Default for MrcReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MrcReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                MRC_SUFFIXES
                    .iter()
                    .any(|suffix| extension.eq_ignore_ascii_case(suffix))
            })
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < HEADER_SIZE || looks_like_other_format(header) {
            return false;
        }
        order_candidates(header)
            .into_iter()
            .any(|order| ParsedHeader::decode(header, order, None).is_some())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut raw_header = [0; HEADER_SIZE];
        reader.read_exact(&mut raw_header)?;
        let parsed = parse_header(&raw_header, file_len)?;
        let metadata = build_metadata(&raw_header, parsed, path);

        self.file = Some(reader);
        self.path = Some(path.to_path_buf());
        self.metadata = Some(metadata);
        self.parsed = Some(parsed);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.file = None;
        self.path = None;
        self.metadata = None;
        self.parsed = None;
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
        self.metadata.as_ref().expect("MrcReader not initialized")
    }

    fn current_file(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn used_files(&self) -> Vec<PathBuf> {
        self.path.iter().cloned().collect()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (_, metadata) = self.initialized()?;
        self.read_region(plane_index, 0, 0, metadata.size_x, metadata.size_y)
    }

    fn open_bytes_into(&mut self, plane_index: u32, destination: &mut [u8]) -> Result<usize> {
        let (_, metadata) = self.initialized()?;
        let (width, height) = (metadata.size_x, metadata.size_y);
        self.read_region_into(plane_index, 0, 0, width, height, destination)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.read_region(plane_index, x, y, w, h)
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
        self.read_region_into(plane_index, x, y, w, h, destination)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (_, metadata) = self.initialized()?;
        let width = metadata.size_x.min(256);
        let height = metadata.size_y.min(256);
        let x = (metadata.size_x - width) / 2;
        let y = (metadata.size_y - height) / 2;
        self.read_region(plane_index, x, y, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempMrc {
        path: PathBuf,
    }

    impl TempMrc {
        fn write(name: &str, bytes: &[u8]) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bioformats_rs_mrc_{name}_{}_{unique}.mrc",
                std::process::id()
            ));
            fs::write(&path, bytes).unwrap();
            Self { path }
        }
    }

    impl Drop for TempMrc {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn write_i16(bytes: &mut [u8], offset: usize, value: i16, order: ByteOrder) {
        let encoded = match order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        bytes[offset..offset + 2].copy_from_slice(&encoded);
    }

    fn write_i32(bytes: &mut [u8], offset: usize, value: i32, order: ByteOrder) {
        let encoded = match order {
            ByteOrder::Little => value.to_le_bytes(),
            ByteOrder::Big => value.to_be_bytes(),
        };
        bytes[offset..offset + 4].copy_from_slice(&encoded);
    }

    fn write_f32(bytes: &mut [u8], offset: usize, value: f32, order: ByteOrder) {
        let encoded = match order {
            ByteOrder::Little => value.to_bits().to_le_bytes(),
            ByteOrder::Big => value.to_bits().to_be_bytes(),
        };
        bytes[offset..offset + 4].copy_from_slice(&encoded);
    }

    fn synthetic_header(
        order: ByteOrder,
        width: i32,
        height: i32,
        planes: i32,
        mode: i32,
        extended_header_size: i32,
    ) -> Vec<u8> {
        let ext = usize::try_from(extended_header_size.max(0)).unwrap();
        let mut bytes = vec![0; HEADER_SIZE + ext];
        write_i32(&mut bytes, 0, width, order);
        write_i32(&mut bytes, 4, height, order);
        write_i32(&mut bytes, 8, planes, order);
        write_i32(&mut bytes, 12, mode, order);
        write_i32(&mut bytes, 28, width.max(1), order);
        write_i32(&mut bytes, 32, height.max(1), order);
        write_i32(&mut bytes, 36, planes.max(1), order);
        write_f32(&mut bytes, 40, width.max(1) as f32 * 10.0, order);
        write_f32(&mut bytes, 44, height.max(1) as f32 * 10.0, order);
        write_f32(&mut bytes, 48, planes.max(1) as f32 * 10.0, order);
        write_f32(&mut bytes, 52, 90.0, order);
        write_f32(&mut bytes, 56, 90.0, order);
        write_f32(&mut bytes, 60, 90.0, order);
        write_f32(&mut bytes, 76, 0.0, order);
        write_f32(&mut bytes, 80, 255.0, order);
        write_f32(&mut bytes, 84, 127.5, order);
        write_i32(&mut bytes, 88, 1, order);
        write_i32(&mut bytes, 92, extended_header_size, order);
        bytes[104..108].copy_from_slice(b"MRC ");
        bytes[208..212].copy_from_slice(b"MAP ");
        bytes[ENDIANNESS_OFFSET] = match order {
            ByteOrder::Little => 68,
            ByteOrder::Big => 17,
        };
        write_i16(&mut bytes, 160, 1, order);
        write_i32(&mut bytes, 220, 1, order);
        bytes[224..228].copy_from_slice(b"test");
        bytes
    }

    fn synthetic_file(
        order: ByteOrder,
        width: i32,
        height: i32,
        planes: i32,
        mode: i32,
        extended_header_size: i32,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut bytes = synthetic_header(order, width, height, planes, mode, extended_header_size);
        bytes.extend_from_slice(pixels);
        bytes
    }

    #[test]
    fn detects_supported_suffixes_and_plausible_headers() {
        let reader = MrcReader::new();
        for suffix in ["mrc", "ST", "ali", "map", "rec", "mrcs"] {
            assert!(reader.is_this_type_by_name(Path::new(&format!("image.{suffix}"))));
        }
        assert!(!reader.is_this_type_by_name(Path::new("image.tif")));

        let little = synthetic_file(ByteOrder::Little, 2, 2, 1, 0, 0, &[0; 4]);
        let big = synthetic_file(ByteOrder::Big, 2, 2, 1, 1, 0, &[0; 8]);
        assert!(reader.is_this_type_by_bytes(&little[..HEADER_SIZE]));
        assert!(reader.is_this_type_by_bytes(&big[..HEADER_SIZE]));
        assert!(!reader.is_this_type_by_bytes(&little[..HEADER_SIZE - 1]));

        let mut unsupported = synthetic_header(ByteOrder::Little, 2, 2, 1, 99, 0);
        assert!(!reader.is_this_type_by_bytes(&unsupported));
        unsupported[..10].copy_from_slice(b"ZISRAWFILE");
        assert!(!reader.is_this_type_by_bytes(&unsupported));
    }

    #[test]
    fn reads_little_endian_bytes_with_top_down_rows_and_metadata() {
        let bytes = synthetic_file(ByteOrder::Little, 3, 2, 1, 0, 0, &[4, 5, 6, 1, 2, 3]);
        let file = TempMrc::write("little", &bytes);
        let mut reader = MrcReader::new();
        reader.set_id(&file.path).unwrap();

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        let mut destination = [0xaa; 8];
        assert_eq!(reader.open_bytes_into(0, &mut destination).unwrap(), 6);
        assert_eq!(&destination[..6], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(&destination[6..], &[0xaa, 0xaa]);
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_z, 1);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert!(reader.metadata().is_little_endian);
        assert_eq!(reader.metadata().physical_size_x_um, Some(0.001));
        assert_eq!(reader.metadata().dimension_order, DimensionOrder::XYZTC);
        assert_eq!(reader.used_files(), vec![file.path.clone()]);
        assert_eq!(reader.metadata().used_files, vec![file.path.clone()]);
        assert!(matches!(
            reader.metadata().series_metadata.get("Series type"),
            Some(MetadataValue::String(value)) if value == "tilt"
        ));
    }

    #[test]
    fn reads_big_endian_header_and_preserves_sample_byte_order() {
        let pixels = [0, 3, 0, 4, 0, 1, 0, 2];
        let bytes = synthetic_file(ByteOrder::Big, 2, 2, 1, 1, 0, &pixels);
        let file = TempMrc::write("big", &bytes);
        let mut reader = MrcReader::new();
        reader.set_id(&file.path).unwrap();

        assert!(!reader.metadata().is_little_endian);
        assert_eq!(reader.metadata().pixel_type, PixelType::Int16);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0, 1, 0, 2, 0, 3, 0, 4]);
    }

    #[test]
    fn exposes_java_compatible_pixel_modes() {
        let cases = [
            (0, PixelType::Uint8, 1, 1, false),
            (1, PixelType::Int16, 2, 1, false),
            (2, PixelType::Float32, 4, 1, false),
            (3, PixelType::Uint32, 4, 1, false),
            (4, PixelType::Float64, 8, 1, false),
            (6, PixelType::Uint16, 2, 1, false),
            (16, PixelType::Uint8, 3, 3, true),
        ];
        for (mode, pixel_type, pixel_bytes, samples_per_pixel, rgb) in cases {
            let bytes = synthetic_file(ByteOrder::Little, 1, 1, 1, mode, 0, &vec![7; pixel_bytes]);
            let file = TempMrc::write(&format!("mode_{mode}"), &bytes);
            let mut reader = MrcReader::new();
            reader.set_id(&file.path).unwrap();
            assert_eq!(reader.metadata().pixel_type, pixel_type, "mode {mode}");
            assert_eq!(
                reader.metadata().samples_per_pixel,
                samples_per_pixel,
                "mode {mode}"
            );
            assert_eq!(reader.metadata().is_rgb, rgb, "mode {mode}");
            assert_eq!(reader.open_bytes(0).unwrap(), vec![7; pixel_bytes]);
        }
    }

    #[test]
    fn honors_imod_signed_bytes_and_eman_unsigned_sixteen_bit_range() {
        let mut signed = synthetic_header(ByteOrder::Little, 1, 1, 1, 0, 0);
        write_i32(
            &mut signed,
            IMOD_STAMP_OFFSET,
            IMOD_STAMP,
            ByteOrder::Little,
        );
        write_i32(&mut signed, IMOD_FLAGS_OFFSET, 1, ByteOrder::Little);
        signed.push(0xff);
        let signed_file = TempMrc::write("signed", &signed);
        let mut reader = MrcReader::new();
        reader.set_id(&signed_file.path).unwrap();
        assert_eq!(reader.metadata().pixel_type, PixelType::Int8);

        let mut unsigned = synthetic_header(ByteOrder::Little, 1, 1, 1, 1, 0);
        write_f32(&mut unsigned, 80, 65_535.0, ByteOrder::Little);
        unsigned.extend_from_slice(&[0xff, 0xff]);
        let unsigned_file = TempMrc::write("unsigned", &unsigned);
        reader.set_id(&unsigned_file.path).unwrap();
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    }

    #[test]
    fn skips_extended_header_and_indexes_z_planes() {
        let mut bytes = synthetic_header(ByteOrder::Little, 2, 1, 2, 6, 12);
        bytes[HEADER_SIZE..HEADER_SIZE + 12].fill(0xaa);
        bytes.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
        let file = TempMrc::write("extended", &bytes);
        let mut reader = MrcReader::new();
        reader.set_id(&file.path).unwrap();

        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 0, 2, 0]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 0, 4, 0]);
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
    }

    #[test]
    fn reads_a_bounded_region_without_materializing_the_plane() {
        let pixels = [9, 10, 11, 12, 5, 6, 7, 8, 1, 2, 3, 4];
        let bytes = synthetic_file(ByteOrder::Little, 4, 3, 1, 0, 0, &pixels);
        let file = TempMrc::write("region", &bytes);
        let mut reader = MrcReader::new();
        reader.set_id(&file.path).unwrap();

        assert_eq!(
            reader.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
            [6, 7, 10, 11]
        );
        assert!(matches!(
            reader.open_bytes_region(0, 3, 0, 2, 1),
            Err(BioFormatsError::InvalidRegion { .. })
        ));
        assert!(matches!(
            reader.open_bytes_region(0, 0, 0, 0, 1),
            Err(BioFormatsError::InvalidRegionShape { .. })
        ));
        assert!(matches!(
            reader.open_bytes_region(0, 0, 0, u32::MAX, u32::MAX),
            Err(BioFormatsError::InvalidRegionShape { .. })
                | Err(BioFormatsError::InvalidRegion { .. })
        ));
    }

    #[test]
    fn interprets_agar_cell_lengths_as_micrometers() {
        let mut bytes = synthetic_header(ByteOrder::Little, 2, 1, 1, 0, 0);
        bytes[104..108].copy_from_slice(b"AGAR");
        write_i32(&mut bytes, 28, 2, ByteOrder::Little);
        write_f32(&mut bytes, 40, 1.0, ByteOrder::Little);
        bytes.extend_from_slice(&[1, 2]);
        let file = TempMrc::write("agar", &bytes);
        let mut reader = MrcReader::new();
        reader.set_id(&file.path).unwrap();
        assert_eq!(reader.metadata().physical_size_x_um, Some(0.5));
    }

    #[test]
    fn rejects_invalid_unsupported_and_truncated_files() {
        let mut zero_width = synthetic_header(ByteOrder::Little, 0, 1, 1, 0, 0);
        zero_width.push(1);
        let zero_file = TempMrc::write("zero", &zero_width);
        assert!(MrcReader::new().set_id(&zero_file.path).is_err());

        let mut negative_ext = synthetic_header(ByteOrder::Little, 1, 1, 1, 0, 0);
        write_i32(&mut negative_ext, 92, -1, ByteOrder::Little);
        negative_ext.push(1);
        let negative_file = TempMrc::write("negative_ext", &negative_ext);
        assert!(MrcReader::new().set_id(&negative_file.path).is_err());

        let mut unsupported = synthetic_header(ByteOrder::Little, 1, 1, 1, 12, 0);
        unsupported.extend_from_slice(&[0, 0]);
        let unsupported_file = TempMrc::write("unsupported", &unsupported);
        assert!(matches!(
            MrcReader::new().set_id(&unsupported_file.path),
            Err(BioFormatsError::UnsupportedFormat(_))
        ));

        let truncated = synthetic_header(ByteOrder::Little, 2, 2, 1, 2, 0);
        let truncated_file = TempMrc::write("truncated", &truncated);
        assert!(MrcReader::new().set_id(&truncated_file.path).is_err());
    }

    #[test]
    fn close_clears_reader_state_and_only_series_zero_exists() {
        let bytes = synthetic_file(ByteOrder::Little, 1, 1, 1, 0, 0, &[1]);
        let file = TempMrc::write("close", &bytes);
        let mut reader = MrcReader::new();
        reader.set_id(&file.path).unwrap();
        assert!(matches!(
            reader.set_series(1),
            Err(BioFormatsError::SeriesOutOfRange(1))
        ));
        reader.close().unwrap();
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::NotInitialized)
        ));
        assert!(reader.used_files().is_empty());
    }
}
