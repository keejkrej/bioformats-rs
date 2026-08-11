use std::ops::Range;
use std::sync::{Arc, Mutex};

use bioformats_rs::{
    open_source, MetadataValue, PixelType, PlaneCoordinates, RandomAccessSource, ReadRequest,
    SourceId, SourceInfo, SourceInput, SourceResult,
};

const SEGMENT_HEADER: usize = 32;
const FILE_HEADER_BODY: usize = 80;
const DIRECTORY_HEADER: usize = 128;
const SUBBLOCK_HEADER: usize = 256;
const DIMENSION_COUNT: usize = 3;
const DIRECTORY_ENTRY_SIZE: usize = 32 + DIMENSION_COUNT * 20;
const ATTACHMENT_HEADER: usize = 256;
const ATTACHMENT_ENTRY_SIZE: usize = 128;

struct RecordingSource {
    info: SourceInfo,
    bytes: Arc<[u8]>,
    ranges: Arc<Mutex<Vec<(u64, usize)>>>,
}

impl RandomAccessSource for RecordingSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        self.ranges
            .lock()
            .expect("CZI attachment range recorder")
            .push((offset, destination.len()));
        let start = usize::try_from(offset)?;
        let end = start
            .checked_add(destination.len())
            .ok_or_else(|| std::io::Error::other("recording source range overflow"))?;
        let source = self
            .bytes
            .get(start..end)
            .ok_or_else(|| std::io::Error::other("recording source range out of bounds"))?;
        destination.copy_from_slice(source);
        Ok(())
    }
}

struct NestedCzi {
    bytes: Vec<u8>,
    directory_entry: usize,
    subblock: usize,
    pixels: Range<usize>,
}

struct AttachedCzi {
    bytes: Vec<u8>,
    label_container: Range<usize>,
    label_directory_entry: usize,
    label_subblock: usize,
    label_pixels: Range<usize>,
    preview_segment: usize,
    preview_pixels: Range<usize>,
}

#[test]
fn embedded_label_and_preview_are_lazy_independent_series() {
    let fixture = generated_czi_with_attachments();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let bytes: Arc<[u8]> = fixture.bytes.into();
    let source = Arc::new(RecordingSource {
        info: SourceInfo::new(
            SourceId::new("memory:czi-attachments"),
            "attachments.czi",
            bytes.len() as u64,
        ),
        bytes,
        ranges: Arc::clone(&ranges),
    });

    let dataset = open_source(SourceInput::new(source)).expect("open attached CZI source");
    assert_eq!(dataset.series().len(), 3);
    let label = dataset.series()[1].resolutions()[0].metadata();
    assert_eq!((label.size_x, label.size_y), (2, 1));
    assert_eq!(label.pixel_type, PixelType::Uint8);
    assert_eq!(label.samples_per_pixel, 3);
    assert!(matches!(
        label.series_metadata.get("czi_attachment_name"),
        Some(MetadataValue::String(name)) if name == "Label"
    ));
    let preview = dataset.series()[2].resolutions()[0].metadata();
    assert_eq!((preview.size_x, preview.size_y), (1, 1));
    assert_eq!(preview.pixel_type, PixelType::Uint16);
    assert_eq!(preview.samples_per_pixel, 3);
    assert!(matches!(
        preview.series_metadata.get("czi_attachment_name"),
        Some(MetadataValue::String(name)) if name == "SlidePreview"
    ));

    let opened_ranges = ranges.lock().expect("CZI attachment range recorder");
    assert!(!overlaps_any(&opened_ranges, &fixture.label_pixels));
    assert!(!overlaps_any(&opened_ranges, &fixture.preview_pixels));
    drop(opened_ranges);

    let label_plane = dataset
        .read_plane(ReadRequest::new(1, PlaneCoordinates::default()))
        .expect("read attached label");
    assert_eq!(label_plane.bytes(), [1, 2, 3, 4, 5, 6]);
    let label_ranges = ranges.lock().expect("CZI attachment range recorder");
    assert!(overlaps_any(&label_ranges, &fixture.label_pixels));
    assert!(!overlaps_any(&label_ranges, &fixture.preview_pixels));
    drop(label_ranges);

    let preview_plane = dataset
        .read_plane(ReadRequest::new(2, PlaneCoordinates::default()))
        .expect("read attached slide preview");
    assert_eq!(preview_plane.bytes(), [1, 0, 2, 0, 3, 0]);
    assert!(overlaps_any(
        &ranges.lock().expect("CZI attachment range recorder"),
        &fixture.preview_pixels
    ));
}

#[test]
fn embedded_subblock_pointer_cannot_escape_its_attachment() {
    let mut fixture = generated_czi_with_attachments();
    let pointer = fixture.label_directory_entry + 6;
    let escaped_position = i64::try_from(fixture.preview_segment - fixture.label_container.start)
        .expect("escaped test pointer fits i64");
    fixture.bytes[pointer..pointer + 8].copy_from_slice(&escaped_position.to_le_bytes());

    let error = match open_memory(fixture.bytes) {
        Ok(_) => panic!("escaping nested pointer must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("container"),
        "unexpected error: {error}"
    );
}

#[test]
fn embedded_subblock_segment_cannot_cross_its_attachment_end() {
    let mut fixture = generated_czi_with_attachments();
    let used_size_offset = fixture.label_subblock + 24;
    let crossing_used_size = fixture
        .label_container
        .end
        .checked_sub(fixture.label_subblock + SEGMENT_HEADER)
        .and_then(|remaining| remaining.checked_add(1))
        .expect("crossing test segment size");
    fixture.bytes[used_size_offset..used_size_offset + 8]
        .copy_from_slice(&(crossing_used_size as u64).to_le_bytes());

    let error = match open_memory(fixture.bytes) {
        Ok(_) => panic!("cross-container segment must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("embedded attachment"),
        "unexpected error: {error}"
    );
}

fn open_memory(bytes: Vec<u8>) -> bioformats_rs::Result<bioformats_rs::Dataset> {
    let bytes: Arc<[u8]> = bytes.into();
    let source = Arc::new(RecordingSource {
        info: SourceInfo::new(
            SourceId::new("memory:malformed-czi-attachment"),
            "malformed-attachment.czi",
            bytes.len() as u64,
        ),
        bytes,
        ranges: Arc::new(Mutex::new(Vec::new())),
    });
    open_source(SourceInput::new(source))
}

fn overlaps_any(reads: &[(u64, usize)], expected: &Range<usize>) -> bool {
    reads.iter().any(|(offset, length)| {
        let start = usize::try_from(*offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(*length);
        start < expected.end && end > expected.start
    })
}

fn generated_czi_with_attachments() -> AttachedCzi {
    let main = generated_single_tile_czi(1, 1, 0, &[9]);
    let label = generated_single_tile_czi(2, 1, 3, &[3, 2, 1, 6, 5, 4]);
    let preview = generated_single_tile_czi(1, 1, 4, &[3, 0, 2, 0, 1, 0]);

    // Keep attachment pixels beyond the registry's format-probe prefix so the
    // read-range assertions measure CZI initialization itself.
    let attachment_directory = 8192;
    let attachment_directory_used = ATTACHMENT_HEADER + 2 * ATTACHMENT_ENTRY_SIZE;
    let label_segment =
        align_segment(attachment_directory + SEGMENT_HEADER + attachment_directory_used);
    let label_container_start = label_segment + SEGMENT_HEADER + ATTACHMENT_HEADER;
    let preview_segment = align_segment(label_container_start + label.bytes.len());
    let preview_container_start = preview_segment + SEGMENT_HEADER + ATTACHMENT_HEADER;
    let file_end = align_segment(preview_container_start + preview.bytes.len());

    let mut bytes = main.bytes;
    bytes.resize(file_end, 0);
    bytes[SEGMENT_HEADER + 72..SEGMENT_HEADER + 80]
        .copy_from_slice(&(attachment_directory as u64).to_le_bytes());

    write_segment_header(
        &mut bytes,
        attachment_directory,
        b"ZISRAWATTDIR",
        attachment_directory_used as u64,
    );
    let attachment_directory_body = attachment_directory + SEGMENT_HEADER;
    bytes[attachment_directory_body..attachment_directory_body + 4]
        .copy_from_slice(&2_i32.to_le_bytes());
    write_attachment_entry(
        &mut bytes[attachment_directory_body + ATTACHMENT_HEADER
            ..attachment_directory_body + ATTACHMENT_HEADER + ATTACHMENT_ENTRY_SIZE],
        label_segment,
        "Label",
    );
    write_attachment_entry(
        &mut bytes[attachment_directory_body + ATTACHMENT_HEADER + ATTACHMENT_ENTRY_SIZE
            ..attachment_directory_body + ATTACHMENT_HEADER + 2 * ATTACHMENT_ENTRY_SIZE],
        preview_segment,
        "SlidePreview",
    );

    write_attachment_segment(&mut bytes, label_segment, "Label", &label.bytes);
    write_attachment_segment(&mut bytes, preview_segment, "SlidePreview", &preview.bytes);

    let label_container = label_container_start..label_container_start + label.bytes.len();
    let label_directory_entry = label_container_start + label.directory_entry;
    let label_subblock = label_container_start + label.subblock;
    let label_pixels =
        label_container_start + label.pixels.start..label_container_start + label.pixels.end;
    let preview_pixels = preview_container_start + preview.pixels.start
        ..preview_container_start + preview.pixels.end;
    AttachedCzi {
        bytes,
        label_container,
        label_directory_entry,
        label_subblock,
        label_pixels,
        preview_segment,
        preview_pixels,
    }
}

fn generated_single_tile_czi(width: i32, height: i32, pixel_type: i32, pixels: &[u8]) -> NestedCzi {
    let directory_position = align_segment(SEGMENT_HEADER + FILE_HEADER_BODY);
    let directory_used = DIRECTORY_HEADER + DIRECTORY_ENTRY_SIZE;
    let subblock = align_segment(directory_position + SEGMENT_HEADER + directory_used);
    let pixel_start = subblock + SEGMENT_HEADER + SUBBLOCK_HEADER;
    let file_end = align_segment(pixel_start + pixels.len());
    let mut bytes = vec![0_u8; file_end];

    write_segment_header(&mut bytes, 0, b"ZISRAWFILE", FILE_HEADER_BODY as u64);
    bytes[SEGMENT_HEADER + 52..SEGMENT_HEADER + 60]
        .copy_from_slice(&(directory_position as u64).to_le_bytes());
    write_segment_header(
        &mut bytes,
        directory_position,
        b"ZISRAWDIRECTORY",
        directory_used as u64,
    );
    let directory_body = directory_position + SEGMENT_HEADER;
    bytes[directory_body..directory_body + 4].copy_from_slice(&1_i32.to_le_bytes());
    let directory_entry = directory_body + DIRECTORY_HEADER;
    write_directory_entry(
        &mut bytes[directory_entry..directory_entry + DIRECTORY_ENTRY_SIZE],
        subblock,
        width,
        height,
        pixel_type,
    );

    write_segment_header(
        &mut bytes,
        subblock,
        b"ZISRAWSUBBLOCK",
        (SUBBLOCK_HEADER + pixels.len()) as u64,
    );
    let subblock_body = subblock + SEGMENT_HEADER;
    bytes[subblock_body + 8..subblock_body + 16]
        .copy_from_slice(&(pixels.len() as u64).to_le_bytes());
    write_directory_entry(
        &mut bytes[subblock_body + 16..subblock_body + 16 + DIRECTORY_ENTRY_SIZE],
        subblock,
        width,
        height,
        pixel_type,
    );
    bytes[pixel_start..pixel_start + pixels.len()].copy_from_slice(pixels);

    NestedCzi {
        bytes,
        directory_entry,
        subblock,
        pixels: pixel_start..pixel_start + pixels.len(),
    }
}

fn write_directory_entry(
    bytes: &mut [u8],
    subblock: usize,
    width: i32,
    height: i32,
    pixel_type: i32,
) {
    bytes[0..2].copy_from_slice(b"DV");
    bytes[2..6].copy_from_slice(&pixel_type.to_le_bytes());
    bytes[6..14].copy_from_slice(&(subblock as i64).to_le_bytes());
    bytes[28..32].copy_from_slice(&(DIMENSION_COUNT as i32).to_le_bytes());
    for (index, (name, size)) in [(b"X\0\0\0", width), (b"Y\0\0\0", height), (b"C\0\0\0", 1)]
        .into_iter()
        .enumerate()
    {
        let dimension = 32 + index * 20;
        bytes[dimension..dimension + 4].copy_from_slice(name);
        bytes[dimension + 8..dimension + 12].copy_from_slice(&size.to_le_bytes());
        bytes[dimension + 16..dimension + 20].copy_from_slice(&size.to_le_bytes());
    }
}

fn write_attachment_segment(bytes: &mut [u8], position: usize, name: &str, data: &[u8]) {
    write_segment_header(
        bytes,
        position,
        b"ZISRAWATTACH",
        (ATTACHMENT_HEADER + data.len()) as u64,
    );
    let body = position + SEGMENT_HEADER;
    bytes[body..body + 4].copy_from_slice(&(data.len() as i32).to_le_bytes());
    write_attachment_entry(
        &mut bytes[body + 16..body + 16 + ATTACHMENT_ENTRY_SIZE],
        position,
        name,
    );
    bytes[body + ATTACHMENT_HEADER..body + ATTACHMENT_HEADER + data.len()].copy_from_slice(data);
}

fn write_attachment_entry(bytes: &mut [u8], position: usize, name: &str) {
    bytes[0..2].copy_from_slice(b"A1");
    bytes[12..20].copy_from_slice(&(position as i64).to_le_bytes());
    bytes[40..43].copy_from_slice(b"CZI");
    bytes[48..48 + name.len()].copy_from_slice(name.as_bytes());
}

fn write_segment_header(bytes: &mut [u8], offset: usize, kind: &[u8], used: u64) {
    bytes[offset..offset + kind.len()].copy_from_slice(kind);
    bytes[offset + 16..offset + 24].copy_from_slice(&used.to_le_bytes());
    bytes[offset + 24..offset + 32].copy_from_slice(&used.to_le_bytes());
}

fn align_segment(position: usize) -> usize {
    position.checked_add(31).expect("generated CZI overflow") / 32 * 32
}
