/// Shared error type for all bioformats crates.
#[derive(thiserror::Error, Debug)]
pub enum BioFormatsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Format error: {0}")]
    Format(String),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("Invalid data: {0}")]
    InvalidData(String),
    #[error("Codec error: {0}")]
    Codec(String),
    #[error("Snapshot unsupported: {0}")]
    SnapshotUnsupported(String),
    #[error("Reader not initialized — call set_id first")]
    NotInitialized,
    #[error("Series index {0} out of range")]
    SeriesOutOfRange(usize),
    #[error("Resolution index {resolution} out of range for series {series}")]
    ResolutionOutOfRange { series: usize, resolution: usize },
    #[error("Plane index {0} out of range")]
    PlaneOutOfRange(u32),
    #[error(
        "Plane coordinates Z={z}, C={c}, T={t} are outside Z={size_z}, C={size_c}, T={size_t}"
    )]
    PlaneCoordinatesOutOfRange {
        z: u32,
        c: u32,
        t: u32,
        size_z: u32,
        size_c: u32,
        size_t: u32,
    },
    #[error(
        "Region ({x}, {y}, {width}, {height}) is outside image dimensions {image_width}x{image_height}"
    )]
    InvalidRegion {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        image_width: u32,
        image_height: u32,
    },
    #[error("Region ({x}, {y}, {width}, {height}) is empty or overflows its coordinates")]
    InvalidRegionShape {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    #[error("Destination buffer is too small: needs {required} bytes, got {actual}")]
    BufferTooSmall { required: usize, actual: usize },
    #[error("Plane byte count overflows addressable memory")]
    PlaneByteCountOverflow,
    #[error("Reader returned {actual} bytes for a plane whose layout requires {expected}")]
    PlaneByteCountMismatch { expected: usize, actual: usize },
    #[error("Reader state is unavailable after another thread panicked")]
    ReaderStatePoisoned,
}

pub type Result<T> = std::result::Result<T, BioFormatsError>;
