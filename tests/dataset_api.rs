use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats_rs::{
    open, BioFormatsError, FormatId, PixelType, PlaneCoordinates, ReadRequest, Rect, Region,
};

struct TempTiff {
    path: PathBuf,
}

impl TempTiff {
    fn with_two_samples() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("bioformats_dataset_two_samples_{unique}.tif"));
        write_two_sample_tiff(&path);
        Self { path }
    }
}

impl TempTiff {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bioformats_dataset_{unique}.tif"));
        write_tiff(&path);
        Self { path }
    }
}

impl Drop for TempTiff {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn opens_hierarchical_metadata_and_reads_explicit_region() {
    let fixture = TempTiff::new();
    let dataset = open(&fixture.path).expect("open TIFF dataset");

    assert_eq!(dataset.format(), FormatId::Tiff);
    assert_eq!(dataset.used_files(), std::slice::from_ref(&fixture.path));
    assert_eq!(dataset.series().len(), 1);
    assert_eq!(dataset.series()[0].index(), 0);
    assert_eq!(dataset.series()[0].resolutions().len(), 1);

    let metadata = dataset.series()[0].resolutions()[0].metadata();
    assert_eq!((metadata.size_x, metadata.size_y), (3, 2));
    assert_eq!(metadata.pixel_type, PixelType::Uint8);

    let request = ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)).with_region(Region::Rect(
        Rect::new(1, 0, 2, 2).expect("valid rectangle"),
    ));
    let expected = dataset.plane_info(request).expect("preflight plane");
    assert_eq!(expected.byte_len, 4);
    assert_eq!(expected.region.width, 2);

    let plane = dataset.read_plane(request).expect("read region");
    assert_eq!(plane.bytes(), &[2, 3, 5, 6]);
    assert_eq!(plane.info().region.width, 2);
    assert_eq!(plane.info().layout.samples_per_pixel, 1);
    assert_eq!(plane.info().byte_len, 4);
}

#[test]
fn plane_info_counts_non_rgb_samples_per_pixel() {
    let fixture = TempTiff::with_two_samples();
    let dataset = open(&fixture.path).expect("open two-sample TIFF dataset");
    let metadata = dataset.series()[0].resolutions()[0].metadata();

    assert!(!metadata.is_rgb);
    assert_eq!(metadata.samples_per_pixel, 2);
    assert_eq!(metadata.rgb_channel_count(), 2);

    let request = ReadRequest::new(0, PlaneCoordinates::default());
    let info = dataset.plane_info(request).expect("preflight plane");
    assert_eq!(info.layout.samples_per_pixel, 2);
    assert_eq!(info.byte_len, 12);

    let plane = dataset.read_plane(request).expect("read two-sample plane");
    assert_eq!(plane.bytes(), &[1, 11, 2, 12, 3, 13, 4, 14, 5, 15, 6, 16]);
}

#[test]
fn invalid_coordinates_and_regions_are_errors_not_panics() {
    let fixture = TempTiff::new();
    let dataset = open(&fixture.path).expect("open TIFF dataset");

    let coordinate_error = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(1, 0, 0)))
        .expect_err("Z=1 must be rejected");
    assert!(matches!(
        coordinate_error,
        BioFormatsError::PlaneCoordinatesOutOfRange { .. }
    ));

    let region = Region::Rect(Rect::new(2, 0, 2, 1).expect("well-formed rectangle"));
    let region_error = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()).with_region(region))
        .expect_err("out-of-bounds rectangle must be rejected");
    assert!(matches!(
        region_error,
        BioFormatsError::InvalidRegion { .. }
    ));

    assert!(matches!(
        Rect::new(u32::MAX, 0, 2, 1),
        Err(BioFormatsError::InvalidRegionShape { .. })
    ));
}

#[test]
fn missing_input_preserves_the_filesystem_error() {
    let path = std::env::temp_dir().join("bioformats_missing_input.unknown");
    let _ = fs::remove_file(&path);
    assert!(matches!(open(path), Err(BioFormatsError::Io(_))));
}

#[test]
fn caller_buffer_is_checked_and_can_be_reused() {
    let fixture = TempTiff::new();
    let dataset = open(&fixture.path).expect("open TIFF dataset");
    let request = ReadRequest::new(0, PlaneCoordinates::default());

    let mut too_small = [0_u8; 5];
    let error = dataset
        .read_plane_into(request, &mut too_small)
        .expect_err("small buffer must fail");
    assert!(matches!(
        error,
        BioFormatsError::BufferTooSmall {
            required: 6,
            actual: 5
        }
    ));
    assert_eq!(too_small, [0; 5]);

    let mut destination = [0xaa_u8; 8];
    let info = dataset
        .read_plane_into(request, &mut destination)
        .expect("read into reusable buffer");
    assert_eq!(info.byte_len, 6);
    assert_eq!(&destination[..6], &[1, 2, 3, 4, 5, 6]);
    assert_eq!(&destination[6..], &[0xaa, 0xaa]);
}

#[test]
fn explicit_requests_are_safe_across_threads() {
    let fixture = TempTiff::new();
    let dataset = Arc::new(open(&fixture.path).expect("open TIFF dataset"));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let dataset = Arc::clone(&dataset);
        workers.push(std::thread::spawn(move || {
            dataset
                .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
                .expect("concurrent plane read")
                .into_bytes()
        }));
    }
    for worker in workers {
        assert_eq!(
            worker.join().expect("worker did not panic"),
            [1, 2, 3, 4, 5, 6]
        );
    }
}

fn write_tiff(path: &Path) {
    let width = 3_u32;
    let height = 2_u32;
    let pixels = [1_u8, 2, 3, 4, 5, 6];
    let ifd_offset = 8_u32;
    let tag_count = 9_u16;
    let pixel_offset = ifd_offset as usize + 2 + tag_count as usize * 12 + 4;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&ifd_offset.to_le_bytes());
    bytes.extend_from_slice(&tag_count.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, width);
    push_tag(&mut bytes, 257, 4, 1, height);
    push_tag(&mut bytes, 258, 3, 1, 8);
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(&mut bytes, 273, 4, 1, pixel_offset as u32);
    push_tag(&mut bytes, 277, 3, 1, 1);
    push_tag(&mut bytes, 278, 4, 1, height);
    push_tag(&mut bytes, 279, 4, 1, pixels.len() as u32);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&pixels);
    fs::write(path, bytes).expect("write TIFF fixture");
}

fn write_two_sample_tiff(path: &Path) {
    let width = 3_u32;
    let height = 2_u32;
    let pixels = [1_u8, 11, 2, 12, 3, 13, 4, 14, 5, 15, 6, 16];
    let ifd_offset = 8_u32;
    let tag_count = 9_u16;
    let pixel_offset = ifd_offset as usize + 2 + tag_count as usize * 12 + 4;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&ifd_offset.to_le_bytes());
    bytes.extend_from_slice(&tag_count.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, width);
    push_tag(&mut bytes, 257, 4, 1, height);
    push_tag(&mut bytes, 258, 3, 2, u32::from_le_bytes([8, 0, 8, 0]));
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(&mut bytes, 273, 4, 1, pixel_offset as u32);
    push_tag(&mut bytes, 277, 3, 1, 2);
    push_tag(&mut bytes, 278, 4, 1, height);
    push_tag(&mut bytes, 279, 4, 1, pixels.len() as u32);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&pixels);
    fs::write(path, bytes).expect("write two-sample TIFF fixture");
}

fn push_tag(bytes: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&field_type.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&value.to_le_bytes());
}
