/*
 * #%L
 * BSD implementations of Bio-Formats readers and writers
 * %%
 * Copyright (C) 2005 - 2017 Open Microscopy Environment:
 *   - Board of Regents of the University of Wisconsin-Madison
 *   - Glencoe Software, Inc.
 *   - University of Dundee
 * %%
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * 1. Redistributions of source code must retain the above copyright notice,
 *    this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright notice,
 *    this list of conditions and the following disclaimer in the documentation
 *    and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDERS OR CONTRIBUTORS BE
 * LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
 * CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
 * SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
 * INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
 * CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
 * ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
 * POSSIBILITY OF SUCH DAMAGE.
 * #L%
 */

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{destination_prefix, try_zeroed_bytes, validate_region, FormatReader};
use crate::source::{
    map_source_io_error, CompanionReference, SourceHandle, SourceInfo, SourceInput,
};
use flate2::read::GzDecoder;

const NRRD_MAGIC: &[u8] = b"NRRD";

pub struct NrrdReader {
    current_path: Option<PathBuf>,
    header_source: Option<SourceHandle>,
    data_source: Option<SourceHandle>,
    used_sources: Vec<SourceInfo>,
    data_offset: u64,
    encoding: Option<Encoding>,
    metadata: ImageMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Raw,
    Gzip,
}

struct ParsedHeader {
    metadata: ImageMetadata,
    data_file: Option<String>,
    data_offset: u64,
    encoding: Encoding,
}

impl NrrdReader {
    pub fn new() -> Self {
        Self {
            current_path: None,
            header_source: None,
            data_source: None,
            used_sources: Vec::new(),
            data_offset: 0,
            encoding: None,
            metadata: ImageMetadata::default(),
        }
    }

    fn plane_size(&self) -> Result<u64> {
        let samples = u64::from(self.metadata.size_x)
            .checked_mul(u64::from(self.metadata.size_y))
            .and_then(|value| value.checked_mul(u64::from(self.metadata.size_c)))
            .ok_or_else(|| invalid_data("plane sample count overflows u64"))?;
        samples
            .checked_mul(bytes_per_sample(self.metadata.pixel_type)?)
            .ok_or_else(|| invalid_data("plane byte count overflows u64"))
    }

    fn read_raw_region_into(
        &self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        destination: &mut [u8],
    ) -> Result<usize> {
        let source = self
            .data_source
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_pixel = bytes_per_sample(self.metadata.pixel_type)?
            .checked_mul(u64::from(self.metadata.size_c))
            .ok_or_else(|| invalid_data("bytes per pixel overflows u64"))?;
        let source_row_bytes = u64::from(self.metadata.size_x)
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid_data("source row byte count overflows u64"))?;
        let output_row_bytes = u64::from(width)
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid_data("output row byte count overflows u64"))?;
        let output_len = output_row_bytes
            .checked_mul(u64::from(height))
            .ok_or_else(|| invalid_data("output byte count overflows u64"))?;
        let output_len = usize::try_from(output_len)
            .map_err(|_| invalid_data("output byte count does not fit in memory"))?;
        let output_row_bytes_usize = usize::try_from(output_row_bytes)
            .map_err(|_| invalid_data("row byte count does not fit in memory"))?;

        let plane_start = u64::from(plane_index)
            .checked_mul(self.plane_size()?)
            .and_then(|value| self.data_offset.checked_add(value))
            .ok_or_else(|| invalid_data("plane offset overflows u64"))?;
        let x_offset = u64::from(x)
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid_data("X byte offset overflows u64"))?;

        let output = destination_prefix(destination, output_len)?;
        for row in 0..height {
            let source_y = u64::from(y)
                .checked_add(u64::from(row))
                .ok_or_else(|| invalid_data("Y coordinate overflows u64"))?;
            let row_offset = source_y
                .checked_mul(source_row_bytes)
                .and_then(|value| value.checked_add(x_offset))
                .and_then(|value| plane_start.checked_add(value))
                .ok_or_else(|| invalid_data("row offset overflows u64"))?;
            let destination_start = usize::try_from(row)
                .ok()
                .and_then(|value| value.checked_mul(output_row_bytes_usize))
                .ok_or_else(|| invalid_data("destination row offset overflows usize"))?;
            let destination_end = destination_start
                .checked_add(output_row_bytes_usize)
                .ok_or_else(|| invalid_data("destination row end overflows usize"))?;
            source
                .read_at(row_offset, &mut output[destination_start..destination_end])
                .map_err(|error| {
                    if matches!(error, BioFormatsError::SourceRangeOutOfBounds { .. }) {
                        invalid_data("pixel data is truncated")
                    } else {
                        error
                    }
                })?;
        }
        Ok(output_len)
    }

    fn read_gzip_region_into(
        &self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        destination: &mut [u8],
    ) -> Result<usize> {
        let source = self
            .data_source
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_pixel = bytes_per_sample(self.metadata.pixel_type)?
            .checked_mul(u64::from(self.metadata.size_c))
            .ok_or_else(|| invalid_data("bytes per pixel overflows u64"))?;
        let source_row_bytes = u64::from(self.metadata.size_x)
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid_data("source row byte count overflows u64"))?;
        let output_row_bytes = u64::from(width)
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid_data("output row byte count overflows u64"))?;
        let output_len = output_row_bytes
            .checked_mul(u64::from(height))
            .ok_or_else(|| invalid_data("output byte count overflows u64"))?;
        let output_len = usize::try_from(output_len)
            .map_err(|_| invalid_data("output byte count does not fit in memory"))?;
        let output_row_bytes_usize = usize::try_from(output_row_bytes)
            .map_err(|_| invalid_data("row byte count does not fit in memory"))?;
        let x_bytes = u64::from(x)
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| invalid_data("X byte offset overflows u64"))?;
        let trailing_bytes = source_row_bytes
            .checked_sub(x_bytes)
            .and_then(|value| value.checked_sub(output_row_bytes))
            .ok_or_else(|| invalid_data("region row exceeds source row"))?;
        let decoded_start = u64::from(plane_index)
            .checked_mul(self.plane_size()?)
            .and_then(|value| {
                u64::from(y)
                    .checked_mul(source_row_bytes)
                    .and_then(|rows| value.checked_add(rows))
            })
            .ok_or_else(|| invalid_data("decoded plane offset overflows u64"))?;

        let mut file = source.cursor();
        file.seek(SeekFrom::Start(self.data_offset))?;
        let mut decoder = GzDecoder::new(file);
        skip_decoded(&mut decoder, decoded_start)?;
        let output = destination_prefix(destination, output_len)?;
        for row in 0..height {
            skip_decoded(&mut decoder, x_bytes)?;
            let destination_start = usize::try_from(row)
                .ok()
                .and_then(|value| value.checked_mul(output_row_bytes_usize))
                .ok_or_else(|| invalid_data("destination row offset overflows usize"))?;
            let destination_end = destination_start
                .checked_add(output_row_bytes_usize)
                .ok_or_else(|| invalid_data("destination row end overflows usize"))?;
            read_exact_decoded(
                &mut decoder,
                &mut output[destination_start..destination_end],
            )?;
            if row + 1 < height {
                skip_decoded(&mut decoder, trailing_bytes)?;
            }
        }
        Ok(output_len)
    }

    fn region_output_len(&self, width: u32, height: u32) -> Result<usize> {
        let bytes_per_pixel = bytes_per_sample(self.metadata.pixel_type)?
            .checked_mul(u64::from(self.metadata.size_c))
            .ok_or_else(|| invalid_data("bytes per pixel overflows u64"))?;
        let length = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or_else(|| invalid_data("output byte count overflows u64"))?;
        usize::try_from(length)
            .map_err(|_| invalid_data("output byte count does not fit in memory"))
    }

    fn read_region_into(
        &self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        destination: &mut [u8],
    ) -> Result<usize> {
        if self.header_source.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if plane_index >= self.metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        validate_region(&self.metadata, x, y, width, height)?;
        match self.encoding.ok_or(BioFormatsError::NotInitialized)? {
            Encoding::Raw => {
                self.read_raw_region_into(plane_index, x, y, width, height, destination)
            }
            Encoding::Gzip => {
                self.read_gzip_region_into(plane_index, x, y, width, height, destination)
            }
        }
    }

    fn read_region(
        &self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        if self.header_source.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if plane_index >= self.metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        validate_region(&self.metadata, x, y, width, height)?;
        let output_len = self.region_output_len(width, height)?;
        let mut output = try_zeroed_bytes(output_len, "NRRD region buffer")?;
        self.read_region_into(plane_index, x, y, width, height, &mut output)?;
        Ok(output)
    }
}

impl Default for NrrdReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NrrdReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("nrrd") || extension.eq_ignore_ascii_case("nhdr")
            })
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(NRRD_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.set_source(SourceInput::from_path(path)?)
    }

    fn set_source(&mut self, input: SourceInput) -> Result<()> {
        let header_source = input.primary_handle()?;
        let parsed = parse_header(&header_source)?;
        let data_source = if let Some(reference) = parsed.data_file.as_deref() {
            input
                .resolve(&header_source, CompanionReference::Named(reference))?
                .into_iter()
                .next()
                .ok_or_else(|| BioFormatsError::CompanionNotFound {
                    identity: header_source.info().identity().clone(),
                    reference: reference.to_owned(),
                })?
        } else {
            header_source.clone()
        };
        let mut metadata = parsed.metadata;
        metadata.used_files = header_source
            .path()
            .map(Path::to_path_buf)
            .into_iter()
            .collect();
        if data_source.info().identity() != header_source.info().identity() {
            if let Some(path) = data_source.path() {
                metadata.used_files.push(path.to_path_buf());
            }
        }
        self.current_path = header_source.path().map(Path::to_path_buf);
        self.used_sources = vec![header_source.info().clone()];
        if data_source.info().identity() != header_source.info().identity() {
            self.used_sources.push(data_source.info().clone());
        }
        self.header_source = Some(header_source);
        self.data_source = Some(data_source);
        self.data_offset = parsed.data_offset;
        self.encoding = Some(parsed.encoding);
        self.metadata = metadata;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.current_path = None;
        self.header_source = None;
        self.data_source = None;
        self.used_sources.clear();
        self.data_offset = 0;
        self.encoding = None;
        self.metadata = ImageMetadata::default();
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
        &self.metadata
    }

    fn current_file(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    fn used_files(&self) -> Vec<PathBuf> {
        self.metadata.used_files.clone()
    }

    fn used_sources(&self) -> Vec<SourceInfo> {
        self.used_sources.clone()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.read_region(
            plane_index,
            0,
            0,
            self.metadata.size_x,
            self.metadata.size_y,
        )
    }

    fn open_bytes_into(&mut self, plane_index: u32, destination: &mut [u8]) -> Result<usize> {
        self.read_region_into(
            plane_index,
            0,
            0,
            self.metadata.size_x,
            self.metadata.size_y,
            destination,
        )
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        self.read_region(plane_index, x, y, width, height)
    }

    fn open_bytes_region_into(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        destination: &mut [u8],
    ) -> Result<usize> {
        self.read_region_into(plane_index, x, y, width, height, destination)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let width = self.metadata.size_x.min(256);
        let height = self.metadata.size_y.min(256);
        let x = (self.metadata.size_x - width) / 2;
        let y = (self.metadata.size_y - height) / 2;
        self.open_bytes_region(plane_index, x, y, width, height)
    }
}

fn parse_header(source: &SourceHandle) -> Result<ParsedHeader> {
    let mut reader = BufReader::new(source.cursor());
    let mut line = String::new();
    let first_len = reader.read_line(&mut line)?;
    if first_len == 0 || !line.as_bytes().starts_with(NRRD_MAGIC) {
        return Err(BioFormatsError::UnsupportedFormat(
            source.info().name().to_owned(),
        ));
    }
    if line.trim_end_matches(['\r', '\n']).len() < 8 {
        return Err(invalid_data("invalid NRRD magic/version line"));
    }

    let mut header_len =
        u64::try_from(first_len).map_err(|_| invalid_data("header length does not fit u64"))?;
    let mut pixel_type = None;
    let mut dimension = None;
    let mut sizes = None;
    let mut encoding = None;
    let mut endian = None;
    let mut byte_skip = 0_u64;
    let mut data_file = None;
    let mut fields = Vec::new();
    let mut terminated = false;

    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        header_len = header_len
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| invalid_data("header line length does not fit u64"))?,
            )
            .ok_or_else(|| invalid_data("header length overflows u64"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            terminated = true;
            break;
        }
        if trimmed.starts_with('#') {
            continue;
        }
        let (raw_key, raw_value) = trimmed
            .split_once(':')
            .ok_or_else(|| invalid_data(format!("malformed header line: {trimmed}")))?;
        let key = raw_key.trim().to_ascii_lowercase();
        let value = raw_value.trim().trim_start_matches('=').trim();
        fields.push((key.clone(), value.to_owned()));
        match key.as_str() {
            "type" => pixel_type = Some(parse_pixel_type(value)?),
            "dimension" => dimension = Some(parse_positive_u32("dimension", value)?),
            "sizes" => sizes = Some(parse_sizes(value)?),
            "encoding" => {
                encoding = Some(match value.to_ascii_lowercase().as_str() {
                    "raw" => Encoding::Raw,
                    "gzip" | "gz" => Encoding::Gzip,
                    other => {
                        return Err(BioFormatsError::Codec(format!(
                            "NRRD unsupported encoding: {other}"
                        )))
                    }
                })
            }
            "endian" => {
                endian = Some(match value.to_ascii_lowercase().as_str() {
                    "little" => true,
                    "big" => false,
                    _ => return Err(invalid_data(format!("invalid endian value: {value}"))),
                })
            }
            "byte skip" | "byteskip" => {
                byte_skip = value
                    .parse::<u64>()
                    .map_err(|_| invalid_data(format!("invalid byte skip: {value}")))?
            }
            "data file" | "datafile" => {
                let value = value.trim_matches('"');
                if value.eq_ignore_ascii_case("list")
                    || value.to_ascii_lowercase().starts_with("list ")
                {
                    return Err(BioFormatsError::UnsupportedFormat(
                        "NRRD LIST data files are not supported".into(),
                    ));
                }
                data_file = Some(value.to_owned());
            }
            _ => {}
        }
    }

    if data_file.is_none() && !terminated {
        return Err(invalid_data("inline NRRD header has no blank terminator"));
    }
    let pixel_type = pixel_type.ok_or_else(|| invalid_data("missing type field"))?;
    let dimension = dimension.ok_or_else(|| invalid_data("missing dimension field"))?;
    let sizes = sizes.ok_or_else(|| invalid_data("missing sizes field"))?;
    if usize::try_from(dimension).ok() != Some(sizes.len()) {
        return Err(invalid_data(format!(
            "dimension {dimension} does not match {} sizes",
            sizes.len()
        )));
    }
    let encoding = encoding.ok_or_else(|| invalid_data("missing encoding field"))?;
    let (size_x, size_y, size_z, size_c, size_t) = map_dimensions(dimension, &sizes)?;
    let bytes = bytes_per_sample(pixel_type)?;
    let is_little_endian = if bytes == 1 {
        endian.unwrap_or(cfg!(target_endian = "little"))
    } else {
        endian.ok_or_else(|| invalid_data("multi-byte NRRD data requires endian field"))?
    };
    let image_count = size_z
        .checked_mul(size_t)
        .ok_or_else(|| invalid_data("image count overflows u32"))?;
    let bits_per_pixel =
        u8::try_from(bytes * 8).map_err(|_| invalid_data("bits per pixel does not fit u8"))?;
    let data_offset = if data_file.is_some() { 0 } else { header_len }
        .checked_add(byte_skip)
        .ok_or_else(|| invalid_data("data offset overflows u64"))?;

    let mut metadata = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel,
        samples_per_pixel: size_c,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: size_c > 1,
        is_interleaved: true,
        is_indexed: false,
        is_false_color: false,
        is_little_endian,
        ..ImageMetadata::default()
    };
    for (key, value) in fields {
        metadata
            .series_metadata
            .insert(key, MetadataValue::String(value));
    }
    Ok(ParsedHeader {
        metadata,
        data_file,
        data_offset,
        encoding,
    })
}

fn parse_pixel_type(value: &str) -> Result<PixelType> {
    match value.trim().to_ascii_lowercase().as_str() {
        // These signed spellings intentionally follow Bio-Formats' NRRDReader,
        // which exposes 8/16/32-bit integer NRRD samples as unsigned types.
        "char" | "signed char" | "int8" | "int8_t" | "uchar" | "unsigned char" | "uint8"
        | "uint8_t" => Ok(PixelType::Uint8),
        "short" | "short int" | "signed short" | "signed short int" | "int16" | "int16_t"
        | "ushort" | "unsigned short" | "unsigned short int" | "uint16" | "uint16_t" => {
            Ok(PixelType::Uint16)
        }
        "int" | "signed int" | "int32" | "int32_t" | "uint" | "unsigned int" | "uint32"
        | "uint32_t" => Ok(PixelType::Uint32),
        "float" => Ok(PixelType::Float32),
        "double" => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "NRRD unsupported scalar type: {value}"
        ))),
    }
}

fn parse_positive_u32(field: &str, value: &str) -> Result<u32> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| invalid_data(format!("invalid {field}: {value}")))?;
    if parsed == 0 {
        return Err(invalid_data(format!("{field} must be positive")));
    }
    Ok(parsed)
}

fn parse_sizes(value: &str) -> Result<Vec<u32>> {
    value
        .split_whitespace()
        .map(|size| parse_positive_u32("size", size))
        .collect()
}

fn map_dimensions(dimension: u32, sizes: &[u32]) -> Result<(u32, u32, u32, u32, u32)> {
    if !(1..=5).contains(&dimension) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "NRRD dimension {dimension} is not supported"
        )));
    }
    let mut size_x = 1;
    let mut size_y = 1;
    let mut size_z = 1;
    let mut size_c = 1;
    let mut size_t = 1;
    for (index, &size) in sizes.iter().enumerate() {
        if dimension >= 3 && index == 0 && size > 1 && size <= 16 {
            size_c = size;
        } else if index == 0 || (size_c > 1 && index == 1) {
            size_x = size;
        } else if index == 1 || (size_c > 1 && index == 2) {
            size_y = size;
        } else if index == 2 || (size_c > 1 && index == 3) {
            size_z = size;
        } else if index == 3 || (size_c > 1 && index == 4) {
            size_t = size;
        }
    }
    let mapped = u64::from(size_x)
        .checked_mul(u64::from(size_y))
        .and_then(|value| value.checked_mul(u64::from(size_z)))
        .and_then(|value| value.checked_mul(u64::from(size_c)))
        .and_then(|value| value.checked_mul(u64::from(size_t)))
        .ok_or_else(|| invalid_data("mapped dimensions overflow u64"))?;
    let declared = sizes.iter().try_fold(1_u64, |product, &size| {
        product
            .checked_mul(u64::from(size))
            .ok_or_else(|| invalid_data("declared dimensions overflow u64"))
    })?;
    if mapped != declared {
        return Err(BioFormatsError::UnsupportedFormat(
            "NRRD dimensions cannot be represented as X/Y/Z/C/T".into(),
        ));
    }
    Ok((size_x, size_y, size_z, size_c, size_t))
}

fn bytes_per_sample(pixel_type: PixelType) -> Result<u64> {
    let bytes = pixel_type.bytes_per_sample();
    if bytes == 0 {
        Err(BioFormatsError::UnsupportedFormat(
            "NRRD bit-packed samples are not supported".into(),
        ))
    } else {
        u64::try_from(bytes).map_err(|_| invalid_data("sample size does not fit u64"))
    }
}

fn skip_decoded(reader: &mut impl Read, mut count: u64) -> Result<()> {
    let mut scratch = [0_u8; 8192];
    while count > 0 {
        let chunk = usize::try_from(count.min(scratch.len() as u64))
            .map_err(|_| invalid_data("gzip skip length does not fit usize"))?;
        let read = reader
            .read(&mut scratch[..chunk])
            .map_err(nrrd_gzip_error)?;
        if read == 0 {
            return Err(invalid_data("gzip pixel data is truncated"));
        }
        count -=
            u64::try_from(read).map_err(|_| invalid_data("gzip read length does not fit u64"))?;
    }
    Ok(())
}

fn read_exact_decoded(reader: &mut impl Read, destination: &mut [u8]) -> Result<()> {
    reader.read_exact(destination).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof && error.get_ref().is_none() {
            invalid_data("gzip pixel data is truncated")
        } else {
            nrrd_gzip_error(error)
        }
    })
}

fn nrrd_gzip_error(error: std::io::Error) -> BioFormatsError {
    match map_source_io_error(error) {
        BioFormatsError::Io(error) => {
            BioFormatsError::Codec(format!("NRRD gzip decode failed: {error}"))
        }
        source_error => source_error,
    }
}

fn invalid_data(message: impl Into<String>) -> BioFormatsError {
    BioFormatsError::InvalidData(format!("NRRD: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::NrrdReader;
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("bioformats_nrrd_{label}_{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn detects_nrrd_by_magic_and_name() {
        let reader = NrrdReader::new();

        assert!(reader.is_this_type_by_bytes(b"NRRD0005\n"));
        assert!(!reader.is_this_type_by_bytes(b"not nrrd"));
        assert!(reader.is_this_type_by_name(Path::new("image.NRRD")));
        assert!(reader.is_this_type_by_name(Path::new("image.NhDr")));
        assert!(!reader.is_this_type_by_name(Path::new("image.raw")));
    }

    #[test]
    fn integer_type_mapping_matches_java_bioformats() {
        assert_eq!(super::parse_pixel_type("int8").unwrap(), PixelType::Uint8);
        assert_eq!(
            super::parse_pixel_type("signed short").unwrap(),
            PixelType::Uint16
        );
        assert_eq!(
            super::parse_pixel_type("int32_t").unwrap(),
            PixelType::Uint32
        );
    }

    #[test]
    fn reads_inline_raw_scalar_plane() {
        let directory = TestDir::new("inline");
        let mut contents = b"NRRD0005\n\
type: uint16\n\
dimension: 2\n\
sizes: 2 2\n\
endian: little\n\
encoding: raw\n\
\n"
        .to_vec();
        contents.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
        let path = directory.write("inline.nrrd", &contents);

        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(reader.metadata().samples_per_pixel, 1);
        assert!(reader.metadata().is_little_endian);
        assert_eq!(reader.open_bytes(0).unwrap(), [1, 0, 2, 0, 3, 0, 4, 0]);
        let mut destination = [0xaa; 10];
        assert_eq!(reader.open_bytes_into(0, &mut destination).unwrap(), 8);
        assert_eq!(&destination[..8], &[1, 0, 2, 0, 3, 0, 4, 0]);
        assert_eq!(&destination[8..], &[0xaa, 0xaa]);
    }

    #[test]
    fn reads_detached_raw_with_byte_skip_and_vector_axis() {
        let directory = TestDir::new("detached");
        let raw_path = directory.write(
            "pixels.raw",
            &[99, 98, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        );
        let header_path = directory.write(
            "image.nhdr",
            b"NRRD0005\n\
type: uint8\n\
dimension: 4\n\
sizes: 3 2 1 2\n\
encoding: raw\n\
byte skip: 2\n\
data file: pixels.raw\n\
\n",
        );

        let mut reader = NrrdReader::new();
        reader.set_id(&header_path).unwrap();

        assert_eq!(reader.metadata().size_c, 3);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().samples_per_pixel, 3);
        assert!(reader.metadata().is_rgb);
        assert!(reader.metadata().is_interleaved);
        assert_eq!(reader.used_files(), [header_path, raw_path]);
        assert_eq!(reader.open_bytes(1).unwrap(), [7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn reads_inline_gzip_plane() {
        let directory = TestDir::new("gzip");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[10, 20, 30, 40, 50, 60]).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut contents = b"NRRD0005\n\
type: uint8\n\
dimension: 2\n\
sizes: 3 2\n\
encoding: gz\n\
\n"
        .to_vec();
        contents.extend_from_slice(&compressed);
        let path = directory.write("compressed.nrrd", &contents);

        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.open_bytes(0).unwrap(), [10, 20, 30, 40, 50, 60]);
        let mut destination = [0xaa; 8];
        assert_eq!(
            reader
                .open_bytes_region_into(0, 1, 0, 2, 2, &mut destination)
                .unwrap(),
            4
        );
        assert_eq!(&destination[..4], &[20, 30, 50, 60]);
        assert_eq!(&destination[4..], &[0xaa; 4]);
    }

    #[test]
    fn raw_region_does_not_materialize_full_plane() {
        let directory = TestDir::new("large_region");
        let mut contents = b"NRRD0005\n\
type: uint8\n\
dimension: 2\n\
sizes: 2 4294967295\n\
encoding: raw\n\
\n"
        .to_vec();
        contents.extend_from_slice(&[41, 42]);
        let path = directory.write("large.nrrd", &contents);
        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.open_bytes_region(0, 0, 0, 2, 1).unwrap(), [41, 42]);
    }

    #[test]
    fn invalid_region_is_recoverable() {
        let directory = TestDir::new("invalid_region");
        let mut contents = b"NRRD0005\n\
type: uint8\n\
dimension: 2\n\
sizes: 2 2\n\
encoding: raw\n\
\n"
        .to_vec();
        contents.extend_from_slice(&[1, 2, 3, 4]);
        let path = directory.write("region.nrrd", &contents);
        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();

        assert!(matches!(
            reader.open_bytes_region(0, 1, 1, 2, 1),
            Err(crate::common::error::BioFormatsError::InvalidRegion { .. })
        ));
        assert!(matches!(
            reader.open_bytes_region(0, 0, 0, 0, 1),
            Err(crate::common::error::BioFormatsError::InvalidRegionShape { .. })
        ));
        assert!(matches!(
            reader.open_bytes_region(0, 0, 0, u32::MAX, u32::MAX),
            Err(crate::common::error::BioFormatsError::InvalidRegionShape { .. })
                | Err(crate::common::error::BioFormatsError::InvalidRegion { .. })
        ));
        assert_eq!(reader.open_bytes_region(0, 0, 0, 1, 1).unwrap(), [1]);
    }

    #[test]
    fn truncated_pixel_data_returns_error() {
        let directory = TestDir::new("truncated");
        let mut contents = b"NRRD0005\n\
type: uint8\n\
dimension: 2\n\
sizes: 2 2\n\
encoding: raw\n\
\n"
        .to_vec();
        contents.extend_from_slice(&[1, 2, 3]);
        let path = directory.write("truncated.nrrd", &contents);
        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();

        assert!(matches!(
            reader.open_bytes(0),
            Err(crate::common::error::BioFormatsError::InvalidData(_))
        ));
    }
}
