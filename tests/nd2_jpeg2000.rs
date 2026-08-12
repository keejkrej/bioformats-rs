use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats_rs::{
    open, open_source, BioFormatsError, DimensionOrder, FormatId, FormatReader, ImageReader,
    PixelType, PlaneCoordinates, RandomAccessSource, ReadRequest, Rect, Region, SourceId,
    SourceInfo, SourceInput, SourceResult,
};
use dicom_toolkit_jpeg2000::{encode, EncodeOptions};

const WIDTH: u32 = 3;
const HEIGHT: u32 = 2;

struct LegacyFixture {
    bytes: Vec<u8>,
    codestream_ranges: Vec<Range<usize>>,
}

struct MemorySource {
    info: SourceInfo,
    bytes: Arc<[u8]>,
}

impl MemorySource {
    fn new(identity: &str, name: &str, bytes: Vec<u8>) -> Self {
        let bytes: Arc<[u8]> = bytes.into();
        Self {
            info: SourceInfo::new(SourceId::new(identity), name, bytes.len() as u64),
            bytes,
        }
    }
}

impl RandomAccessSource for MemorySource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        let start = usize::try_from(offset)?;
        let end = start
            .checked_add(destination.len())
            .ok_or_else(|| std::io::Error::other("memory source range overflow"))?;
        let source = self
            .bytes
            .get(start..end)
            .ok_or_else(|| std::io::Error::other("memory source range out of bounds"))?;
        destination.copy_from_slice(source);
        Ok(())
    }
}

struct RecordingSource {
    inner: MemorySource,
    ranges: Arc<Mutex<Vec<Range<usize>>>>,
}

impl RandomAccessSource for RecordingSource {
    fn info(&self) -> &SourceInfo {
        self.inner.info()
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        let start = usize::try_from(offset)?;
        let end = start
            .checked_add(destination.len())
            .ok_or_else(|| std::io::Error::other("recorded source range overflow"))?;
        self.ranges.lock().expect("range recorder").push(start..end);
        self.inner.read_at(offset, destination)
    }
}

struct TemporaryLegacyFile {
    directory: PathBuf,
    path: PathBuf,
}

impl TemporaryLegacyFile {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "bioformats_rs_nd2_jpeg2000_{}_{}_{}",
            std::process::id(),
            unique,
            name.replace('.', "_")
        ));
        fs::create_dir(&directory).expect("create temporary ND2 directory");
        let path = directory.join(name);
        fs::write(&path, bytes).expect("write temporary legacy ND2");
        Self { directory, path }
    }

    fn rename(&mut self, name: &str) {
        let next = self.directory.join(name);
        fs::rename(&self.path, &next).expect("relocate temporary legacy ND2");
        self.path = next;
    }
}

impl Drop for TemporaryLegacyFile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn generated_legacy_jpeg2000_maps_scalar_channels_and_series() {
    let planes = vec![
        vec![1, 2, 3, 4, 5, 6],
        vec![11, 12, 13, 14, 15, 16],
        vec![21, 22, 23, 24, 25, 26],
        vec![31, 32, 33, 34, 35, 36],
    ];
    let fixture = legacy_fixture(&planes, 8, 2, 2);
    let file = TemporaryLegacyFile::new("generated.nd2", &fixture.bytes);

    let mut reader = ImageReader::open(&file.path).expect("open generated legacy ND2");
    assert_eq!(reader.format(), Some(FormatId::Nd2));
    assert_eq!(reader.series_count(), 2);
    assert_legacy_scalar_metadata(reader.metadata(), 8, 2);
    assert_eq!(reader.open_bytes(0).expect("read S0 C0"), planes[0]);
    assert_eq!(reader.open_bytes(1).expect("read S0 C1"), planes[1]);
    assert_eq!(
        reader
            .open_bytes_region(0, 1, 0, 2, 2)
            .expect("read S0 C0 region"),
        [2, 3, 5, 6]
    );

    reader.set_series(1).expect("select generated series 1");
    assert_legacy_scalar_metadata(reader.metadata(), 8, 2);
    assert_eq!(reader.open_bytes(0).expect("read S1 C0"), planes[2]);
    assert_eq!(reader.open_bytes(1).expect("read S1 C1"), planes[3]);
    assert_eq!(
        reader
            .open_bytes_region(1, 1, 0, 2, 2)
            .expect("read S1 C1 region"),
        [32, 33, 35, 36]
    );

    let dataset = open(&file.path).expect("open generated legacy ND2 dataset");
    assert_eq!(dataset.format(), FormatId::Nd2);
    assert_eq!(dataset.series().len(), 2);
    for series in dataset.series() {
        assert_legacy_scalar_metadata(series.resolutions()[0].metadata(), 8, 2);
    }
    let request = ReadRequest::new(1, PlaneCoordinates::new(0, 1, 0));
    let info = dataset.plane_info(request).expect("preflight legacy plane");
    assert_eq!(info.byte_len, 6);
    assert_eq!(
        dataset
            .read_plane(request)
            .expect("read legacy plane")
            .bytes(),
        planes[3]
    );
    let region_request = request.with_region(Region::Rect(
        Rect::new(1, 0, 2, 2).expect("valid generated region"),
    ));
    let mut destination = [0xa5; 7];
    let info = dataset
        .read_plane_into(region_request, &mut destination)
        .expect("read legacy region into caller storage");
    assert_eq!(info.byte_len, 4);
    assert_eq!(destination, [32, 33, 35, 36, 0xa5, 0xa5, 0xa5]);
}

#[test]
fn generated_legacy_u16_is_exposed_in_file_big_endian_order() {
    assert!(FormatId::Nd2.extensions().contains(&"jp2"));
    let little_endian_pixels = [0x0102_u16, 0x0304, 0x0506, 0x0708, 0x090a, 0x0b0c]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    let expected_big_endian = [0x0102_u16, 0x0304, 0x0506, 0x0708, 0x090a, 0x0b0c]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
    let fixture = legacy_fixture(&[little_endian_pixels], 16, 1, 1);

    let source = Arc::new(MemorySource::new(
        "memory:legacy-jp2",
        "generated.jp2",
        fixture.bytes,
    ));
    let dataset = open_source(SourceInput::new(source)).expect("open legacy .jp2 source");
    assert_eq!(dataset.format(), FormatId::Nd2);
    let metadata = dataset.series()[0].resolutions()[0].metadata();
    assert_legacy_scalar_metadata(metadata, 16, 1);
    assert_eq!(metadata.pixel_type, PixelType::Uint16);
    assert_eq!(
        dataset
            .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
            .expect("decode generated u16 JPEG 2000")
            .bytes(),
        expected_big_endian
    );
}

#[test]
fn legacy_open_scans_boxes_without_materializing_the_codestream() {
    let codestream = add_large_jpeg2000_comment(encode_plane(&[1, 2, 3, 4, 5, 6], 8));
    let fixture = legacy_fixture_from_codestreams(&[codestream], 8, 1, 1);
    let codestream = fixture.codestream_ranges[0].clone();
    let guarded_byte = codestream.start + codestream.len() / 2;
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let source = Arc::new(RecordingSource {
        inner: MemorySource::new("memory:lazy-legacy-nd2", "lazy.nd2", fixture.bytes),
        ranges: Arc::clone(&ranges),
    });

    let dataset = open_source(SourceInput::new(source)).expect("open lazy legacy ND2");
    let opening_ranges = ranges.lock().expect("opening ranges").clone();
    assert!(
        opening_ranges
            .iter()
            .all(|range| !range.contains(&guarded_byte)),
        "metadata opening unexpectedly fetched compressed pixel storage: {opening_ranges:?}"
    );

    assert_eq!(
        dataset
            .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
            .expect("decode lazy legacy ND2")
            .bytes(),
        [1, 2, 3, 4, 5, 6]
    );
    assert!(
        ranges
            .lock()
            .expect("all ranges")
            .iter()
            .any(|range| range.contains(&guarded_byte)),
        "pixel read did not fetch the compressed codestream"
    );
}

#[test]
fn legacy_snapshot_retargets_and_preserves_the_active_series() {
    let planes = vec![vec![1, 2, 3, 4, 5, 6], vec![11, 12, 13, 14, 15, 16]];
    let fixture = legacy_fixture(&planes, 8, 1, 2);
    let mut file = TemporaryLegacyFile::new("before.nd2", &fixture.bytes);
    let mut reader = ImageReader::open(&file.path).expect("open snapshot legacy ND2");
    reader.set_series(1).expect("select legacy series 1");
    let mut snapshot = reader.snapshot().expect("snapshot legacy ND2");

    file.rename("after.nd2");
    snapshot.retarget_path(&file.path);
    let mut restored = snapshot
        .into_reader()
        .expect("restore relocated legacy ND2");
    assert_eq!(restored.series(), 1);
    assert_eq!(restored.series_count(), 2);
    assert_eq!(restored.current_file(), Some(file.path.as_path()));
    assert_eq!(restored.used_files(), [file.path.clone()]);
    let restored_plane = restored.open_bytes(0).expect("read restored series");
    assert_eq!(restored_plane, [11, 12, 13, 14, 15, 16]);
    assert_ne!(restored_plane, [1, 2, 3, 4, 5, 6]);
}

#[test]
fn legacy_rejects_malformed_boxes_and_inconsistent_metadata() {
    let valid = legacy_fixture(&[vec![1, 2, 3, 4, 5, 6]], 8, 1, 1);

    let mut short_box = valid.bytes.clone();
    short_box[12..16].copy_from_slice(&4_u32.to_be_bytes());
    assert_container_error(open_memory(short_box, "short-box.nd2"));

    let missing_ihdr = legacy_fixture_without_ihdr(&encode_plane(&[1, 2, 3, 4, 5, 6], 8));
    assert_container_error(open_memory(missing_ihdr, "missing-ihdr.nd2"));

    let mut signed = valid.bytes.clone();
    let siz_component = valid.codestream_ranges[0].start + 42;
    signed[siz_component] |= 0x80;
    assert_container_error(open_memory(signed, "signed.nd2"));

    let mut tile_bomb = valid.bytes.clone();
    let siz = valid.codestream_ranges[0].start;
    tile_bomb[siz + 8..siz + 12].copy_from_slice(&60_000_u32.to_be_bytes());
    tile_bomb[siz + 12..siz + 16].copy_from_slice(&60_000_u32.to_be_bytes());
    tile_bomb[siz + 24..siz + 28].copy_from_slice(&1_u32.to_be_bytes());
    tile_bomb[siz + 28..siz + 32].copy_from_slice(&1_u32.to_be_bytes());
    let tile_error = match open_memory(tile_bomb, "tile-bomb.nd2") {
        Ok(_) => panic!("pathological JPEG 2000 tile grids must be bounded"),
        Err(error) => error,
    };
    assert!(
        matches!(&tile_error, BioFormatsError::UnsupportedFormat(message) if message.contains("tiles")),
        "unexpected tile-grid error: {tile_error:?}"
    );

    let mut mismatched_ihdr = valid.bytes;
    let ihdr_height = find_box_payload(&mismatched_ihdr, b"ihdr").expect("generated ihdr");
    mismatched_ihdr[ihdr_height..ihdr_height + 4].copy_from_slice(&3_u32.to_be_bytes());
    assert_container_error(open_memory(mismatched_ihdr, "mismatched-ihdr.nd2"));
}

#[test]
fn legacy_decoder_failures_are_structured_codec_errors() {
    let encoded = encode_plane(&[1, 2, 3, 4, 5, 6], 8);
    let siz_end = siz_segment_end(&encoded);
    let fixture = legacy_fixture_from_codestreams(&[encoded[..siz_end].to_vec()], 8, 1, 1);
    let dataset = open_memory(fixture.bytes, "truncated-codec.nd2")
        .expect("SIZ metadata remains readable before pixel decode");
    let error = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect_err("truncated JPEG 2000 must fail decoding");
    assert!(
        matches!(error, BioFormatsError::Codec(_)),
        "unexpected decoder error: {error:?}"
    );
}

fn assert_legacy_scalar_metadata(
    metadata: &bioformats_rs::ImageMetadata,
    bit_depth: u8,
    channels: u32,
) {
    assert_eq!((metadata.size_x, metadata.size_y), (WIDTH, HEIGHT));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (1, channels, 1)
    );
    assert_eq!(metadata.image_count, channels);
    assert_eq!(metadata.bits_per_pixel, bit_depth);
    assert_eq!(
        metadata.pixel_type,
        if bit_depth <= 8 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        }
    );
    assert_eq!(metadata.samples_per_pixel, 1);
    assert_eq!(
        metadata.dimension_order,
        if channels > 1 {
            DimensionOrder::XYCZT
        } else {
            DimensionOrder::XYZCT
        }
    );
    assert!(!metadata.is_rgb);
    assert!(!metadata.is_interleaved);
    assert!(!metadata.is_indexed);
    assert!(metadata.is_false_color);
    assert!(!metadata.is_little_endian);
}

fn open_memory(bytes: Vec<u8>, name: &str) -> bioformats_rs::Result<bioformats_rs::Dataset> {
    let source = Arc::new(MemorySource::new(&format!("memory:{name}"), name, bytes));
    open_source(SourceInput::new(source))
}

fn assert_container_error(result: bioformats_rs::Result<bioformats_rs::Dataset>) {
    let error = result.err().expect("malformed legacy ND2 must be rejected");
    assert!(
        matches!(
            error,
            BioFormatsError::Format(_)
                | BioFormatsError::UnsupportedFormat(_)
                | BioFormatsError::InvalidData(_)
                | BioFormatsError::SourceRangeOutOfBounds { .. }
        ),
        "unexpected malformed-container error: {error:?}"
    );
}

fn legacy_fixture(planes: &[Vec<u8>], bit_depth: u8, channels: u32, series: u32) -> LegacyFixture {
    let codestreams = planes
        .iter()
        .map(|plane| encode_plane(plane, bit_depth))
        .collect::<Vec<_>>();
    legacy_fixture_from_codestreams(&codestreams, bit_depth, channels, series)
}

fn legacy_fixture_from_codestreams(
    codestreams: &[Vec<u8>],
    bit_depth: u8,
    channels: u32,
    series: u32,
) -> LegacyFixture {
    assert_eq!(codestreams.len(), (channels * series) as usize);
    let mut bytes = Vec::new();
    push_box(&mut bytes, *b"jP  ", &[0x0d, 0x0a, 0x87, 0x0a]);
    push_box(&mut bytes, *b"ftyp", b"jp2 \0\0\0\0jp2 ");

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&HEIGHT.to_be_bytes());
    ihdr.extend_from_slice(&WIDTH.to_be_bytes());
    ihdr.extend_from_slice(&1_u16.to_be_bytes());
    ihdr.push(bit_depth - 1);
    ihdr.extend_from_slice(&[7, 1, 0]);
    let mut jp2h = Vec::new();
    push_box(&mut jp2h, *b"ihdr", &ihdr);
    push_box(&mut bytes, *b"jp2h", &jp2h);

    let mut codestream_ranges = Vec::with_capacity(codestreams.len());
    for codestream in codestreams {
        let start = bytes.len() + 8;
        push_box(&mut bytes, *b"jp2c", codestream);
        codestream_ranges.push(start..start + codestream.len());
    }

    let mut descriptions = String::new();
    for channel in 0..channels {
        descriptions.push_str(&format!(
            "<sDescription value=\"generated-C{channel}\"/><EmissionWavelength value=\"525\"/>"
        ));
    }
    let xml = format!(
        "<variant><VirtualComponents value=\"{channels}\"/><XYCount value=\"{series}\"/>{descriptions}</variant>"
    );
    push_box(&mut bytes, *b"xml ", xml.as_bytes());

    LegacyFixture {
        bytes,
        codestream_ranges,
    }
}

fn legacy_fixture_without_ihdr(codestream: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_box(&mut bytes, *b"jP  ", &[0x0d, 0x0a, 0x87, 0x0a]);
    push_box(&mut bytes, *b"ftyp", b"jp2 \0\0\0\0jp2 ");
    push_box(&mut bytes, *b"jp2c", codestream);
    push_box(
        &mut bytes,
        *b"xml ",
        br#"<variant><VirtualComponents value="1"/><XYCount value="1"/></variant>"#,
    );
    bytes
}

fn encode_plane(pixels: &[u8], bit_depth: u8) -> Vec<u8> {
    let options = EncodeOptions {
        num_decomposition_levels: 0,
        ..EncodeOptions::default()
    };
    encode(pixels, WIDTH, HEIGHT, 1, bit_depth, false, &options)
        .expect("encode generated JPEG 2000 plane")
}

fn add_large_jpeg2000_comment(mut codestream: Vec<u8>) -> Vec<u8> {
    let insert_at = siz_segment_end(&codestream);
    const COMMENT_LENGTH: u16 = 60_000;
    let mut comment = Vec::with_capacity(usize::from(COMMENT_LENGTH) + 2);
    comment.extend_from_slice(&[0xff, 0x64]);
    comment.extend_from_slice(&COMMENT_LENGTH.to_be_bytes());
    comment.extend_from_slice(&0_u16.to_be_bytes());
    comment.resize(usize::from(COMMENT_LENGTH) + 2, 0xa5);
    codestream.splice(insert_at..insert_at, comment);
    codestream
}

fn siz_segment_end(codestream: &[u8]) -> usize {
    assert!(codestream.starts_with(&[0xff, 0x4f, 0xff, 0x51]));
    let length = u16::from_be_bytes([codestream[4], codestream[5]]) as usize;
    4 + length
}

fn push_box(bytes: &mut Vec<u8>, kind: [u8; 4], payload: &[u8]) {
    let length = u32::try_from(payload.len() + 8).expect("generated JP2 box length");
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&kind);
    bytes.extend_from_slice(payload);
}

fn find_box_payload(bytes: &[u8], target: &[u8; 4]) -> Option<usize> {
    fn find(bytes: &[u8], target: &[u8; 4], start: usize, end: usize) -> Option<usize> {
        let mut cursor = start;
        while cursor + 8 <= end {
            let length = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into().ok()?) as usize;
            if length < 8 || cursor.checked_add(length)? > end {
                return None;
            }
            let kind: &[u8; 4] = bytes[cursor + 4..cursor + 8].try_into().ok()?;
            if kind == target {
                return Some(cursor + 8);
            }
            if kind == b"jp2h" {
                if let Some(found) = find(bytes, target, cursor + 8, cursor + length) {
                    return Some(found);
                }
            }
            cursor += length;
        }
        None
    }
    find(bytes, target, 0, bytes.len())
}
