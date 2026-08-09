//! Integration-oriented, request-based access to an opened microscopy dataset.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::pixel_type::PixelType;
use crate::registry::{FormatId, ImageReader};
use crate::source::{SourceInfo, SourceInput};

/// Open a dataset with the built-in format registry.
pub fn open(path: impl AsRef<Path>) -> Result<Dataset> {
    Dataset::open(path)
}

/// Open a dataset from an application-owned random-access source.
pub fn open_source(input: SourceInput) -> Result<Dataset> {
    Dataset::open_source(input)
}

/// Metadata for one source series and all of its pyramid resolutions.
#[derive(Debug, Clone)]
pub struct Series {
    index: usize,
    resolutions: Vec<Resolution>,
}

impl Series {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn resolutions(&self) -> &[Resolution] {
        &self.resolutions
    }
}

/// Metadata for one resolution; index zero is the native/highest resolution.
#[derive(Debug, Clone)]
pub struct Resolution {
    index: usize,
    metadata: ImageMetadata,
}

impl Resolution {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }
}

/// Logical Z/C/T coordinates. C addresses a logical channel, not an RGB sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaneCoordinates {
    pub z: u32,
    pub c: u32,
    pub t: u32,
}

impl PlaneCoordinates {
    pub const fn new(z: u32, c: u32, t: u32) -> Self {
        Self { z, c, t }
    }
}

/// A non-empty rectangular XY region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Result<Self> {
        if width == 0
            || height == 0
            || x.checked_add(width).is_none()
            || y.checked_add(height).is_none()
        {
            return Err(BioFormatsError::InvalidRegionShape {
                x,
                y,
                width,
                height,
            });
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }
}

/// Full-plane or bounded-region access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Region {
    #[default]
    Full,
    Rect(Rect),
}

/// A complete, explicit read request with no mutable selection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequest {
    pub series: usize,
    pub resolution: usize,
    pub plane: PlaneCoordinates,
    pub region: Region,
}

impl ReadRequest {
    pub const fn new(series: usize, plane: PlaneCoordinates) -> Self {
        Self {
            series,
            resolution: 0,
            plane,
            region: Region::Full,
        }
    }

    pub const fn with_resolution(mut self, resolution: usize) -> Self {
        self.resolution = resolution;
        self
    }

    pub const fn with_region(mut self, region: Region) -> Self {
        self.region = region;
        self
    }
}

/// Storage layout of the returned native samples.
///
/// Rows are tightly packed with no padding. When `interleaved` is true, the
/// byte offset for sample `s` of pixel `(x, y)` is
/// `((y * width + x) * samples_per_pixel + s) * bytes_per_sample`. When it is
/// false, complete sample components are stored consecutively and the offset
/// is `(s * width * height + y * width + x) * bytes_per_sample`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelLayout {
    pub pixel_type: PixelType,
    pub significant_bits: u8,
    pub samples_per_pixel: u32,
    pub interleaved: bool,
    pub little_endian: bool,
}

/// Description shared by allocated and caller-buffer plane reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneInfo {
    pub series: usize,
    pub resolution: usize,
    pub coordinates: PlaneCoordinates,
    pub region: Rect,
    pub layout: PixelLayout,
    pub byte_len: usize,
}

/// One native plane or region. Bytes retain the byte order advertised by `layout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    info: PlaneInfo,
    bytes: Vec<u8>,
}

impl Plane {
    pub fn info(&self) -> &PlaneInfo {
        &self.info
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// An opened, lazy dataset. Metadata is fixed at open; pixel decoding happens on read.
pub struct Dataset {
    reader: Mutex<ImageReader>,
    format: FormatId,
    used_files: Vec<PathBuf>,
    used_sources: Vec<SourceInfo>,
    series: Vec<Series>,
}

impl Dataset {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_reader(ImageReader::open(path.as_ref())?)
    }

    pub fn open_source(input: SourceInput) -> Result<Self> {
        Self::from_reader(ImageReader::open_source(input)?)
    }

    fn from_reader(mut reader: ImageReader) -> Result<Self> {
        let format = reader.format().ok_or(BioFormatsError::NotInitialized)?;
        let used_files = reader.used_files();
        let used_sources = reader.used_sources();

        reader.set_flattened_resolutions(false)?;
        let series_count = reader.series_count();
        if series_count == 0 {
            return Err(BioFormatsError::InvalidData(
                "reader exposed no image series".into(),
            ));
        }
        let mut series_metadata = Vec::new();
        series_metadata
            .try_reserve_exact(series_count)
            .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        for series_index in 0..series_count {
            reader.set_series(series_index)?;
            let resolution_count = reader.resolution_count();
            if resolution_count == 0 {
                return Err(BioFormatsError::InvalidData(format!(
                    "series {series_index} exposed no resolutions"
                )));
            }
            let mut resolutions = Vec::new();
            resolutions
                .try_reserve_exact(resolution_count)
                .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
            for resolution_index in 0..resolution_count {
                reader.set_resolution(resolution_index)?;
                let metadata = reader.metadata().clone();
                validate_metadata(&metadata, series_index, resolution_index, resolution_count)?;
                resolutions.push(Resolution {
                    index: resolution_index,
                    metadata,
                });
            }
            series_metadata.push(Series {
                index: series_index,
                resolutions,
            });
        }
        reader.set_series(0)?;
        reader.set_resolution(0)?;

        Ok(Self {
            reader: Mutex::new(reader),
            format,
            used_files,
            used_sources,
            series: series_metadata,
        })
    }

    pub fn format(&self) -> FormatId {
        self.format
    }

    pub fn used_files(&self) -> &[PathBuf] {
        &self.used_files
    }

    /// Stable identities and logical names of every source used by the dataset.
    pub fn used_sources(&self) -> &[SourceInfo] {
        &self.used_sources
    }

    pub fn series(&self) -> &[Series] {
        &self.series
    }

    /// Validate a request and describe its native layout without reading pixels.
    pub fn plane_info(&self, request: ReadRequest) -> Result<PlaneInfo> {
        let metadata = self.metadata_for(request)?;
        validate_coordinates(metadata, request.plane)?;
        let region = resolve_region(metadata, request.region)?;
        let layout = pixel_layout(metadata)?;
        let byte_len = plane_byte_len(region, layout)?;

        Ok(PlaneInfo {
            series: request.series,
            resolution: request.resolution,
            coordinates: request.plane,
            region,
            layout,
            byte_len,
        })
    }

    pub fn read_plane(&self, request: ReadRequest) -> Result<Plane> {
        let info = self.plane_info(request)?;
        let metadata = self.metadata_for(request)?;
        let plane_index = metadata
            .checked_index(request.plane.z, request.plane.c, request.plane.t)
            .ok_or_else(|| {
                BioFormatsError::InvalidData(
                    "validated coordinates could not be converted to a plane index".into(),
                )
            })?;

        let bytes = {
            let mut reader = self
                .reader
                .lock()
                .map_err(|_| BioFormatsError::ReaderStatePoisoned)?;
            reader.set_series(request.series)?;
            reader.set_resolution(request.resolution)?;
            if info.region.x == 0
                && info.region.y == 0
                && info.region.width == metadata.size_x
                && info.region.height == metadata.size_y
            {
                reader.open_bytes(plane_index)?
            } else {
                reader.open_bytes_region(
                    plane_index,
                    info.region.x,
                    info.region.y,
                    info.region.width,
                    info.region.height,
                )?
            }
        };

        if bytes.len() != info.byte_len {
            return Err(BioFormatsError::PlaneByteCountMismatch {
                expected: info.byte_len,
                actual: bytes.len(),
            });
        }
        Ok(Plane { info, bytes })
    }

    /// Read into a reusable caller buffer. An oversized buffer's suffix is unchanged.
    ///
    /// Raw row-addressable readers decode directly into this buffer. Codecs or
    /// readers that need whole-plane transforms may still use temporary storage.
    pub fn read_plane_into(
        &self,
        request: ReadRequest,
        destination: &mut [u8],
    ) -> Result<PlaneInfo> {
        let expected = self.plane_info(request)?;
        if destination.len() < expected.byte_len {
            return Err(BioFormatsError::BufferTooSmall {
                required: expected.byte_len,
                actual: destination.len(),
            });
        }
        let metadata = self.metadata_for(request)?;
        let plane_index = metadata
            .checked_index(request.plane.z, request.plane.c, request.plane.t)
            .ok_or_else(|| {
                BioFormatsError::InvalidData(
                    "validated coordinates could not be converted to a plane index".into(),
                )
            })?;
        // Give lower-level readers only the promised writable prefix so a
        // buggy or newly ported reader cannot mutate an oversized buffer's
        // suffix before its returned length is validated below.
        let destination = &mut destination[..expected.byte_len];
        let written = {
            let mut reader = self
                .reader
                .lock()
                .map_err(|_| BioFormatsError::ReaderStatePoisoned)?;
            reader.set_series(request.series)?;
            reader.set_resolution(request.resolution)?;
            if expected.region.x == 0
                && expected.region.y == 0
                && expected.region.width == metadata.size_x
                && expected.region.height == metadata.size_y
            {
                reader.open_bytes_into(plane_index, destination)?
            } else {
                reader.open_bytes_region_into(
                    plane_index,
                    expected.region.x,
                    expected.region.y,
                    expected.region.width,
                    expected.region.height,
                    destination,
                )?
            }
        };
        if written != expected.byte_len {
            return Err(BioFormatsError::PlaneByteCountMismatch {
                expected: expected.byte_len,
                actual: written,
            });
        }
        Ok(expected)
    }

    fn metadata_for(&self, request: ReadRequest) -> Result<&ImageMetadata> {
        let series = self
            .series
            .get(request.series)
            .ok_or(BioFormatsError::SeriesOutOfRange(request.series))?;
        series
            .resolutions
            .get(request.resolution)
            .map(Resolution::metadata)
            .ok_or(BioFormatsError::ResolutionOutOfRange {
                series: request.series,
                resolution: request.resolution,
            })
    }
}

fn validate_coordinates(metadata: &ImageMetadata, coordinates: PlaneCoordinates) -> Result<()> {
    let size_c = metadata.effective_size_c();
    if coordinates.z < metadata.size_z && coordinates.c < size_c && coordinates.t < metadata.size_t
    {
        Ok(())
    } else {
        Err(BioFormatsError::PlaneCoordinatesOutOfRange {
            z: coordinates.z,
            c: coordinates.c,
            t: coordinates.t,
            size_z: metadata.size_z,
            size_c,
            size_t: metadata.size_t,
        })
    }
}

fn validate_metadata(
    metadata: &ImageMetadata,
    series: usize,
    resolution: usize,
    resolution_count: usize,
) -> Result<()> {
    let context = || format!("series {series}, resolution {resolution}");
    if metadata.size_x == 0
        || metadata.size_y == 0
        || metadata.size_z == 0
        || metadata.size_c == 0
        || metadata.size_t == 0
        || metadata.image_count == 0
    {
        return Err(BioFormatsError::InvalidData(format!(
            "{} has a zero image dimension or plane count",
            context()
        )));
    }
    if metadata.resolution_count as usize != resolution_count {
        return Err(BioFormatsError::InvalidData(format!(
            "{} reports {} resolutions, but the reader exposes {resolution_count}",
            context(),
            metadata.resolution_count
        )));
    }

    let size_zt = u64::from(metadata.size_z)
        .checked_mul(u64::from(metadata.size_t))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    if u64::from(metadata.image_count) % size_zt != 0 {
        return Err(BioFormatsError::InvalidData(format!(
            "{} has image_count {} that is not divisible by SizeZ*SizeT ({size_zt})",
            context(),
            metadata.image_count
        )));
    }
    let logical_channels = u64::from(metadata.image_count) / size_zt;
    if logical_channels == 0 || logical_channels > u64::from(u32::MAX) {
        return Err(BioFormatsError::InvalidData(format!(
            "{} has an invalid logical channel count",
            context()
        )));
    }
    if metadata.is_rgb {
        let stored_size_c = logical_channels
            .checked_mul(u64::from(metadata.samples_per_pixel))
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        if stored_size_c != u64::from(metadata.size_c) {
            return Err(BioFormatsError::InvalidData(format!(
                "{} reports RGB SizeC {}, but {} logical channels with {} samples per pixel require SizeC {stored_size_c}",
                context(),
                metadata.size_c,
                logical_channels,
                metadata.samples_per_pixel
            )));
        }
    }

    let layout = pixel_layout(metadata)?;
    plane_byte_len(
        Rect {
            x: 0,
            y: 0,
            width: metadata.size_x,
            height: metadata.size_y,
        },
        layout,
    )?;
    Ok(())
}

fn resolve_region(metadata: &ImageMetadata, region: Region) -> Result<Rect> {
    let rect = match region {
        Region::Full => Rect {
            x: 0,
            y: 0,
            width: metadata.size_x,
            height: metadata.size_y,
        },
        Region::Rect(rect) => rect,
    };
    crate::common::reader::validate_region(metadata, rect.x, rect.y, rect.width, rect.height)?;
    Ok(rect)
}

fn pixel_layout(metadata: &ImageMetadata) -> Result<PixelLayout> {
    let bytes_per_sample = metadata.pixel_type.bytes_per_sample();
    if bytes_per_sample == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Dataset API does not yet expose packed {:?} samples",
            metadata.pixel_type
        )));
    }
    if metadata.samples_per_pixel == 0 {
        return Err(BioFormatsError::InvalidData(
            "SamplesPerPixel must be positive".into(),
        ));
    }
    let storage_bits = bytes_per_sample * 8;
    if metadata.bits_per_pixel == 0 || usize::from(metadata.bits_per_pixel) > storage_bits {
        return Err(BioFormatsError::InvalidData(format!(
            "significant bit count {} is incompatible with {:?} storage ({storage_bits} bits)",
            metadata.bits_per_pixel, metadata.pixel_type
        )));
    }
    Ok(PixelLayout {
        pixel_type: metadata.pixel_type,
        significant_bits: metadata.bits_per_pixel,
        samples_per_pixel: metadata.samples_per_pixel,
        interleaved: metadata.is_interleaved,
        little_endian: metadata.is_little_endian,
    })
}

fn plane_byte_len(region: Rect, layout: PixelLayout) -> Result<usize> {
    let bytes_per_sample = layout.pixel_type.bytes_per_sample();
    if bytes_per_sample == 0
        || layout.significant_bits == 0
        || usize::from(layout.significant_bits) > bytes_per_sample * 8
        || layout.samples_per_pixel == 0
    {
        return Err(BioFormatsError::InvalidData(
            "pixel layout has inconsistent storage width".into(),
        ));
    }
    let byte_len = (region.width as usize)
        .checked_mul(region.height as usize)
        .and_then(|pixels| pixels.checked_mul(layout.samples_per_pixel as usize))
        .and_then(|samples| samples.checked_mul(bytes_per_sample))
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    if byte_len > isize::MAX as usize {
        return Err(BioFormatsError::PlaneByteCountOverflow);
    }
    Ok(byte_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_byte_count_rejects_overflow() {
        let region = Rect {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: u32::MAX,
        };
        let layout = PixelLayout {
            pixel_type: PixelType::Float64,
            significant_bits: 64,
            samples_per_pixel: u32::MAX,
            interleaved: true,
            little_endian: true,
        };

        assert!(matches!(
            plane_byte_len(region, layout),
            Err(BioFormatsError::PlaneByteCountOverflow)
        ));
    }

    #[test]
    fn expected_byte_count_rejects_lengths_above_isize_max() {
        let region = Rect {
            x: 0,
            y: 0,
            width: u32::MAX,
            height: u32::MAX,
        };
        let layout = PixelLayout {
            pixel_type: PixelType::Uint8,
            significant_bits: 8,
            samples_per_pixel: 1,
            interleaved: true,
            little_endian: true,
        };

        assert!(matches!(
            plane_byte_len(region, layout),
            Err(BioFormatsError::PlaneByteCountOverflow)
        ));
    }

    #[test]
    fn pixel_layout_rejects_packed_or_impossible_storage() {
        let packed = ImageMetadata {
            pixel_type: PixelType::Bit,
            bits_per_pixel: 1,
            ..ImageMetadata::default()
        };
        assert!(matches!(
            pixel_layout(&packed),
            Err(BioFormatsError::UnsupportedFormat(_))
        ));

        let too_many_bits = ImageMetadata {
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 12,
            ..ImageMetadata::default()
        };
        assert!(matches!(
            pixel_layout(&too_many_bits),
            Err(BioFormatsError::InvalidData(_))
        ));
    }

    #[test]
    fn metadata_rejects_inconsistent_logical_and_stored_channels() {
        let metadata = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_c: 3,
            samples_per_pixel: 2,
            image_count: 1,
            is_rgb: true,
            ..ImageMetadata::default()
        };

        assert!(matches!(
            validate_metadata(&metadata, 0, 0, 1),
            Err(BioFormatsError::InvalidData(message)) if message.contains("require SizeC 2")
        ));
    }
}
