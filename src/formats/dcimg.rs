/*
 * #%L
 * OME Bio-Formats package for reading and converting biological file formats.
 * %%
 * Copyright (C) 2005 - 2024 Open Microscopy Environment:
 *   - Board of Regents of the University of Wisconsin-Madison
 *   - Glencoe Software, Inc.
 *   - University of Dundee
 * %%
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as
 * published by the Free Software Foundation, either version 2 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public
 * License along with this program.  If not, see
 * <http://www.gnu.org/licenses/gpl-2.0.html>.
 * #L%
 */

//! Hamamatsu DCIMG reader.
//!
//! Ported from OME Bio-Formats' `DCIMGReader.java`. The implementation also
//! incorporates the bounded-region and overflow checks exercised by the
//! `bioformats-zig` DCIMG reader.
//!
//! Like Java Bio-Formats' default grouping mode, sorted sibling DCIMG files are
//! exposed as Z while frames inside each file are exposed as T.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{destination_prefix, try_zeroed_bytes, validate_region, FormatReader};
use crate::source::{SourceHandle, SourceInfo, SourceInput};

const SIGNATURE: &[u8; 5] = b"DCIMG";
const VERSION_0: u32 = 0x0000_0007;
const VERSION_1: u32 = 0x0100_0000;
const PIXEL_MONO8: u32 = 1;
const PIXEL_MONO16: u32 = 2;
const MAIN_HEADER_LEN: usize = 72;
const VERSION_0_SESSION_LEN: usize = 80;
const VERSION_1_SESSION_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DcimgVersion {
    V0,
    V1,
}

#[derive(Debug, Clone, Copy)]
struct DcimgHeader {
    version_number: u32,
    data_start: u64,
    width: u32,
    height: u32,
    frame_count: u32,
    pixel_type: PixelType,
    bytes_per_image: u64,
    frame_footer_size: u64,
    four_pixel_correction: Option<FourPixelCorrection>,
}

impl DcimgHeader {
    fn bytes_per_sample(self) -> u64 {
        self.pixel_type.bytes_per_sample() as u64
    }

    fn frame_stride(self) -> Result<u64> {
        checked_add(self.bytes_per_image, self.frame_footer_size, "frame stride")
    }

    fn frame_start(self, plane_index: u32) -> Result<u64> {
        let relative = checked_mul(self.frame_stride()?, u64::from(plane_index), "frame offset")?;
        checked_add(self.data_start, relative, "frame start")
    }
}

#[derive(Debug, Clone, Copy)]
struct FourPixelCorrection {
    output_row: u32,
    file_offset: u64,
}

#[derive(Debug)]
struct DcimgPart {
    source: SourceHandle,
    header: DcimgHeader,
}

/// Reader for Hamamatsu DCIMG version 0 and version 1 files.
pub struct DcimgReader {
    parts: Vec<DcimgPart>,
    path: Option<PathBuf>,
    used_files: Vec<PathBuf>,
    metadata: Option<ImageMetadata>,
}

impl DcimgReader {
    pub fn new() -> Self {
        Self {
            parts: Vec::new(),
            path: None,
            used_files: Vec::new(),
            metadata: None,
        }
    }

    fn initialized_metadata(&self) -> Result<&ImageMetadata> {
        self.metadata
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)
    }

    fn plane_source(&self, plane_index: u32) -> Result<(&SourceHandle, DcimgHeader, u32)> {
        let metadata = self.initialized_metadata()?;
        if plane_index >= metadata.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (z, c, t) = metadata.get_zct_coords(plane_index);
        if c != 0 {
            return Err(invalid_data(format!(
                "plane {plane_index} mapped to unexpected channel {c}"
            )));
        }
        let z = usize::try_from(z).map_err(|_| invalid_data("Z index does not fit in memory"))?;
        let part = self
            .parts
            .get(z)
            .ok_or_else(|| invalid_data(format!("plane {plane_index} mapped to missing Z {z}")))?;
        let header = part.header;
        if t >= header.frame_count {
            return Err(invalid_data(format!(
                "plane {plane_index} mapped to frame {t}, but Z {z} has {} frames",
                header.frame_count
            )));
        }
        Ok((&part.source, header, t))
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
        let metadata = self.initialized_metadata()?;
        validate_region(metadata, x, y, width, height)?;
        let (source, header, frame_index) = self.plane_source(plane_index)?;
        let bytes_per_sample = header.bytes_per_sample();
        let source_row_bytes = checked_mul(
            u64::from(header.width),
            bytes_per_sample,
            "source row byte count",
        )?;
        let output_row_bytes =
            checked_mul(u64::from(width), bytes_per_sample, "region row byte count")?;
        let output_len = checked_mul(output_row_bytes, u64::from(height), "region byte count")?;
        let output_len = usize_from_u64(output_len, "region byte count")?;
        let output_row_bytes = usize_from_u64(output_row_bytes, "region row byte count")?;
        let frame_start = header.frame_start(frame_index)?;

        let output = destination_prefix(destination, output_len)?;

        // Bio-Formats treats `y` as a row in the stored image and reverses only
        // the requested row window while copying it into the caller's buffer.
        for output_row in 0..height {
            let source_row = u64::from(y) + u64::from(height - 1 - output_row);
            let source_row_offset = checked_mul(source_row, source_row_bytes, "source row offset")?;
            let source_x_offset =
                checked_mul(u64::from(x), bytes_per_sample, "source column offset")?;
            let source_offset = checked_add(
                checked_add(frame_start, source_row_offset, "source row start")?,
                source_x_offset,
                "source region start",
            )?;
            let destination_start = usize_from_u64(
                checked_mul(
                    u64::from(output_row),
                    output_row_bytes as u64,
                    "destination row offset",
                )?,
                "destination row offset",
            )?;
            let destination_end = destination_start
                .checked_add(output_row_bytes)
                .ok_or_else(|| invalid_data("destination row range overflows"))?;
            let destination = &mut output[destination_start..destination_end];
            source.read_at(source_offset, destination)?;

            if let Some(correction) = header.four_pixel_correction {
                if output_row == correction.output_row && x < 4 {
                    let corrected_pixels = width.min(4 - x);
                    let corrected_bytes = usize_from_u64(
                        checked_mul(
                            u64::from(corrected_pixels),
                            bytes_per_sample,
                            "four-pixel correction byte count",
                        )?,
                        "four-pixel correction byte count",
                    )?;
                    let correction_offset = checked_add(
                        correction.file_offset,
                        checked_mul(
                            u64::from(x),
                            bytes_per_sample,
                            "four-pixel correction column offset",
                        )?,
                        "four-pixel correction offset",
                    )?;
                    source.read_at(correction_offset, &mut destination[..corrected_bytes])?;
                }
            }
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
        let metadata = self.initialized_metadata()?;
        validate_region(metadata, x, y, width, height)?;
        let (_, header, _) = self.plane_source(plane_index)?;
        let output_len = checked_mul(
            checked_mul(
                u64::from(width),
                header.bytes_per_sample(),
                "region row byte count",
            )?,
            u64::from(height),
            "region byte count",
        )?;
        let output_len = usize_from_u64(output_len, "region byte count")?;
        let mut output = try_zeroed_bytes(output_len, "DCIMG region buffer")?;
        self.read_region_into(plane_index, x, y, width, height, &mut output)?;
        Ok(output)
    }
}

impl Default for DcimgReader {
    fn default() -> Self {
        Self::new()
    }
}

fn is_dcimg_named(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("dcimg"))
}

fn dcimg_sort_name(name: &str) -> &OsStr {
    let path = Path::new(name);
    path.file_name().unwrap_or(path.as_os_str())
}

fn validate_group_member(
    primary: DcimgHeader,
    candidate: DcimgHeader,
    candidate_name: &str,
) -> Result<()> {
    if candidate.version_number != primary.version_number
        || candidate.width != primary.width
        || candidate.height != primary.height
        || candidate.frame_count != primary.frame_count
        || candidate.pixel_type != primary.pixel_type
    {
        return Err(invalid_data(format!(
            "group member {candidate_name:?} is incompatible with the primary file: expected version {:#010x}, {}x{}, {} frames, {:?}; found version {:#010x}, {}x{}, {} frames, {:?}",
            primary.version_number,
            primary.width,
            primary.height,
            primary.frame_count,
            primary.pixel_type,
            candidate.version_number,
            candidate.width,
            candidate.height,
            candidate.frame_count,
            candidate.pixel_type,
        )));
    }
    Ok(())
}

impl FormatReader for DcimgReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dcimg"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(SIGNATURE)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.set_source(SourceInput::from_path(path)?)
    }

    fn set_source(&mut self, input: SourceInput) -> Result<()> {
        let primary = input.primary_handle()?;
        let primary_identity = primary.info().identity().clone();
        let path = primary.path().map(Path::to_path_buf);
        let primary_header = {
            let mut cursor = primary.cursor();
            parse_header(&mut cursor, primary.info().len())?
        };

        let mut siblings = input.resolve_siblings_where(&primary, is_dcimg_named)?;

        // Choose one deterministic logical-name representative for every
        // sibling identity. The caller's primary handle is retained separately
        // so an alias returned by the resolver cannot discard its path/name.
        siblings.sort_by(|left, right| {
            left.info()
                .identity()
                .cmp(right.info().identity())
                .then_with(|| {
                    dcimg_sort_name(left.info().name()).cmp(dcimg_sort_name(right.info().name()))
                })
                .then_with(|| left.info().name().cmp(right.info().name()))
        });
        let mut identities = HashSet::from([primary_identity.clone()]);
        siblings.retain(|source| identities.insert(source.info().identity().clone()));
        let mut sources = vec![primary.clone()];
        sources.extend(siblings);

        let mut dcimg_sources = Vec::new();
        dcimg_sources
            .try_reserve_exact(sources.len())
            .map_err(|error| invalid_data(format!("cannot allocate source list: {error}")))?;
        for source in sources {
            let is_primary = source.info().identity() == &primary_identity;
            if is_primary || source.read_prefix(SIGNATURE.len())?.starts_with(SIGNATURE) {
                dcimg_sources.push(source);
            }
        }
        dcimg_sources.sort_by(|left, right| {
            dcimg_sort_name(left.info().name())
                .cmp(dcimg_sort_name(right.info().name()))
                .then_with(|| left.info().name().cmp(right.info().name()))
                .then_with(|| left.info().identity().cmp(right.info().identity()))
        });

        let mut parts = Vec::new();
        parts
            .try_reserve_exact(dcimg_sources.len())
            .map_err(|error| invalid_data(format!("cannot allocate DCIMG part list: {error}")))?;
        for source in dcimg_sources {
            let candidate = if source.info().identity() == &primary_identity {
                primary_header
            } else {
                let mut cursor = source.cursor();
                parse_header(&mut cursor, source.info().len())?
            };
            validate_group_member(primary_header, candidate, source.info().name())?;
            parts.push(DcimgPart {
                source,
                header: candidate,
            });
        }

        let header = primary_header;
        let size_z = u32::try_from(parts.len())
            .map_err(|_| invalid_data("grouped file count exceeds u32"))?;
        let image_count = size_z
            .checked_mul(header.frame_count)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        let used_files = parts
            .iter()
            .filter_map(|part| part.source.path().map(Path::to_path_buf))
            .collect::<Vec<_>>();

        let mut metadata = ImageMetadata {
            size_x: header.width,
            size_y: header.height,
            size_z,
            size_c: 1,
            size_t: header.frame_count,
            pixel_type: header.pixel_type,
            bits_per_pixel: (header.bytes_per_sample() * 8) as u8,
            samples_per_pixel: 1,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_false_color: false,
            is_little_endian: true,
            used_files: used_files.clone(),
            ..ImageMetadata::default()
        };
        metadata.series_metadata.insert(
            "Version".to_string(),
            MetadataValue::Int(i64::from(header.version_number)),
        );

        self.parts = parts;
        self.path = path;
        self.used_files = used_files;
        self.metadata = Some(metadata);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.parts.clear();
        self.path = None;
        self.used_files.clear();
        self.metadata = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if self.metadata.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if series != 0 {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        Ok(())
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metadata.as_ref().expect("set_id not called")
    }

    fn current_file(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn used_files(&self) -> Vec<PathBuf> {
        self.used_files.clone()
    }

    fn used_sources(&self) -> Vec<SourceInfo> {
        self.parts
            .iter()
            .map(|part| part.source.info().clone())
            .collect()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let metadata = self.initialized_metadata()?;
        self.read_region(plane_index, 0, 0, metadata.size_x, metadata.size_y)
    }

    fn open_bytes_into(&mut self, plane_index: u32, destination: &mut [u8]) -> Result<usize> {
        let metadata = self.initialized_metadata()?;
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
        let metadata = self.initialized_metadata()?;
        let width = metadata.size_x.min(256);
        let height = metadata.size_y.min(256);
        let x = (metadata.size_x - width) / 2;
        let y = (metadata.size_y - height) / 2;
        self.read_region(plane_index, x, y, width, height)
    }
}

fn parse_header<R: Read + Seek>(reader: &mut R, file_len: u64) -> Result<DcimgHeader> {
    let main = read_array_at::<MAIN_HEADER_LEN, _>(reader, 0, file_len, "main header")?;
    if !main.starts_with(SIGNATURE) {
        return Err(invalid_data("not a DCIMG file"));
    }

    let version_number = read_u32(&main, 8);
    let version = match version_number {
        VERSION_0 => DcimgVersion::V0,
        VERSION_1 => DcimgVersion::V1,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "DCIMG version {other:#010x}"
            )));
        }
    };

    let header_size = u64::from(read_u32(&main, 40));
    let file_size = read_u32(&main, 48);
    let repeated_file_size = read_u32(&main, 64);
    if file_size != repeated_file_size {
        return Err(invalid_data("header file sizes do not match"));
    }

    match version {
        DcimgVersion::V0 => parse_version_0(reader, file_len, version_number, header_size),
        DcimgVersion::V1 => parse_version_1(reader, file_len, version_number, header_size),
    }
}

fn parse_version_0<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    version_number: u32,
    header_size: u64,
) -> Result<DcimgHeader> {
    let session = read_array_at::<VERSION_0_SESSION_LEN, _>(
        reader,
        header_size,
        file_len,
        "version 0 session header",
    )?;
    let frame_count = positive_i32(read_i32(&session, 32), "frame count")?;
    let pixel_type = parse_pixel_type(read_i32(&session, 36))?;
    let width = positive_i32(read_i32(&session, 44), "width")?;
    let bytes_per_row = u64::from(read_u32(&session, 48));
    let height = positive_i32(read_i32(&session, 52), "height")?;
    let bytes_per_image = u64::from(read_u32(&session, 56));
    let data_offset = non_negative_i32(read_i32(&session, 68), "data offset")?;
    let offset_to_footer = non_negative_i64(read_i64(&session, 72), "footer offset")?;
    let data_start = checked_add(header_size, data_offset, "pixel data start")?;

    validate_frame_layout(
        file_len,
        data_start,
        width,
        height,
        frame_count,
        pixel_type,
        bytes_per_image,
        0,
    )?;

    let footer_offset = checked_add(header_size, offset_to_footer, "version 0 footer")?;
    let first_footer =
        read_array_at::<16, _>(reader, footer_offset, file_len, "version 0 first footer")?;
    let footer_version = read_u32(&first_footer, 0);
    if footer_version != version_number {
        return Err(invalid_data(format!(
            "header version {version_number} does not match footer version {footer_version}"
        )));
    }
    let second_footer_offset =
        non_negative_i64(read_i64(&first_footer, 8), "second footer offset")?;
    let second_footer_start = checked_add(
        footer_offset,
        second_footer_offset,
        "version 0 second footer",
    )?;
    let second_footer = read_array_at::<112, _>(
        reader,
        second_footer_start,
        file_len,
        "version 0 second footer",
    )?;
    let offset_to_four_pixels = read_i64(&second_footer, 88);
    let four_pixel_offset_in_frame = u64::from(read_u32(&second_footer, 100));
    let four_pixel_size = read_i64(&second_footer, 104);
    if four_pixel_size < 0 {
        return Err(invalid_data("negative four-pixel correction size"));
    }

    let four_pixel_correction = if four_pixel_size > 0 {
        if bytes_per_row == 0 {
            return Err(invalid_data(
                "zero bytes-per-row with four-pixel correction",
            ));
        }
        let output_row = four_pixel_offset_in_frame
            .checked_div(bytes_per_row)
            .and_then(|line| line.checked_add(1))
            .ok_or_else(|| invalid_data("four-pixel correction row overflows"))?;
        let output_row = u32::try_from(output_row)
            .map_err(|_| invalid_data("four-pixel correction row is too large"))?;
        if output_row >= height {
            return Err(invalid_data("four-pixel correction row is out of bounds"));
        }
        let correction_bytes = checked_mul(
            u64::from(width.min(4)),
            pixel_type.bytes_per_sample() as u64,
            "four-pixel correction size",
        )?;
        if (four_pixel_size as u64) < correction_bytes {
            return Err(invalid_data("four-pixel correction record is too small"));
        }
        let file_offset = checked_add(
            footer_offset,
            non_negative_i64(offset_to_four_pixels, "four-pixel data offset")?,
            "four-pixel correction file offset",
        )?;
        require_file_range(
            file_offset,
            correction_bytes,
            file_len,
            "four-pixel correction",
        )?;
        Some(FourPixelCorrection {
            output_row,
            file_offset,
        })
    } else {
        None
    };

    Ok(DcimgHeader {
        version_number,
        data_start,
        width,
        height,
        frame_count,
        pixel_type,
        bytes_per_image,
        frame_footer_size: 0,
        four_pixel_correction,
    })
}

fn parse_version_1<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    version_number: u32,
    header_size: u64,
) -> Result<DcimgHeader> {
    let session = read_array_at::<VERSION_1_SESSION_LEN, _>(
        reader,
        header_size,
        file_len,
        "version 1 session header",
    )?;
    let frame_count = positive_i32(read_i32(&session, 60), "frame count")?;
    let pixel_type = parse_pixel_type(read_i32(&session, 64))?;
    let width = positive_i32(read_i32(&session, 72), "width")?;
    let height = positive_i32(read_i32(&session, 76), "height")?;
    let bytes_per_image = u64::from(read_u32(&session, 84));
    let data_offset = non_negative_i64(read_i64(&session, 96), "data offset")?;
    let frame_footer_size = u64::from(read_u32(&session, 124));
    let data_start = checked_add(header_size, data_offset, "pixel data start")?;

    validate_frame_layout(
        file_len,
        data_start,
        width,
        height,
        frame_count,
        pixel_type,
        bytes_per_image,
        frame_footer_size,
    )?;

    let four_pixel_correction = if frame_footer_size == 32 || frame_footer_size >= 512 {
        let output_row = if height % 2 == 0 {
            height / 2
        } else {
            height / 2 + 1
        };
        let file_offset = checked_add(
            checked_add(data_start, bytes_per_image, "first frame footer")?,
            12,
            "four-pixel correction file offset",
        )?;
        let correction_bytes = checked_mul(
            u64::from(width.min(4)),
            pixel_type.bytes_per_sample() as u64,
            "four-pixel correction size",
        )?;
        require_file_range(
            file_offset,
            correction_bytes,
            file_len,
            "four-pixel correction",
        )?;
        Some(FourPixelCorrection {
            output_row,
            file_offset,
        })
    } else {
        None
    };

    Ok(DcimgHeader {
        version_number,
        data_start,
        width,
        height,
        frame_count,
        pixel_type,
        bytes_per_image,
        frame_footer_size,
        four_pixel_correction,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_frame_layout(
    file_len: u64,
    data_start: u64,
    width: u32,
    height: u32,
    frame_count: u32,
    pixel_type: PixelType,
    bytes_per_image: u64,
    frame_footer_size: u64,
) -> Result<()> {
    let pixels = checked_mul(u64::from(width), u64::from(height), "pixel count")?;
    let expected_image_bytes = checked_mul(
        pixels,
        pixel_type.bytes_per_sample() as u64,
        "image byte count",
    )?;
    if bytes_per_image < expected_image_bytes {
        return Err(invalid_data(format!(
            "bytes per image {bytes_per_image} is smaller than {expected_image_bytes}"
        )));
    }
    let frame_stride = checked_add(bytes_per_image, frame_footer_size, "frame stride")?;
    let all_frames = checked_mul(frame_stride, u64::from(frame_count), "frame data size")?;
    require_file_range(data_start, all_frames, file_len, "frame data")
}

fn parse_pixel_type(value: i32) -> Result<PixelType> {
    match value {
        value if value == PIXEL_MONO8 as i32 => Ok(PixelType::Uint8),
        value if value == PIXEL_MONO16 as i32 => Ok(PixelType::Uint16),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "DCIMG pixel type {other}"
        ))),
    }
}

fn positive_i32(value: i32, field: &str) -> Result<u32> {
    u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_data(format!("{field} must be positive, got {value}")))
}

fn non_negative_i32(value: i32, field: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| invalid_data(format!("{field} must be non-negative, got {value}")))
}

fn non_negative_i64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| invalid_data(format!("{field} must be non-negative, got {value}")))
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| invalid_data(format!("{field} overflows")))
}

fn checked_mul(left: u64, right: u64, field: &str) -> Result<u64> {
    left.checked_mul(right)
        .ok_or_else(|| invalid_data(format!("{field} overflows")))
}

fn usize_from_u64(value: u64, field: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| invalid_data(format!("{field} does not fit in memory")))
}

fn require_file_range(offset: u64, length: u64, file_len: u64, field: &str) -> Result<()> {
    let end = checked_add(offset, length, field)?;
    if end > file_len {
        return Err(invalid_data(format!(
            "truncated {field}: byte range {offset}..{end} exceeds file length {file_len}"
        )));
    }
    Ok(())
}

fn read_array_at<const N: usize, R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    file_len: u64,
    field: &str,
) -> Result<[u8; N]> {
    require_file_range(offset, N as u64, file_len, field)?;
    let mut bytes = [0; N];
    read_exact_at(reader, offset, &mut bytes, field)?;
    Ok(bytes)
}

fn read_exact_at<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    destination: &mut [u8],
    field: &str,
) -> Result<()> {
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::from)?;
    reader.read_exact(destination).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            invalid_data(format!("truncated {field}"))
        } else {
            BioFormatsError::Io(error)
        }
    })
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed header"))
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed header"))
}

fn read_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed header"))
}

fn invalid_data(message: impl Into<String>) -> BioFormatsError {
    BioFormatsError::InvalidData(format!("DCIMG: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("bioformats-rs-dcimg-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.path.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct TestFile {
        _directory: TestDirectory,
        path: PathBuf,
    }

    impl TestFile {
        fn new(bytes: &[u8]) -> Self {
            let directory = TestDirectory::new();
            let path = directory.write("sample.dcimg", bytes);
            Self {
                _directory: directory,
                path,
            }
        }
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i32(bytes: &mut [u8], offset: usize, value: i32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn finish_main_header(bytes: &mut [u8]) {
        let file_len = u32::try_from(bytes.len()).unwrap();
        put_u32(bytes, 48, file_len);
        put_u32(bytes, 64, file_len);
    }

    fn version_1_file(
        width: u32,
        height: u32,
        pixel_type: u32,
        footer_size: u32,
        frames: &[&[u8]],
    ) -> Vec<u8> {
        let header_size = 128usize;
        let data_offset = 128usize;
        let bytes_per_sample = if pixel_type == PIXEL_MONO16 { 2 } else { 1 };
        let bytes_per_image = width as usize * height as usize * bytes_per_sample;
        let mut bytes = vec![0; header_size + data_offset];
        bytes[..SIGNATURE.len()].copy_from_slice(SIGNATURE);
        put_u32(&mut bytes, 8, VERSION_1);
        put_u32(&mut bytes, 40, header_size as u32);
        put_i32(&mut bytes, header_size + 60, frames.len() as i32);
        put_i32(&mut bytes, header_size + 64, pixel_type as i32);
        put_i32(&mut bytes, header_size + 72, width as i32);
        put_i32(&mut bytes, header_size + 76, height as i32);
        put_u32(&mut bytes, header_size + 84, bytes_per_image as u32);
        put_i64(&mut bytes, header_size + 96, data_offset as i64);
        put_u32(&mut bytes, header_size + 124, footer_size);
        for frame in frames {
            assert_eq!(frame.len(), bytes_per_image);
            bytes.extend_from_slice(frame);
            bytes.resize(bytes.len() + footer_size as usize, 0xa5);
        }
        finish_main_header(&mut bytes);
        bytes
    }

    fn version_0_file(
        width: u32,
        height: u32,
        pixel_type: u32,
        frames: &[&[u8]],
        correction: Option<(u32, &[u8])>,
    ) -> Vec<u8> {
        let header_size = 120usize;
        let data_offset = 112usize;
        let bytes_per_sample = if pixel_type == PIXEL_MONO16 { 2 } else { 1 };
        let bytes_per_row = width as usize * bytes_per_sample;
        let bytes_per_image = bytes_per_row * height as usize;
        let mut bytes = vec![0; header_size + data_offset];
        bytes[..SIGNATURE.len()].copy_from_slice(SIGNATURE);
        put_u32(&mut bytes, 8, VERSION_0);
        put_u32(&mut bytes, 40, header_size as u32);
        put_i32(&mut bytes, header_size + 32, frames.len() as i32);
        put_i32(&mut bytes, header_size + 36, pixel_type as i32);
        put_i32(&mut bytes, header_size + 44, width as i32);
        put_u32(&mut bytes, header_size + 48, bytes_per_row as u32);
        put_i32(&mut bytes, header_size + 52, height as i32);
        put_u32(&mut bytes, header_size + 56, bytes_per_image as u32);
        put_i32(&mut bytes, header_size + 68, data_offset as i32);
        for frame in frames {
            assert_eq!(frame.len(), bytes_per_image);
            bytes.extend_from_slice(frame);
        }

        let footer_offset = bytes.len();
        put_i64(
            &mut bytes,
            header_size + 72,
            (footer_offset - header_size) as i64,
        );
        let second_footer_offset = 32usize;
        let correction_offset = second_footer_offset + 112;
        bytes.resize(footer_offset + correction_offset, 0);
        put_u32(&mut bytes, footer_offset, VERSION_0);
        put_i64(&mut bytes, footer_offset + 8, second_footer_offset as i64);
        put_i64(
            &mut bytes,
            footer_offset + second_footer_offset + 88,
            correction_offset as i64,
        );
        if let Some((output_row, correction_bytes)) = correction {
            let offset_in_frame = output_row
                .checked_sub(1)
                .unwrap()
                .checked_mul(bytes_per_row as u32)
                .unwrap();
            put_u32(
                &mut bytes,
                footer_offset + second_footer_offset + 100,
                offset_in_frame,
            );
            put_i64(
                &mut bytes,
                footer_offset + second_footer_offset + 104,
                correction_bytes.len() as i64,
            );
            bytes.extend_from_slice(correction_bytes);
        }
        finish_main_header(&mut bytes);
        bytes
    }

    #[test]
    fn reads_version_1_mono8_frames_with_java_row_orientation() {
        let frame_0 = [1, 2, 3, 4, 5, 6];
        let frame_1 = [7, 8, 9, 10, 11, 12];
        let file = TestFile::new(&version_1_file(3, 2, PIXEL_MONO8, 0, &[&frame_0, &frame_1]));
        let mut reader = DcimgReader::new();
        assert!(reader.is_this_type_by_name(&file.path));
        assert!(reader.is_this_type_by_bytes(b"DCIMG"));
        reader.set_id(&file.path).unwrap();

        assert_eq!(reader.metadata().size_t, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(reader.metadata().samples_per_pixel, 1);
        assert_eq!(reader.metadata().dimension_order, DimensionOrder::XYZCT);
        assert_eq!(reader.open_bytes(0).unwrap(), [4, 5, 6, 1, 2, 3]);
        assert_eq!(reader.open_bytes(1).unwrap(), [10, 11, 12, 7, 8, 9]);
        let mut destination = [0xaa; 8];
        assert_eq!(reader.open_bytes_into(0, &mut destination).unwrap(), 6);
        assert_eq!(&destination[..6], &[4, 5, 6, 1, 2, 3]);
        assert_eq!(&destination[6..], &[0xaa, 0xaa]);
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
        assert_eq!(reader.used_files(), vec![file.path.clone()]);
    }

    #[test]
    fn groups_sorted_files_as_z_before_t_from_either_member() {
        let directory = TestDirectory::new();
        let first = directory.write(
            "camera-000.dcimg",
            &version_1_file(
                3,
                2,
                PIXEL_MONO8,
                0,
                &[&[1, 2, 3, 4, 5, 6], &[11, 12, 13, 14, 15, 16]],
            ),
        );
        let second = directory.write(
            "camera-001.DCIMG",
            &version_1_file(
                3,
                2,
                PIXEL_MONO8,
                0,
                &[&[21, 22, 23, 24, 25, 26], &[31, 32, 33, 34, 35, 36]],
            ),
        );
        directory.write("not-an-image.dcimg", b"not a DCIMG file");

        for opened_path in [&first, &second] {
            let mut reader = DcimgReader::new();
            reader.set_id(opened_path).unwrap();

            assert_eq!(reader.current_file(), Some(opened_path.as_path()));
            assert_eq!(reader.used_files(), vec![first.clone(), second.clone()]);
            assert_eq!(
                reader.metadata().used_files,
                vec![first.clone(), second.clone()]
            );
            assert_eq!(reader.used_sources().len(), 2);
            assert_eq!(
                (
                    reader.metadata().size_z,
                    reader.metadata().size_t,
                    reader.metadata().image_count,
                ),
                (2, 2, 4)
            );
            assert_eq!(reader.open_bytes(0).unwrap(), [4, 5, 6, 1, 2, 3]);
            assert_eq!(reader.open_bytes(1).unwrap(), [24, 25, 26, 21, 22, 23]);
            assert_eq!(reader.open_bytes(2).unwrap(), [14, 15, 16, 11, 12, 13]);
            assert_eq!(reader.open_bytes(3).unwrap(), [34, 35, 36, 31, 32, 33]);
            assert!(matches!(
                reader.open_bytes(4),
                Err(BioFormatsError::PlaneOutOfRange(4))
            ));
        }
    }

    #[test]
    fn groups_version_0_members_with_per_file_footer_correction() {
        let directory = TestDirectory::new();
        let first_frame = (1_u8..=16).collect::<Vec<_>>();
        let second_frame = (21_u8..=36).collect::<Vec<_>>();
        let first = directory.write(
            "camera-000.dcimg",
            &version_0_file(4, 4, PIXEL_MONO8, &[&first_frame], None),
        );
        directory.write(
            "camera-001.dcimg",
            &version_0_file(
                4,
                4,
                PIXEL_MONO8,
                &[&second_frame],
                Some((2, &[101, 102, 103, 104])),
            ),
        );

        let mut reader = DcimgReader::new();
        reader.set_id(&first).unwrap();
        assert_eq!(
            (
                reader.metadata().size_z,
                reader.metadata().size_t,
                reader.metadata().image_count,
            ),
            (2, 1, 2)
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            [33, 34, 35, 36, 29, 30, 31, 32, 101, 102, 103, 104, 21, 22, 23, 24,]
        );
    }

    #[test]
    fn rejects_every_incompatible_group_field_before_pixel_reads() {
        let incompatible = [
            (
                "version",
                version_0_file(3, 2, PIXEL_MONO8, &[&[1, 2, 3, 4, 5, 6]], None),
            ),
            (
                "width",
                version_1_file(2, 2, PIXEL_MONO8, 0, &[&[1, 2, 3, 4]]),
            ),
            (
                "height",
                version_1_file(3, 3, PIXEL_MONO8, 0, &[&[1, 2, 3, 4, 5, 6, 7, 8, 9]]),
            ),
            (
                "frame count",
                version_1_file(
                    3,
                    2,
                    PIXEL_MONO8,
                    0,
                    &[&[1, 2, 3, 4, 5, 6], &[7, 8, 9, 10, 11, 12]],
                ),
            ),
            (
                "pixel type",
                version_1_file(
                    3,
                    2,
                    PIXEL_MONO16,
                    0,
                    &[&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]],
                ),
            ),
        ];

        for (field, candidate) in incompatible {
            let directory = TestDirectory::new();
            let primary = directory.write(
                "camera-000.dcimg",
                &version_1_file(3, 2, PIXEL_MONO8, 0, &[&[1, 2, 3, 4, 5, 6]]),
            );
            directory.write("camera-001.dcimg", &candidate);

            assert!(
                matches!(
                    DcimgReader::new().set_id(&primary),
                    Err(BioFormatsError::InvalidData(message))
                        if message.contains("group member") && message.contains("incompatible")
                ),
                "accepted incompatible DCIMG {field}"
            );
        }
    }

    #[test]
    fn failed_regroup_preserves_the_initialized_reader() {
        let valid = TestFile::new(&version_1_file(
            3,
            2,
            PIXEL_MONO8,
            0,
            &[&[1, 2, 3, 4, 5, 6]],
        ));
        let mut reader = DcimgReader::new();
        reader.set_id(&valid.path).unwrap();

        let invalid_group = TestDirectory::new();
        let invalid_primary = invalid_group.write(
            "camera-000.dcimg",
            &version_1_file(3, 2, PIXEL_MONO8, 0, &[&[7, 8, 9, 10, 11, 12]]),
        );
        invalid_group.write(
            "camera-001.dcimg",
            &version_1_file(2, 2, PIXEL_MONO8, 0, &[&[13, 14, 15, 16]]),
        );

        assert!(reader.set_id(&invalid_primary).is_err());
        assert_eq!(reader.current_file(), Some(valid.path.as_path()));
        assert_eq!((reader.metadata().size_z, reader.metadata().size_t), (1, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), [4, 5, 6, 1, 2, 3]);
    }

    #[test]
    fn reads_version_1_mono16_without_changing_little_endian_samples() {
        let samples = [1u16, 2, 0x1234, 0xabcd];
        let frame = samples
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let file = TestFile::new(&version_1_file(2, 2, PIXEL_MONO16, 0, &[&frame]));
        let mut reader = DcimgReader::new();
        reader.set_id(&file.path).unwrap();

        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(reader.metadata().bits_per_pixel, 16);
        let expected = [0x34, 0x12, 0xcd, 0xab, 1, 0, 2, 0];
        assert_eq!(reader.open_bytes(0).unwrap(), expected);
    }

    #[test]
    fn skips_version_1_frame_footers() {
        let mut bytes = version_1_file(1, 1, PIXEL_MONO8, 16, &[&[9], &[7]]);
        let first_footer = 256 + 1;
        bytes[first_footer..first_footer + 16].fill(0xaa);
        let second_footer = first_footer + 16 + 1;
        bytes[second_footer..second_footer + 16].fill(0xbb);
        let file = TestFile::new(&bytes);
        let mut reader = DcimgReader::new();
        reader.set_id(&file.path).unwrap();

        assert_eq!(reader.open_bytes(0).unwrap(), [9]);
        assert_eq!(reader.open_bytes(1).unwrap(), [7]);
    }

    #[test]
    fn reads_region_directly_with_java_window_flip() {
        let frame = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let file = TestFile::new(&version_1_file(4, 3, PIXEL_MONO8, 0, &[&frame]));
        let mut reader = DcimgReader::new();
        reader.set_id(&file.path).unwrap();

        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            [6, 7, 2, 3]
        );
        assert!(matches!(
            reader.open_bytes_region(0, 3, 0, 2, 1),
            Err(BioFormatsError::InvalidRegion { .. })
        ));
        assert!(matches!(
            reader.open_bytes_region(0, 0, 0, u32::MAX, u32::MAX),
            Err(BioFormatsError::InvalidRegionShape { .. })
                | Err(BioFormatsError::InvalidRegion { .. })
        ));
    }

    #[test]
    fn applies_version_1_four_pixel_footer_correction() {
        let frame = (1u8..=24).collect::<Vec<_>>();
        let mut bytes = version_1_file(6, 4, PIXEL_MONO8, 32, &[&frame]);
        let footer = 256 + frame.len();
        bytes[footer + 12..footer + 16].copy_from_slice(&[101, 102, 103, 104]);
        let file = TestFile::new(&bytes);
        let mut reader = DcimgReader::new();
        reader.set_id(&file.path).unwrap();

        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(&plane[12..18], &[101, 102, 103, 104, 11, 12]);
    }

    #[test]
    fn reads_version_0_footer_and_four_pixel_correction() {
        let frame_0 = (1u8..=16).collect::<Vec<_>>();
        let frame_1 = (21u8..=36).collect::<Vec<_>>();
        let bytes = version_0_file(
            4,
            4,
            PIXEL_MONO8,
            &[&frame_0, &frame_1],
            Some((2, &[101, 102, 103, 104])),
        );
        let file = TestFile::new(&bytes);
        let mut reader = DcimgReader::new();
        reader.set_id(&file.path).unwrap();

        assert_eq!(reader.metadata().size_t, 2);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [13, 14, 15, 16, 9, 10, 11, 12, 101, 102, 103, 104, 1, 2, 3, 4,]
        );
        assert_eq!(&reader.open_bytes(1).unwrap()[8..12], &[101, 102, 103, 104]);
    }

    #[test]
    fn rejects_invalid_and_truncated_headers() {
        let invalid_magic = TestFile::new(b"NOIMG");
        let mut reader = DcimgReader::new();
        assert!(!reader.is_this_type_by_bytes(b"NOIMG"));
        assert!(matches!(
            reader.set_id(&invalid_magic.path),
            Err(BioFormatsError::InvalidData(_))
        ));

        let mut mismatched = version_1_file(1, 1, PIXEL_MONO8, 0, &[&[1]]);
        let mismatched_len = mismatched.len() as u32;
        put_u32(&mut mismatched, 64, mismatched_len + 1);
        let mismatched = TestFile::new(&mismatched);
        assert!(matches!(
            reader.set_id(&mismatched.path),
            Err(BioFormatsError::InvalidData(_))
        ));

        let mut truncated = version_1_file(2, 2, PIXEL_MONO8, 0, &[&[1, 2, 3, 4]]);
        truncated.pop();
        finish_main_header(&mut truncated);
        let truncated = TestFile::new(&truncated);
        assert!(matches!(
            reader.set_id(&truncated.path),
            Err(BioFormatsError::InvalidData(_))
        ));

        let short_header = TestFile::new(b"DCIMG");
        assert!(matches!(
            reader.set_id(&short_header.path),
            Err(BioFormatsError::InvalidData(_))
        ));
    }
}
