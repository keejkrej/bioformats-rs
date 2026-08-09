use super::pixel_type::PixelType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Dimension ordering of the image planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DimensionOrder {
    XYCTZ,
    XYCZT,
    XYTCZ,
    XYTZC,
    XYZCT,
    XYZTC,
}

impl Default for DimensionOrder {
    fn default() -> Self {
        DimensionOrder::XYCZT
    }
}

impl DimensionOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            DimensionOrder::XYCTZ => "XYCTZ",
            DimensionOrder::XYCZT => "XYCZT",
            DimensionOrder::XYTCZ => "XYTCZ",
            DimensionOrder::XYTZC => "XYTZC",
            DimensionOrder::XYZCT => "XYZCT",
            DimensionOrder::XYZTC => "XYZTC",
        }
    }

    pub fn from_str(order: &str) -> Option<Self> {
        match order {
            "XYCTZ" => Some(DimensionOrder::XYCTZ),
            "XYCZT" => Some(DimensionOrder::XYCZT),
            "XYTCZ" => Some(DimensionOrder::XYTCZ),
            "XYTZC" => Some(DimensionOrder::XYTZC),
            "XYZCT" => Some(DimensionOrder::XYZCT),
            "XYZTC" => Some(DimensionOrder::XYZTC),
            _ => None,
        }
    }

    pub fn axis_positions(self) -> (usize, usize, usize) {
        let order = self.as_str().as_bytes();
        let z = order.iter().position(|axis| *axis == b'Z').unwrap();
        let c = order.iter().position(|axis| *axis == b'C').unwrap();
        let t = order.iter().position(|axis| *axis == b'T').unwrap();
        (z, c, t)
    }
}

/// A typed metadata value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Bytes(Vec<u8>),
}

impl std::fmt::Display for MetadataValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetadataValue::String(s) => write!(f, "{}", s),
            MetadataValue::Int(i) => write!(f, "{}", i),
            MetadataValue::Float(v) => write!(f, "{}", v),
            MetadataValue::Bool(b) => write!(f, "{}", b),
            MetadataValue::Bytes(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

/// Optional indexed colour lookup table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LookupTable {
    pub red: Vec<u16>,
    pub green: Vec<u16>,
    pub blue: Vec<u16>,
}

/// Optional per-channel metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelMetadata {
    pub name: Option<String>,
    pub color: Option<u32>,
    pub emission_wavelength_nm: Option<f64>,
    pub excitation_wavelength_nm: Option<f64>,
}

/// Optional per-plane metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaneMetadata {
    pub z: u32,
    pub c: u32,
    pub t: u32,
    pub delta_t_seconds: Option<f64>,
    pub position_x_um: Option<f64>,
    pub position_y_um: Option<f64>,
    pub position_z_um: Option<f64>,
}

/// Core metadata for one image series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageMetadata {
    pub size_x: u32,
    pub size_y: u32,
    pub size_z: u32,
    pub size_c: u32,
    pub size_t: u32,
    pub pixel_type: PixelType,
    pub bits_per_pixel: u8,
    /// Number of stored samples returned for each XY pixel.
    ///
    /// This is independent of whether those samples represent RGB colour.
    #[serde(default = "default_samples_per_pixel")]
    pub samples_per_pixel: u32,
    pub image_count: u32,
    pub dimension_order: DimensionOrder,
    pub is_rgb: bool,
    pub is_interleaved: bool,
    pub is_indexed: bool,
    pub is_false_color: bool,
    pub is_little_endian: bool,
    pub resolution_count: u32,
    pub series_metadata: HashMap<String, MetadataValue>,
    pub lookup_table: Option<LookupTable>,
    pub physical_size_x_um: Option<f64>,
    pub physical_size_y_um: Option<f64>,
    pub physical_size_z_um: Option<f64>,
    pub time_increment_seconds: Option<f64>,
    pub acquisition_timestamp: Option<String>,
    pub objective_model: Option<String>,
    pub objective_magnification: Option<f64>,
    pub objective_na: Option<f64>,
    pub channel_metadata: Vec<ChannelMetadata>,
    pub plane_metadata: Vec<PlaneMetadata>,
    pub used_files: Vec<PathBuf>,
}

impl Default for ImageMetadata {
    fn default() -> Self {
        ImageMetadata {
            size_x: 0,
            size_y: 0,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            samples_per_pixel: 1,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_false_color: true,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            physical_size_x_um: None,
            physical_size_y_um: None,
            physical_size_z_um: None,
            time_increment_seconds: None,
            acquisition_timestamp: None,
            objective_model: None,
            objective_magnification: None,
            objective_na: None,
            channel_metadata: Vec::new(),
            plane_metadata: Vec::new(),
            used_files: Vec::new(),
        }
    }
}

impl ImageMetadata {
    pub fn effective_size_c(&self) -> u32 {
        let size_zt = u64::from(self.size_z) * u64::from(self.size_t);
        if size_zt == 0 || u64::from(self.image_count) % size_zt != 0 {
            0
        } else {
            u32::try_from(u64::from(self.image_count) / size_zt).unwrap_or(0)
        }
    }

    pub fn rgb_channel_count(&self) -> u32 {
        self.samples_per_pixel
    }

    pub fn logical_channel_count(&self) -> u32 {
        self.effective_size_c()
    }

    pub fn get_index(&self, z: u32, c: u32, t: u32) -> u32 {
        self.checked_index(z, c, t)
            .expect("plane coordinates out of range or image dimensions are inconsistent")
    }

    /// Convert valid logical Z/C/T coordinates to a plane index without overflow.
    pub fn checked_index(&self, z: u32, c: u32, t: u32) -> Option<u32> {
        let logical_channels = self.logical_channel_count();
        if z >= self.size_z || c >= logical_channels || t >= self.size_t {
            return None;
        }
        let dims = [
            u64::from(self.size_z),
            u64::from(logical_channels),
            u64::from(self.size_t),
        ];
        let coords = [u64::from(z), u64::from(c), u64::from(t)];
        let (z_pos, c_pos, t_pos) = self.dimension_order.axis_positions();
        let dim_by_pos = match (z_pos, c_pos, t_pos) {
            (2, 3, 4) => [dims[0], dims[1], dims[2]],
            (2, 4, 3) => [dims[0], dims[2], dims[1]],
            (3, 2, 4) => [dims[1], dims[0], dims[2]],
            (4, 2, 3) => [dims[1], dims[2], dims[0]],
            (3, 4, 2) => [dims[2], dims[0], dims[1]],
            (4, 3, 2) => [dims[2], dims[1], dims[0]],
            _ => unreachable!("invalid dimension order"),
        };
        let coord_by_pos = match (z_pos, c_pos, t_pos) {
            (2, 3, 4) => [coords[0], coords[1], coords[2]],
            (2, 4, 3) => [coords[0], coords[2], coords[1]],
            (3, 2, 4) => [coords[1], coords[0], coords[2]],
            (4, 2, 3) => [coords[1], coords[2], coords[0]],
            (3, 4, 2) => [coords[2], coords[0], coords[1]],
            (4, 3, 2) => [coords[2], coords[1], coords[0]],
            _ => unreachable!("invalid dimension order"),
        };

        let index = coord_by_pos[2]
            .checked_mul(dim_by_pos[0])?
            .checked_mul(dim_by_pos[1])?
            .checked_add(coord_by_pos[1].checked_mul(dim_by_pos[0])?)?
            .checked_add(coord_by_pos[0])?;
        if index >= u64::from(self.image_count) {
            return None;
        }
        u32::try_from(index).ok()
    }

    pub fn get_zct_coords(&self, index: u32) -> (u32, u32, u32) {
        let dims = [
            u64::from(self.size_z),
            u64::from(self.logical_channel_count()),
            u64::from(self.size_t),
        ];
        let (z_pos, c_pos, t_pos) = self.dimension_order.axis_positions();
        let dim_by_pos = match (z_pos, c_pos, t_pos) {
            (2, 3, 4) => [dims[0], dims[1], dims[2]],
            (2, 4, 3) => [dims[0], dims[2], dims[1]],
            (3, 2, 4) => [dims[1], dims[0], dims[2]],
            (4, 2, 3) => [dims[1], dims[2], dims[0]],
            (3, 4, 2) => [dims[2], dims[0], dims[1]],
            (4, 3, 2) => [dims[2], dims[1], dims[0]],
            _ => unreachable!("invalid dimension order"),
        };

        if dim_by_pos.contains(&0) {
            return (0, 0, 0);
        }
        let index = u64::from(index);
        let p0 = index % dim_by_pos[0];
        let p1 = (index / dim_by_pos[0]) % dim_by_pos[1];
        let p2 = index / (dim_by_pos[0] * dim_by_pos[1]);

        let (z, c, t) = match (z_pos, c_pos, t_pos) {
            (2, 3, 4) => (p0, p1, p2),
            (2, 4, 3) => (p0, p2, p1),
            (3, 2, 4) => (p1, p0, p2),
            (4, 2, 3) => (p2, p0, p1),
            (3, 4, 2) => (p1, p2, p0),
            (4, 3, 2) => (p2, p1, p0),
            _ => unreachable!("invalid dimension order"),
        };
        (z as u32, c as u32, t as u32)
    }
}

const fn default_samples_per_pixel() -> u32 {
    1
}
