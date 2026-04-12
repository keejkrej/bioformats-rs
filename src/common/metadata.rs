use super::pixel_type::PixelType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
        }
    }
}

impl ImageMetadata {
    pub fn effective_size_c(&self) -> u32 {
        let size_zt = self.size_z.saturating_mul(self.size_t);
        if size_zt == 0 {
            0
        } else {
            self.image_count / size_zt
        }
    }

    pub fn rgb_channel_count(&self) -> u32 {
        let effective_c = self.effective_size_c();
        if effective_c == 0 {
            0
        } else {
            self.size_c / effective_c
        }
    }

    pub fn logical_channel_count(&self) -> u32 {
        self.effective_size_c()
    }

    pub fn get_index(&self, z: u32, c: u32, t: u32) -> u32 {
        let dims = [self.size_z, self.logical_channel_count(), self.size_t];
        let coords = [z, c, t];
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

        coord_by_pos[0]
            + coord_by_pos[1] * dim_by_pos[0]
            + coord_by_pos[2] * dim_by_pos[0] * dim_by_pos[1]
    }

    pub fn get_zct_coords(&self, index: u32) -> (u32, u32, u32) {
        let dims = [self.size_z, self.logical_channel_count(), self.size_t];
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

        let p0 = index % dim_by_pos[0];
        let p1 = (index / dim_by_pos[0]) % dim_by_pos[1];
        let p2 = index / (dim_by_pos[0] * dim_by_pos[1]);

        match (z_pos, c_pos, t_pos) {
            (2, 3, 4) => (p0, p1, p2),
            (2, 4, 3) => (p0, p2, p1),
            (3, 2, 4) => (p1, p0, p2),
            (4, 2, 3) => (p2, p0, p1),
            (3, 4, 2) => (p1, p2, p0),
            (4, 3, 2) => (p2, p1, p0),
            _ => unreachable!("invalid dimension order"),
        }
    }
}
