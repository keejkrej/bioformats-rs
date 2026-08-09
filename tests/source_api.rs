use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bioformats_rs::{
    open, open_source, BioFormatsError, CompanionReference, CompanionResolver, FormatReader,
    PlaneCoordinates, RandomAccessSource, ReadRequest, Rect, Region, SourceId, SourceInfo,
    SourceInput, SourceResult, TiffReader,
};

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

    fn with_declared_len(identity: &str, name: &str, bytes: Vec<u8>, declared_len: u64) -> Self {
        Self {
            info: SourceInfo::new(SourceId::new(identity), name, declared_len),
            bytes: bytes.into(),
        }
    }
}

struct RecordingSource {
    inner: MemorySource,
    ranges: Arc<Mutex<Vec<(u64, usize)>>>,
}

struct FailAtOrAfterRangeSource {
    inner: MemorySource,
    first_failing_offset: u64,
}

impl RandomAccessSource for FailAtOrAfterRangeSource {
    fn info(&self) -> &SourceInfo {
        self.inner.info()
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        if offset >= self.first_failing_offset {
            return Err(std::io::Error::other("injected post-header range failure").into());
        }
        self.inner.read_at(offset, destination)
    }
}

impl RandomAccessSource for RecordingSource {
    fn info(&self) -> &SourceInfo {
        self.inner.info()
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        self.ranges
            .lock()
            .expect("range recorder lock")
            .push((offset, destination.len()));
        self.inner.read_at(offset, destination)
    }
}

struct MapResolver {
    named: HashMap<String, Arc<dyn RandomAccessSource>>,
    requests: Arc<Mutex<Vec<String>>>,
}

struct SiblingResolver {
    siblings: Vec<Arc<dyn RandomAccessSource>>,
    requests: Arc<Mutex<usize>>,
}

impl CompanionResolver for SiblingResolver {
    fn resolve(
        &self,
        _from: &SourceInfo,
        reference: CompanionReference<'_>,
    ) -> SourceResult<Vec<Arc<dyn RandomAccessSource>>> {
        match reference {
            CompanionReference::Siblings => {
                *self.requests.lock().expect("sibling request lock") += 1;
                Ok(self.siblings.clone())
            }
            CompanionReference::Named(_) => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }
}

impl CompanionResolver for MapResolver {
    fn resolve(
        &self,
        _from: &SourceInfo,
        reference: CompanionReference<'_>,
    ) -> SourceResult<Vec<Arc<dyn RandomAccessSource>>> {
        match reference {
            CompanionReference::Named(name) => {
                self.requests
                    .lock()
                    .expect("resolver request lock")
                    .push(name.to_owned());
                Ok(self.named.get(name).cloned().into_iter().collect())
            }
            CompanionReference::Siblings => Ok(Vec::new()),
            _ => Ok(Vec::new()),
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
            .ok_or_else(|| std::io::Error::other("memory-source range overflow"))?;
        let source = self
            .bytes
            .get(start..end)
            .ok_or_else(|| std::io::Error::other("memory-source range out of bounds"))?;
        destination.copy_from_slice(source);
        Ok(())
    }
}

#[test]
fn application_owned_source_opens_and_reads_without_a_file() {
    let source = Arc::new(MemorySource::new(
        "memory:single-tiff",
        "single.tif",
        basic_tiff(),
    ));
    let dataset = open_source(SourceInput::new(source)).expect("open in-memory TIFF");

    assert!(dataset.used_files().is_empty());
    assert_eq!(dataset.used_sources().len(), 1);
    assert_eq!(
        dataset.used_sources()[0].identity(),
        &SourceId::new("memory:single-tiff")
    );

    let request = ReadRequest::new(0, PlaneCoordinates::default())
        .with_region(Region::Rect(Rect::new(1, 0, 2, 2).expect("valid region")));
    let plane = dataset.read_plane(request).expect("read in-memory region");
    assert_eq!(plane.bytes(), &[2, 3, 5, 6]);
}

#[test]
fn source_reads_are_bounded_ranges_and_pixels_remain_lazy() {
    const PIXEL_OFFSET: usize = 64 * 1024;
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let bytes = padded_tiff(PIXEL_OFFSET);
    let source_len = bytes.len();
    let source = Arc::new(RecordingSource {
        inner: MemorySource::new("memory:recorded-tiff", "recorded.tif", bytes),
        ranges: Arc::clone(&ranges),
    });

    let dataset = open_source(SourceInput::new(source)).expect("open recorded TIFF");
    let opening_ranges = ranges.lock().expect("opening ranges").clone();
    assert!(!opening_ranges.is_empty());
    assert!(opening_ranges
        .iter()
        .all(|(offset, length)| (*offset as usize) + length <= source_len));
    assert!(opening_ranges
        .iter()
        .all(|(_, length)| *length < source_len));
    assert!(
        opening_ranges
            .iter()
            .all(|(offset, _)| *offset < PIXEL_OFFSET as u64),
        "opening must not fetch pixel storage"
    );

    dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect("read recorded plane");
    let all_ranges = ranges.lock().expect("all ranges");
    assert!(all_ranges.iter().any(|(offset, length)| {
        *offset <= PIXEL_OFFSET as u64 && *offset + *length as u64 >= PIXEL_OFFSET as u64 + 6
    }));
}

#[test]
fn cursor_backed_pixel_failures_remain_structured_source_errors() {
    const PIXEL_OFFSET: usize = 64 * 1024;
    let source: Arc<dyn RandomAccessSource> = Arc::new(FailAtOrAfterRangeSource {
        inner: MemorySource::new(
            "memory:failing-tiff-pixels",
            "failing-pixels.tif",
            padded_tiff(PIXEL_OFFSET),
        ),
        first_failing_offset: PIXEL_OFFSET as u64,
    });
    let mut reader = TiffReader::new();
    reader
        .set_source(SourceInput::new(Arc::clone(&source)))
        .expect("open low-level TIFF before pixel failure");
    let low_level_error = reader
        .open_bytes(0)
        .expect_err("low-level pixel range failure must be returned");
    assert!(
        matches!(low_level_error, BioFormatsError::SourceRead { .. }),
        "unexpected low-level error: {low_level_error:?}"
    );

    let dataset = open_source(SourceInput::new(source)).expect("open TIFF before pixel failure");

    let error = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect_err("pixel range failure must be returned");
    assert!(
        matches!(error, BioFormatsError::SourceRead { .. }),
        "unexpected error: {error:?}"
    );
}

#[test]
fn malformed_source_length_is_a_recoverable_error() {
    let source = Arc::new(MemorySource::with_declared_len(
        "memory:bad-length",
        "bad-length.tif",
        basic_tiff(),
        4096,
    ));
    let error = open_source(SourceInput::new(source))
        .err()
        .expect("lying source length must fail");
    assert!(
        matches!(error, BioFormatsError::SourceRead { .. }),
        "unexpected error: {error:?}"
    );
}

#[test]
fn application_source_dataset_is_concurrent_and_preserves_buffer_suffixes() {
    let source = Arc::new(MemorySource::new(
        "memory:concurrent-tiff",
        "concurrent.tif",
        basic_tiff(),
    ));
    let dataset = Arc::new(open_source(SourceInput::new(source)).expect("open concurrent TIFF"));

    let workers = (0..4)
        .map(|_| {
            let dataset = Arc::clone(&dataset);
            std::thread::spawn(move || {
                let mut destination = [0xaa_u8; 8];
                dataset
                    .read_plane_into(
                        ReadRequest::new(0, PlaneCoordinates::default()),
                        &mut destination,
                    )
                    .expect("concurrent caller-buffer read");
                destination
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        assert_eq!(
            worker.join().expect("source worker"),
            [1, 2, 3, 4, 5, 6, 0xaa, 0xaa]
        );
    }
}

#[test]
fn detached_nrrd_pixels_are_resolved_from_an_application_source() {
    let header =
        b"NRRD0004\ntype: uint8\ndimension: 2\nsizes: 3 2\nencoding: raw\ndata file: pixels.raw\n"
            .to_vec();
    let primary = Arc::new(MemorySource::new(
        "memory:nrrd-header",
        "dataset.nhdr",
        header,
    ));
    let pixels: Arc<dyn RandomAccessSource> = Arc::new(MemorySource::new(
        "memory:nrrd-pixels",
        "pixels.raw",
        vec![1, 2, 3, 4, 5, 6],
    ));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let resolver = Arc::new(MapResolver {
        named: HashMap::from([("pixels.raw".to_owned(), pixels)]),
        requests: Arc::clone(&requests),
    });

    let dataset = open_source(SourceInput::new(primary).with_companion_resolver(resolver))
        .expect("open detached NRRD sources");
    assert_eq!(
        requests.lock().expect("resolver requests").as_slice(),
        ["pixels.raw"]
    );
    assert_eq!(
        dataset
            .used_sources()
            .iter()
            .map(|source| source.identity().as_str())
            .collect::<Vec<_>>(),
        ["memory:nrrd-header", "memory:nrrd-pixels"]
    );
    assert!(dataset.used_files().is_empty());

    let plane = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect("read detached NRRD pixels");
    assert_eq!(plane.bytes(), &[1, 2, 3, 4, 5, 6]);
}

#[test]
fn multi_file_ome_tiff_members_are_resolved_by_logical_name() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8"
            SizeX="3" SizeY="2" SizeZ="2" SizeC="1" SizeT="1">
      <Channel ID="Channel:0:0" SamplesPerPixel="1"/>
      <TiffData FirstZ="0" FirstC="0" FirstT="0" IFD="0" PlaneCount="1"
                FileName="plane-0.tif"/>
      <TiffData FirstZ="1" FirstC="0" FirstT="0" IFD="0" PlaneCount="1"
                FileName="plane-1.tif"/>
    </Pixels>
  </Image>
</OME>"#;
    let primary = Arc::new(MemorySource::new(
        "memory:ome-metadata",
        "dataset.companion.ome",
        xml.as_bytes().to_vec(),
    ));
    let plane_0: Arc<dyn RandomAccessSource> = Arc::new(MemorySource::new(
        "memory:ome-plane-0",
        "plane-0.tif",
        basic_tiff_with_pixels([1, 2, 3, 4, 5, 6]),
    ));
    let plane_1: Arc<dyn RandomAccessSource> = Arc::new(MemorySource::new(
        "memory:ome-plane-1",
        "plane-1.tif",
        basic_tiff_with_pixels([11, 12, 13, 14, 15, 16]),
    ));
    let resolver = Arc::new(MapResolver {
        named: HashMap::from([
            ("plane-0.tif".to_owned(), plane_0),
            ("plane-1.tif".to_owned(), plane_1),
        ]),
        requests: Arc::new(Mutex::new(Vec::new())),
    });

    let dataset = open_source(SourceInput::new(primary).with_companion_resolver(resolver))
        .expect("open multi-file OME-TIFF sources");
    assert_eq!(
        dataset
            .used_sources()
            .iter()
            .map(|source| source.identity().as_str())
            .collect::<Vec<_>>(),
        [
            "memory:ome-metadata",
            "memory:ome-plane-0",
            "memory:ome-plane-1"
        ]
    );
    assert!(dataset.used_files().is_empty());

    let first = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)))
        .expect("read first OME member");
    let second = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(1, 0, 0)))
        .expect("read second OME member");
    assert_eq!(first.bytes(), &[1, 2, 3, 4, 5, 6]);
    assert_eq!(second.bytes(), &[11, 12, 13, 14, 15, 16]);
}

#[test]
fn split_czi_parts_are_discovered_and_ordered_through_the_resolver() {
    let master = Arc::new(MemorySource::new(
        "memory:czi-master",
        "sample.czi",
        minimal_czi(0, [1, 2, 3, 4, 5, 6]),
    ));
    let part: Arc<dyn RandomAccessSource> = Arc::new(MemorySource::new(
        "memory:czi-part-1",
        "sample (1).czi",
        minimal_czi(1, [11, 12, 13, 14, 15, 16]),
    ));
    let master_as_source: Arc<dyn RandomAccessSource> = master.clone();
    let sibling_requests = Arc::new(Mutex::new(0));
    let resolver = Arc::new(SiblingResolver {
        // Resolver order is intentionally not dataset order and includes the
        // primary; the reader must de-duplicate and sort by CZI part index.
        siblings: vec![part, master_as_source],
        requests: Arc::clone(&sibling_requests),
    });

    let dataset = open_source(SourceInput::new(master).with_companion_resolver(resolver))
        .expect("open split CZI sources");
    assert_eq!(*sibling_requests.lock().expect("sibling requests"), 1);
    assert_eq!(
        dataset
            .used_sources()
            .iter()
            .map(|source| source.identity().as_str())
            .collect::<Vec<_>>(),
        ["memory:czi-master", "memory:czi-part-1"]
    );

    let first = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)))
        .expect("read CZI master plane");
    let second = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(1, 0, 0)))
        .expect("read CZI part plane");
    assert_eq!(first.bytes(), &[1, 2, 3, 4, 5, 6]);
    assert_eq!(second.bytes(), &[11, 12, 13, 14, 15, 16]);
}

#[test]
fn path_open_keeps_split_czi_discovery_backwards_compatible() {
    let fixture = SplitCziFixture::new();
    let dataset = open(&fixture.master).expect("open split CZI path");

    assert_eq!(dataset.used_files().len(), 2);
    assert_eq!(dataset.used_sources().len(), 2);
    let second = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(1, 0, 0)))
        .expect("read path-backed CZI part");
    assert_eq!(second.bytes(), &[11, 12, 13, 14, 15, 16]);
}

#[test]
fn path_backed_tiff_snapshot_preserves_source_identity() {
    let fixture = TemporaryFile::new("snapshot.tif", &basic_tiff());
    let mut reader = TiffReader::new();
    reader.set_id(&fixture.path).expect("open snapshot TIFF");
    let expected = reader.used_sources();
    assert_eq!(expected.len(), 1);

    let restored = reader
        .snapshot()
        .expect("snapshot path TIFF")
        .into_reader()
        .expect("restore path TIFF snapshot");
    assert_eq!(restored.used_sources(), expected);
}

#[test]
fn mrc_reads_pixels_from_an_application_owned_source() {
    let source = Arc::new(MemorySource::new(
        "memory:mrc",
        "volume.mrc",
        minimal_mrc([4, 5, 6, 1, 2, 3]),
    ));
    let dataset = open_source(SourceInput::new(source)).expect("open in-memory MRC");

    assert!(dataset.used_files().is_empty());
    assert_eq!(dataset.used_sources()[0].identity().as_str(), "memory:mrc");
    let plane = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect("read in-memory MRC plane");
    assert_eq!(plane.bytes(), &[1, 2, 3, 4, 5, 6]);
}

#[test]
fn dcimg_reads_pixels_from_an_application_owned_source() {
    let source = Arc::new(MemorySource::new(
        "memory:dcimg",
        "camera.dcimg",
        minimal_dcimg([1, 2, 3, 4, 5, 6]),
    ));
    let dataset = open_source(SourceInput::new(source)).expect("open in-memory DCIMG");

    assert!(dataset.used_files().is_empty());
    assert_eq!(
        dataset.used_sources()[0].identity().as_str(),
        "memory:dcimg"
    );
    let plane = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect("read in-memory DCIMG plane");
    assert_eq!(plane.bytes(), &[4, 5, 6, 1, 2, 3]);
}

#[test]
fn nd2_reads_pixels_from_an_application_owned_source() {
    let source = Arc::new(MemorySource::new(
        "memory:nd2",
        "acquisition.nd2",
        minimal_nd2([1, 2, 3, 4, 5, 6]),
    ));
    let dataset = open_source(SourceInput::new(source)).expect("open in-memory ND2");

    assert!(dataset.used_files().is_empty());
    assert_eq!(dataset.used_sources()[0].identity().as_str(), "memory:nd2");
    let plane = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect("read in-memory ND2 plane");
    assert_eq!(plane.bytes(), &[1, 2, 3, 4, 5, 6]);
}

#[test]
fn nd2_propagates_post_header_source_failures() {
    let source = Arc::new(FailAtOrAfterRangeSource {
        inner: MemorySource::new(
            "memory:nd2-failing-range",
            "failing.nd2",
            minimal_nd2([1, 2, 3, 4, 5, 6]),
        ),
        first_failing_offset: 1,
    });
    let error = open_source(SourceInput::new(source))
        .err()
        .expect("ND2 metadata range failure must be returned");
    assert!(
        matches!(error, BioFormatsError::SourceRead { .. }),
        "unexpected error: {error:?}"
    );
}

fn basic_tiff() -> Vec<u8> {
    basic_tiff_with_pixels([1, 2, 3, 4, 5, 6])
}

fn basic_tiff_with_pixels(pixels: [u8; 6]) -> Vec<u8> {
    let width = 3_u32;
    let height = 2_u32;
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
    bytes
}

fn padded_tiff(pixel_offset: usize) -> Vec<u8> {
    let width = 3_u32;
    let height = 2_u32;
    let pixels = [1_u8, 2, 3, 4, 5, 6];
    let ifd_offset = 8_u32;
    let tag_count = 9_u16;

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
    bytes.resize(pixel_offset, 0);
    bytes.extend_from_slice(&pixels);
    bytes
}

fn minimal_czi(z: i32, pixels: [u8; 6]) -> Vec<u8> {
    const SEGMENT_HEADER: usize = 32;
    const FILE_HEADER_BODY: usize = 80;
    const DIRECTORY_HEADER: usize = 128;
    const DIMENSION_COUNT: usize = 5;
    const ENTRY_SIZE: usize = 32 + DIMENSION_COUNT * 20;
    const SUBBLOCK_HEADER: usize = 256;

    let directory_position = SEGMENT_HEADER + FILE_HEADER_BODY;
    let directory_used = DIRECTORY_HEADER + ENTRY_SIZE;
    let subblock_position = directory_position + SEGMENT_HEADER + directory_used;
    let subblock_used = SUBBLOCK_HEADER + pixels.len();
    let mut bytes = vec![0_u8; subblock_position + SEGMENT_HEADER + subblock_used];

    write_czi_segment_header(&mut bytes, 0, b"ZISRAWFILE", FILE_HEADER_BODY as u64);
    bytes[SEGMENT_HEADER + 52..SEGMENT_HEADER + 60]
        .copy_from_slice(&(directory_position as u64).to_le_bytes());

    write_czi_segment_header(
        &mut bytes,
        directory_position,
        b"ZISRAWDIRECTORY",
        directory_used as u64,
    );
    let directory_body = directory_position + SEGMENT_HEADER;
    bytes[directory_body..directory_body + 4].copy_from_slice(&1_i32.to_le_bytes());
    let entry = directory_body + DIRECTORY_HEADER;
    bytes[entry + 2..entry + 6].copy_from_slice(&0_i32.to_le_bytes());
    bytes[entry + 6..entry + 14].copy_from_slice(&(subblock_position as i64).to_le_bytes());
    bytes[entry + 18..entry + 22].copy_from_slice(&0_i32.to_le_bytes());
    bytes[entry + 28..entry + 32].copy_from_slice(&(DIMENSION_COUNT as i32).to_le_bytes());
    for (index, (name, start, size)) in [
        (b"X\0\0\0", 0_i32, 3_i32),
        (b"Y\0\0\0", 0, 2),
        (b"Z\0\0\0", z, 1),
        (b"C\0\0\0", 0, 1),
        (b"T\0\0\0", 0, 1),
    ]
    .into_iter()
    .enumerate()
    {
        let dimension = entry + 32 + index * 20;
        bytes[dimension..dimension + 4].copy_from_slice(name);
        bytes[dimension + 4..dimension + 8].copy_from_slice(&start.to_le_bytes());
        bytes[dimension + 8..dimension + 12].copy_from_slice(&size.to_le_bytes());
        bytes[dimension + 16..dimension + 20].copy_from_slice(&size.to_le_bytes());
    }

    write_czi_segment_header(
        &mut bytes,
        subblock_position,
        b"ZISRAWSUBBLOCK",
        subblock_used as u64,
    );
    let subblock_body = subblock_position + SEGMENT_HEADER;
    bytes[subblock_body + 8..subblock_body + 16]
        .copy_from_slice(&(pixels.len() as u64).to_le_bytes());
    bytes[subblock_body + SUBBLOCK_HEADER..subblock_body + SUBBLOCK_HEADER + pixels.len()]
        .copy_from_slice(&pixels);
    bytes
}

fn minimal_mrc(pixels: [u8; 6]) -> Vec<u8> {
    let mut bytes = vec![0_u8; 1024];
    put_i32_le(&mut bytes, 0, 3);
    put_i32_le(&mut bytes, 4, 2);
    put_i32_le(&mut bytes, 8, 1);
    put_i32_le(&mut bytes, 12, 0);
    put_i32_le(&mut bytes, 28, 3);
    put_i32_le(&mut bytes, 32, 2);
    put_i32_le(&mut bytes, 36, 1);
    put_f32_le(&mut bytes, 40, 30.0);
    put_f32_le(&mut bytes, 44, 20.0);
    put_f32_le(&mut bytes, 48, 10.0);
    put_f32_le(&mut bytes, 52, 90.0);
    put_f32_le(&mut bytes, 56, 90.0);
    put_f32_le(&mut bytes, 60, 90.0);
    put_f32_le(&mut bytes, 76, 0.0);
    put_f32_le(&mut bytes, 80, 255.0);
    put_f32_le(&mut bytes, 84, 127.5);
    put_i32_le(&mut bytes, 88, 1);
    bytes[104..108].copy_from_slice(b"MRC ");
    bytes[160..162].copy_from_slice(&1_i16.to_le_bytes());
    bytes[208..212].copy_from_slice(b"MAP ");
    bytes[212] = 68;
    put_i32_le(&mut bytes, 220, 1);
    bytes[224..228].copy_from_slice(b"test");
    bytes.extend_from_slice(&pixels);
    bytes
}

fn minimal_dcimg(pixels: [u8; 6]) -> Vec<u8> {
    let header_size = 128_usize;
    let data_offset = 128_usize;
    let mut bytes = vec![0_u8; header_size + data_offset];
    bytes[..5].copy_from_slice(b"DCIMG");
    put_u32_le(&mut bytes, 8, 0x0100_0000);
    put_u32_le(&mut bytes, 40, header_size as u32);
    put_i32_le(&mut bytes, header_size + 60, 1);
    put_i32_le(&mut bytes, header_size + 64, 1);
    put_i32_le(&mut bytes, header_size + 72, 3);
    put_i32_le(&mut bytes, header_size + 76, 2);
    put_u32_le(&mut bytes, header_size + 84, pixels.len() as u32);
    put_i64_le(&mut bytes, header_size + 96, data_offset as i64);
    put_u32_le(&mut bytes, header_size + 124, 0);
    bytes.extend_from_slice(&pixels);
    let file_len = bytes.len() as u32;
    put_u32_le(&mut bytes, 48, file_len);
    put_u32_le(&mut bytes, 64, file_len);
    bytes
}

fn minimal_nd2(pixels: [u8; 6]) -> Vec<u8> {
    let attributes = br#"<variant>
  <uiWidth value="3"/><uiHeight value="2"/><uiWidthBytes value="3"/>
  <uiComp value="1"/><uiBpcInMemory value="8"/><uiBpcSignificant value="8"/>
</variant>"#;
    let mut image = vec![0_u8; 8];
    image.extend_from_slice(&pixels);

    let mut bytes = Vec::new();
    push_nd2_chunk(&mut bytes, "ImageAttributes!", attributes);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &image);
    bytes
}

fn push_nd2_chunk(bytes: &mut Vec<u8>, name: &str, payload: &[u8]) {
    bytes.extend_from_slice(&[0xda, 0xce, 0xbe, 0x0a]);
    bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(payload);
}

fn put_i32_le(bytes: &mut [u8], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_i64_le(bytes: &mut [u8], offset: usize, value: i64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_f32_le(bytes: &mut [u8], offset: usize, value: f32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_czi_segment_header(bytes: &mut [u8], offset: usize, kind: &[u8], used: u64) {
    bytes[offset..offset + kind.len()].copy_from_slice(kind);
    bytes[offset + 16..offset + 24].copy_from_slice(&used.to_le_bytes());
    bytes[offset + 24..offset + 32].copy_from_slice(&used.to_le_bytes());
}

struct SplitCziFixture {
    directory: std::path::PathBuf,
    master: std::path::PathBuf,
    part: std::path::PathBuf,
}

struct TemporaryFile {
    path: std::path::PathBuf,
}

impl TemporaryFile {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bioformats-rs-{}-{unique}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, bytes).expect("write temporary source fixture");
        Self { path }
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl SplitCziFixture {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "bioformats-rs-split-czi-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create split CZI directory");
        let master = directory.join("sample.czi");
        let part = directory.join("sample (1).czi");
        std::fs::write(&master, minimal_czi(0, [1, 2, 3, 4, 5, 6])).expect("write CZI master");
        std::fs::write(&part, minimal_czi(1, [11, 12, 13, 14, 15, 16])).expect("write CZI part");
        Self {
            directory,
            master,
            part,
        }
    }
}

impl Drop for SplitCziFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.master);
        let _ = std::fs::remove_file(&self.part);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

fn push_tag(bytes: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&field_type.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&value.to_le_bytes());
}
