use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats_rs::{
    open, open_source, BioFormatsError, FormatReader, PixelType, PlaneCoordinates,
    RandomAccessSource, ReadRequest, Rect, Region, SourceId, SourceInfo, SourceInput, SourceResult,
    TiffReader,
};

struct MemorySource {
    info: SourceInfo,
    bytes: Arc<[u8]>,
    reads: Mutex<Vec<(u64, usize)>>,
}

impl MemorySource {
    fn new(identity: &str, name: &str, bytes: Vec<u8>) -> Self {
        let bytes: Arc<[u8]> = bytes.into();
        Self {
            info: SourceInfo::new(SourceId::new(identity), name, bytes.len() as u64),
            bytes,
            reads: Mutex::new(Vec::new()),
        }
    }

    fn reads(&self) -> Vec<(u64, usize)> {
        self.reads.lock().expect("memory-source reads").clone()
    }
}

impl RandomAccessSource for MemorySource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        self.reads
            .lock()
            .expect("record memory-source read")
            .push((offset, destination.len()));
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

struct TempTiff {
    path: PathBuf,
}

impl TempTiff {
    fn write(name: &str, bytes: &[u8]) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bioformats_rs_{name}_{unique}.tif"));
        fs::write(&path, bytes).expect("write generated TIFF fixture");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTiff {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn dataset_expands_one_bit_strips_to_uint8_scalars() {
    let fixture = TempTiff::write("packed_one_bit_strips", &one_bit_stripped_tiff());
    let dataset = open(fixture.path()).expect("open packed TIFF dataset");
    let metadata = dataset.series()[0].resolutions()[0].metadata();

    assert_eq!(metadata.pixel_type, PixelType::Uint8);
    assert_eq!(metadata.bits_per_pixel, 1);

    let full_request = ReadRequest::new(0, PlaneCoordinates::default());
    let info = dataset.plane_info(full_request).expect("packed plane info");
    assert_eq!(info.layout.pixel_type, PixelType::Uint8);
    assert_eq!(info.layout.significant_bits, 1);
    assert_eq!(info.byte_len, 30);

    let expected_full = [
        0, 1, 0, 1, 1, 0, 1, 0, 1, 1, // row 0
        1, 0, 1, 0, 0, 1, 0, 1, 0, 0, // row 1
        1, 1, 1, 1, 0, 0, 0, 0, 1, 0, // row 2
    ];
    assert_eq!(
        dataset
            .read_plane(full_request)
            .expect("read unpacked full plane")
            .bytes(),
        expected_full
    );

    let region_request = full_request.with_region(Region::Rect(
        Rect::new(2, 1, 6, 2).expect("valid packed region"),
    ));
    assert_eq!(
        dataset
            .read_plane(region_request)
            .expect("read unpacked interior region")
            .bytes(),
        [1, 0, 0, 1, 0, 1, 1, 1, 0, 0, 0, 0]
    );
}

#[test]
fn ome_bit_type_uses_the_same_unscaled_uint8_representation() {
    let fixture = TempTiff::write("packed_ome_bit", &one_bit_ome_tiff());
    let dataset = open(fixture.path()).expect("open packed OME-TIFF dataset");
    let metadata = dataset.series()[0].resolutions()[0].metadata();
    assert_eq!(metadata.pixel_type, PixelType::Uint8);
    assert_eq!(metadata.bits_per_pixel, 1);

    let request = ReadRequest::new(0, PlaneCoordinates::default());
    let info = dataset.plane_info(request).expect("packed OME plane info");
    assert_eq!(info.layout.pixel_type, PixelType::Uint8);
    assert_eq!(info.layout.significant_bits, 1);
    assert_eq!(
        dataset
            .read_plane(request)
            .expect("read packed OME plane")
            .bytes(),
        [0, 1, 0, 1, 1, 0, 1, 0, 1, 1]
    );
    assert_eq!(
        dataset
            .read_plane(request.with_region(Region::Rect(
                Rect::new(3, 0, 5, 1).expect("valid packed OME region"),
            )))
            .expect("read packed OME region")
            .bytes(),
        [1, 1, 0, 1, 0]
    );

    const BITS_PER_SAMPLE_VALUE: usize = 10 + 2 * 12 + 8;
    for bits in [2_u32, 8] {
        let mut mismatched = one_bit_ome_tiff();
        write_u32_le(&mut mismatched, BITS_PER_SAMPLE_VALUE, bits);
        let fixture = TempTiff::write(&format!("ome_bit_with_{bits}_bit_ifd"), &mismatched);
        assert!(matches!(
            open(fixture.path()),
            Err(BioFormatsError::Format(message))
                if message.contains("OME pixel type bit requires TIFF BitsPerSample 1")
        ));
    }
}

#[test]
fn dataset_expands_one_bit_tiles_and_crops_across_tile_edges() {
    let fixture = TempTiff::write("packed_one_bit_tiles", &one_bit_tiled_tiff());
    let dataset = open(fixture.path()).expect("open packed tiled TIFF dataset");
    let request = ReadRequest::new(0, PlaneCoordinates::default());

    assert_eq!(
        dataset
            .read_plane(request)
            .expect("read unpacked tiled plane")
            .bytes(),
        [
            0, 1, 0, 1, 1, 0, 1, 0, 1, 1, // row 0
            1, 0, 1, 0, 0, 1, 0, 1, 0, 0, // row 1
            1, 1, 1, 1, 0, 0, 0, 0, 1, 0, // row 2
        ]
    );

    let region = request.with_region(Region::Rect(
        Rect::new(6, 1, 4, 2).expect("valid cross-tile region"),
    ));
    assert_eq!(
        dataset
            .read_plane(region)
            .expect("read unpacked cross-tile region")
            .bytes(),
        [0, 1, 0, 0, 0, 0, 1, 0]
    );
}

#[test]
fn dataset_expands_twelve_bit_strips_into_declared_endian_uint16() {
    let values = [0x001_u16, 0xabc, 0x123, 0xfff, 0x800, 0x456];
    for (name, little_endian) in [
        ("packed_twelve_bit_le", true),
        ("packed_twelve_bit_be", false),
    ] {
        let fixture = TempTiff::write(name, &twelve_bit_stripped_tiff(little_endian));
        let dataset = open(fixture.path()).expect("open twelve-bit TIFF dataset");
        let metadata = dataset.series()[0].resolutions()[0].metadata();

        assert_eq!(metadata.pixel_type, PixelType::Uint16);
        assert_eq!(metadata.bits_per_pixel, 12);
        assert_eq!(metadata.is_little_endian, little_endian);

        let request = ReadRequest::new(0, PlaneCoordinates::default());
        let info = dataset.plane_info(request).expect("twelve-bit plane info");
        assert_eq!(info.layout.pixel_type, PixelType::Uint16);
        assert_eq!(info.layout.significant_bits, 12);
        assert_eq!(info.layout.little_endian, little_endian);
        assert_eq!(info.byte_len, 12);

        let expected = values
            .into_iter()
            .flat_map(|value| {
                if little_endian {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            dataset
                .read_plane(request)
                .expect("read expanded twelve-bit plane")
                .bytes(),
            expected
        );

        let region_request = request.with_region(Region::Rect(
            Rect::new(1, 0, 2, 2).expect("valid twelve-bit region"),
        ));
        let expected_region = [0xabc_u16, 0x123, 0x800, 0x456]
            .into_iter()
            .flat_map(|value| {
                if little_endian {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            dataset
                .read_plane(region_request)
                .expect("read expanded twelve-bit region")
                .bytes(),
            expected_region
        );

        let mut destination = vec![0xa5; info.byte_len + 5];
        dataset
            .read_plane_into(request, &mut destination)
            .expect("read twelve-bit plane into caller buffer");
        assert_eq!(&destination[..info.byte_len], expected);
        assert_eq!(&destination[info.byte_len..], &[0xa5; 5]);
    }
}

#[test]
fn every_advertised_unsigned_packed_width_has_literal_scalar_parity() {
    let cases: &[(u16, &[u8], &[u16])] = &[
        (1, &[0x50], &[0, 1, 0, 1]),
        (2, &[0x1b], &[0, 1, 2, 3]),
        (3, &[0x07, 0x70], &[0, 1, 6, 7]),
        (4, &[0x01, 0xef], &[0, 1, 14, 15]),
        (5, &[0x00, 0x7d, 0xf0], &[0, 1, 30, 31]),
        (6, &[0x00, 0x1f, 0xbf], &[0, 1, 62, 63]),
        (7, &[0x01, 0x57, 0xf8], &[0, 0x55, 0x7f]),
        (9, &[0x00, 0xc0, 0x3f, 0xe0], &[1, 0x100, 0x1ff]),
        (10, &[0x00, 0x60, 0x0f, 0xfc], &[1, 0x200, 0x3ff]),
        (11, &[0x00, 0x30, 0x03, 0xff, 0x80], &[1, 0x400, 0x7ff]),
        (12, &[0x00, 0x18, 0x00, 0xff, 0xf0], &[1, 0x800, 0xfff]),
        (13, &[0x00, 0x0c, 0x00, 0x3f, 0xfe], &[1, 0x1000, 0x1fff]),
        (
            14,
            &[0x00, 0x06, 0x00, 0x0f, 0xff, 0xc0],
            &[1, 0x2000, 0x3fff],
        ),
        (
            15,
            &[0x00, 0x03, 0x00, 0x03, 0xff, 0xf8],
            &[1, 0x4000, 0x7fff],
        ),
    ];

    for &(bits, packed, values) in cases {
        let fixture = TempTiff::write(
            &format!("packed_{bits}_bit_literal"),
            &single_row_packed_tiff(bits, values.len() as u32, packed),
        );
        let dataset = open(fixture.path()).expect("open literal packed-width TIFF");
        let request = ReadRequest::new(0, PlaneCoordinates::default());
        let info = dataset
            .plane_info(request)
            .expect("literal packed-width plane info");
        assert_eq!(info.layout.significant_bits, bits as u8);

        let expected = if bits <= 7 {
            values.iter().map(|value| *value as u8).collect::<Vec<_>>()
        } else {
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            dataset
                .read_plane(request)
                .expect("read literal packed-width plane")
                .bytes(),
            expected,
            "{bits}-bit scalar mismatch"
        );
    }
}

#[test]
fn application_owned_source_reads_packed_regions_and_preserves_buffer_suffix() {
    let (bytes, first_strip_offset, second_strip_offset) =
        one_bit_stripped_tiff_with_gap(16 * 1024);
    let source = Arc::new(MemorySource::new(
        "memory:packed-one-bit",
        "packed-one-bit.tif",
        bytes,
    ));
    let dataset =
        open_source(SourceInput::new(source.clone())).expect("open packed application source");
    assert!(dataset.used_files().is_empty());
    assert_eq!(dataset.used_sources().len(), 1);
    assert_eq!(
        dataset.used_sources()[0].identity(),
        &SourceId::new("memory:packed-one-bit")
    );
    let reads_after_open = source.reads();
    assert!(reads_after_open.iter().all(|(offset, length)| {
        !ranges_overlap(
            *offset,
            *length,
            first_strip_offset,
            second_strip_offset + 2,
        )
    }));

    let request = ReadRequest::new(0, PlaneCoordinates::default()).with_region(Region::Rect(
        Rect::new(2, 2, 6, 1).expect("valid packed source region"),
    ));
    let info = dataset
        .plane_info(request)
        .expect("packed source plane info");
    let mut destination = vec![0x5a; info.byte_len + 7];
    dataset
        .read_plane_into(request, &mut destination)
        .expect("read packed source into caller buffer");

    assert_eq!(&destination[..info.byte_len], &[1, 1, 0, 0, 0, 0]);
    assert_eq!(&destination[info.byte_len..], &[0x5a; 7]);

    let reads_after_plane = source.reads();
    let pixel_reads = &reads_after_plane[reads_after_open.len()..];
    assert!(pixel_reads
        .iter()
        .any(|(offset, length)| *offset == second_strip_offset && *length == 2));
    assert!(pixel_reads.iter().all(|(offset, length)| {
        !ranges_overlap(*offset, *length, first_strip_offset, first_strip_offset + 4)
    }));
}

#[test]
fn packed_deflate_output_is_unpacked_and_bounded() {
    let fixture = TempTiff::write(
        "packed_one_bit_deflate",
        &one_bit_deflate_tiff(&[0x5a, 0xc0, 0xa5, 0x00]),
    );
    let dataset = open(fixture.path()).expect("open packed Deflate TIFF");
    let request = ReadRequest::new(0, PlaneCoordinates::default());
    assert_eq!(
        dataset
            .read_plane(request)
            .expect("read packed Deflate plane")
            .bytes(),
        [
            0, 1, 0, 1, 1, 0, 1, 0, 1, 1, // row 0
            1, 0, 1, 0, 0, 1, 0, 1, 0, 0, // row 1
        ]
    );

    let oversized = TempTiff::write(
        "packed_one_bit_deflate_oversized",
        &one_bit_deflate_tiff(&[0x5a, 0xc0, 0xa5, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff]),
    );
    let oversized_dataset = open(oversized.path()).expect("open oversized packed Deflate TIFF");
    assert!(matches!(
        oversized_dataset.read_plane(request),
        Err(BioFormatsError::Codec(message)) if message.contains("exceeds")
    ));
}

#[test]
fn malformed_packed_tables_counts_offsets_and_metadata_are_structured_errors() {
    const TAGS_START: usize = 10;
    const IFD_SIZE: usize = 2 + 9 * 12 + 4;
    const OFFSETS_OFFSET: usize = 8 + IFD_SIZE;
    const COUNTS_OFFSET: usize = OFFSETS_OFFSET + 2 * 4;
    const PIXELS_OFFSET: u32 = (COUNTS_OFFSET + 2 * 4) as u32;

    let mut short_count = one_bit_stripped_tiff();
    write_u32_le(&mut short_count, COUNTS_OFFSET + 4, 1);
    let short_fixture = TempTiff::write("packed_short_count", &short_count);
    let short_dataset = open(short_fixture.path()).expect("open short packed strip count");
    let short_error = short_dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect_err("short packed strip must fail");
    assert!(
        matches!(
            &short_error,
            BioFormatsError::InvalidData(message) if message.contains("strip 1") && message.contains("expected between 2 and 4")
        ),
        "unexpected short-count error: {short_error:?}"
    );

    let mut oversized_count = one_bit_stripped_tiff();
    write_u32_le(&mut oversized_count, COUNTS_OFFSET, 5);
    let oversized_fixture = TempTiff::write("packed_oversized_count", &oversized_count);
    let oversized_dataset = open(oversized_fixture.path()).expect("open oversized packed count");
    assert!(matches!(
        oversized_dataset.read_plane(ReadRequest::new(0, PlaneCoordinates::default())),
        Err(BioFormatsError::InvalidData(message)) if message.contains("strip 0") && message.contains("expected between 4 and 4")
    ));

    let mut missing_offset = one_bit_stripped_tiff();
    let strip_offsets_entry = TAGS_START + 5 * 12;
    write_u32_le(&mut missing_offset, strip_offsets_entry + 4, 1);
    write_u32_le(&mut missing_offset, strip_offsets_entry + 8, PIXELS_OFFSET);
    let missing_fixture = TempTiff::write("packed_missing_offset", &missing_offset);
    let missing_dataset = open(missing_fixture.path()).expect("open missing packed offset");
    assert!(matches!(
        missing_dataset.read_plane(ReadRequest::new(0, PlaneCoordinates::default())),
        Err(BioFormatsError::InvalidData(message)) if message.contains("strip offset 1 is missing")
    ));

    let mut out_of_range_offset = one_bit_stripped_tiff();
    let invalid_offset = out_of_range_offset.len() as u32 + 32;
    write_u32_le(&mut out_of_range_offset, OFFSETS_OFFSET + 4, invalid_offset);
    let range_fixture = TempTiff::write("packed_out_of_range_offset", &out_of_range_offset);
    let range_dataset = open(range_fixture.path()).expect("open out-of-range packed offset");
    let range_error = range_dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect_err("out-of-range packed offset must fail");
    assert!(
        matches!(
            &range_error,
            BioFormatsError::SourceRangeOutOfBounds { .. }
                | BioFormatsError::SourceRead { .. }
                | BioFormatsError::InvalidData(_)
        ),
        "unexpected out-of-range offset error: {range_error:?}"
    );

    let mut zero_bits = one_bit_stripped_tiff();
    let bits_per_sample_entry = TAGS_START + 2 * 12;
    write_u32_le(&mut zero_bits, bits_per_sample_entry + 8, 0);
    let zero_fixture = TempTiff::write("packed_zero_bits", &zero_bits);
    assert!(matches!(
        open(zero_fixture.path()),
        Err(BioFormatsError::UnsupportedFormat(message)) if message.contains("0-bit samples")
    ));

    for (name, tag, value, expected) in [
        ("packed_fill_order", 266_u16, 2_u32, "FillOrder 2"),
        ("packed_predictor", 317, 2, "Predictor 2"),
        ("packed_signed", 339, 2, "SampleFormat 2"),
    ] {
        let fixture = TempTiff::write(name, &one_bit_stripped_tiff_with_extra(tag, value));
        assert!(matches!(
            open(fixture.path()),
            Err(BioFormatsError::UnsupportedFormat(message)) if message.contains(expected)
        ));
    }

    let mut white_is_zero = one_bit_stripped_tiff();
    let photometric_entry = TAGS_START + 4 * 12;
    write_u32_le(&mut white_is_zero, photometric_entry + 8, 0);
    let white_fixture = TempTiff::write("packed_white_is_zero", &white_is_zero);
    assert!(matches!(
        open(white_fixture.path()),
        Err(BioFormatsError::UnsupportedFormat(message)) if message.contains("MinIsWhite")
    ));
}

#[test]
fn malformed_packed_tile_tables_and_edge_counts_are_structured_errors() {
    const TAGS_START: usize = 10;
    const TILE_OFFSETS_ENTRY: usize = TAGS_START + 8 * 12;
    const TILE_BYTE_COUNTS_ENTRY: usize = TAGS_START + 9 * 12;
    const IFD_SIZE: usize = 2 + 10 * 12 + 4;
    const OFFSETS_OFFSET: usize = 8 + IFD_SIZE;
    const COUNTS_OFFSET: usize = OFFSETS_OFFSET + 4 * 4;

    let request = ReadRequest::new(0, PlaneCoordinates::default());
    let edge_request = request.with_region(Region::Rect(
        Rect::new(8, 2, 2, 1).expect("valid edge-tile region"),
    ));

    let mut missing_offset = one_bit_tiled_tiff();
    write_u32_le(&mut missing_offset, TILE_OFFSETS_ENTRY + 4, 3);
    let fixture = TempTiff::write("packed_tile_missing_offset", &missing_offset);
    let dataset = open(fixture.path()).expect("open packed tile missing an offset");
    assert!(matches!(
        dataset.read_plane(edge_request),
        Err(BioFormatsError::InvalidData(message)) if message.contains("tile offset 3 is missing")
    ));

    let mut missing_count = one_bit_tiled_tiff();
    write_u32_le(&mut missing_count, TILE_BYTE_COUNTS_ENTRY + 4, 3);
    let fixture = TempTiff::write("packed_tile_missing_count", &missing_count);
    let dataset = open(fixture.path()).expect("open packed tile missing a byte count");
    assert!(matches!(
        dataset.read_plane(edge_request),
        Err(BioFormatsError::InvalidData(message)) if message.contains("tile byte count 3 is missing")
    ));

    let mut short_edge_count = one_bit_tiled_tiff();
    write_u32_le(&mut short_edge_count, COUNTS_OFFSET + 3 * 4, 0);
    let fixture = TempTiff::write("packed_tile_short_edge_count", &short_edge_count);
    let dataset = open(fixture.path()).expect("open short packed edge tile");
    assert!(matches!(
        dataset.read_plane(edge_request),
        Err(BioFormatsError::InvalidData(message)) if message.contains("packed tile 3") && message.contains("at least 1")
    ));

    let mut oversized_count = one_bit_tiled_tiff();
    write_u32_le(&mut oversized_count, COUNTS_OFFSET, 3);
    let fixture = TempTiff::write("packed_tile_oversized_count", &oversized_count);
    let dataset = open(fixture.path()).expect("open oversized packed tile");
    assert!(matches!(
        dataset.read_plane(request),
        Err(BioFormatsError::InvalidData(message)) if message.contains("tile 0") && message.contains("maximum is 2")
    ));

    let mut out_of_range_offset = one_bit_tiled_tiff();
    let invalid_offset = out_of_range_offset.len() as u32 + 32;
    write_u32_le(
        &mut out_of_range_offset,
        OFFSETS_OFFSET + 3 * 4,
        invalid_offset,
    );
    let fixture = TempTiff::write("packed_tile_out_of_range_offset", &out_of_range_offset);
    let dataset = open(fixture.path()).expect("open out-of-range packed edge tile");
    assert!(matches!(
        dataset.read_plane(edge_request),
        Err(BioFormatsError::SourceRangeOutOfBounds { .. })
            | Err(BioFormatsError::SourceRead { .. })
            | Err(BioFormatsError::InvalidData(_))
    ));
}

#[test]
fn unimplemented_packed_codec_and_photometric_combinations_fail_at_open() {
    const TAGS_START: usize = 10;
    const BITS_PER_SAMPLE_ENTRY: usize = TAGS_START + 2 * 12;
    const COMPRESSION_ENTRY: usize = TAGS_START + 3 * 12;
    const PHOTOMETRIC_ENTRY: usize = TAGS_START + 4 * 12;

    let mut packed_jpeg = one_bit_stripped_tiff();
    write_u32_le(&mut packed_jpeg, COMPRESSION_ENTRY + 8, 7);
    let fixture = TempTiff::write("packed_jpeg_unsupported", &packed_jpeg);
    assert!(matches!(
        open(fixture.path()),
        Err(BioFormatsError::UnsupportedFormat(message))
            if message.contains("JPEG-compressed 1-bit packed samples")
    ));

    let mut wide_packed = one_bit_stripped_tiff();
    write_u32_le(&mut wide_packed, BITS_PER_SAMPLE_ENTRY + 8, 17);
    let fixture = TempTiff::write("packed_seventeen_bit_unsupported", &wide_packed);
    assert!(matches!(
        open(fixture.path()),
        Err(BioFormatsError::UnsupportedFormat(message)) if message.contains("17-bit samples")
    ));

    for (name, value, expected) in [
        ("packed_cmyk_unsupported", 5, "Cmyk"),
        ("packed_ycbcr_unsupported", 6, "YCbCr"),
    ] {
        let mut bytes = one_bit_stripped_tiff();
        write_u32_le(&mut bytes, PHOTOMETRIC_ENTRY + 8, value);
        let fixture = TempTiff::write(name, &bytes);
        assert!(matches!(
            open(fixture.path()),
            Err(BioFormatsError::UnsupportedFormat(message)) if message.contains(expected)
        ));
    }

    let fixture = TempTiff::write("floating_predictor_unsupported", &floating_predictor_tiff());
    assert!(matches!(
        open(fixture.path()),
        Err(BioFormatsError::UnsupportedFormat(message)) if message.contains("Predictor 3")
    ));
}

#[test]
fn packed_tiff_detection_accepts_the_header_and_rejects_non_tiff_bytes() {
    let reader = TiffReader::new();
    let fixture = one_bit_stripped_tiff();
    assert!(reader.is_this_type_by_bytes(&fixture[..8]));
    assert!(!reader.is_this_type_by_bytes(b"not a TIFF file"));
}

#[test]
fn packed_multisample_strips_preserve_chunky_and_planar_layouts() {
    let chunky = TempTiff::write("packed_chunky_rgb", &four_bit_chunky_rgb_tiff());
    let chunky_dataset = open(chunky.path()).expect("open packed chunky RGB TIFF");
    let request = ReadRequest::new(0, PlaneCoordinates::default());
    let chunky_info = chunky_dataset
        .plane_info(request)
        .expect("packed chunky plane info");
    assert_eq!(chunky_info.layout.pixel_type, PixelType::Uint8);
    assert_eq!(chunky_info.layout.significant_bits, 4);
    assert_eq!(chunky_info.layout.samples_per_pixel, 3);
    assert!(chunky_info.layout.interleaved);
    assert_eq!(
        chunky_dataset
            .read_plane(request)
            .expect("read packed chunky plane")
            .bytes(),
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0, 1, 2]
    );
    assert_eq!(
        chunky_dataset
            .read_plane(request.with_region(Region::Rect(
                Rect::new(1, 0, 2, 2).expect("valid packed chunky region"),
            )))
            .expect("read packed chunky region")
            .bytes(),
        [4, 5, 6, 7, 8, 9, 13, 14, 15, 0, 1, 2]
    );

    let planar = TempTiff::write("packed_planar", &four_bit_planar_tiff());
    let planar_dataset = open(planar.path()).expect("open packed planar TIFF");
    let planar_info = planar_dataset
        .plane_info(request)
        .expect("packed planar plane info");
    assert_eq!(planar_info.layout.samples_per_pixel, 2);
    assert!(!planar_info.layout.interleaved);
    assert_eq!(
        planar_dataset
            .read_plane(request)
            .expect("read packed planar plane")
            .bytes(),
        [1, 2, 3, 4, 5, 6, 10, 11, 12, 13, 14, 15]
    );
    assert_eq!(
        planar_dataset
            .read_plane(request.with_region(Region::Rect(
                Rect::new(1, 0, 2, 2).expect("valid packed planar region"),
            )))
            .expect("read packed planar region")
            .bytes(),
        [2, 3, 5, 6, 11, 12, 14, 15]
    );
}

fn one_bit_stripped_tiff() -> Vec<u8> {
    one_bit_stripped_tiff_with_optional_extra(None)
}

fn one_bit_ome_tiff() -> Vec<u8> {
    const WIDTH: u32 = 10;
    const HEIGHT: u32 = 1;
    const TAG_COUNT: u16 = 10;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    let ome_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="bit" SizeX="10" SizeY="1" SizeZ="1" SizeC="1" SizeT="1">
      <Channel ID="Channel:0:0" SamplesPerPixel="1"/>
      <TiffData IFD="0" PlaneCount="1"/>
    </Pixels>
  </Image>
</OME>"#;
    let description_offset = IFD_OFFSET + IFD_SIZE;
    let pixels_offset = description_offset + ome_xml.len() as u32 + 1;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, WIDTH);
    push_tag(&mut bytes, 257, 4, 1, HEIGHT);
    push_tag(&mut bytes, 258, 3, 1, 1);
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(
        &mut bytes,
        270,
        2,
        ome_xml.len() as u32 + 1,
        description_offset,
    );
    push_tag(&mut bytes, 273, 4, 1, pixels_offset);
    push_tag(&mut bytes, 277, 3, 1, 1);
    push_tag(&mut bytes, 278, 4, 1, HEIGHT);
    push_tag(&mut bytes, 279, 4, 1, 2);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(ome_xml.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&[0x5a, 0xc0]);
    bytes
}

fn one_bit_stripped_tiff_with_gap(gap: usize) -> (Vec<u8>, u64, u64) {
    const IFD_SIZE: usize = 2 + 9 * 12 + 4;
    const OFFSETS_OFFSET: usize = 8 + IFD_SIZE;
    const COUNTS_OFFSET: usize = OFFSETS_OFFSET + 2 * 4;
    const PIXELS_OFFSET: usize = COUNTS_OFFSET + 2 * 4;

    let mut bytes = one_bit_stripped_tiff();
    bytes.splice(PIXELS_OFFSET..PIXELS_OFFSET, vec![0; gap]);
    let first_strip_offset = PIXELS_OFFSET
        .checked_add(gap)
        .expect("gapped packed fixture offset");
    let second_strip_offset = first_strip_offset + 4;
    write_u32_le(
        &mut bytes,
        OFFSETS_OFFSET,
        u32::try_from(first_strip_offset).expect("gapped first strip offset"),
    );
    write_u32_le(
        &mut bytes,
        OFFSETS_OFFSET + 4,
        u32::try_from(second_strip_offset).expect("gapped second strip offset"),
    );
    (bytes, first_strip_offset as u64, second_strip_offset as u64)
}

fn ranges_overlap(offset: u64, length: usize, start: u64, end: u64) -> bool {
    let read_end = offset.saturating_add(length as u64);
    offset < end && read_end > start
}

fn one_bit_stripped_tiff_with_extra(tag: u16, value: u32) -> Vec<u8> {
    one_bit_stripped_tiff_with_optional_extra(Some((tag, value)))
}

fn one_bit_stripped_tiff_with_optional_extra(extra: Option<(u16, u32)>) -> Vec<u8> {
    const WIDTH: u32 = 10;
    const HEIGHT: u32 = 3;
    const IFD_OFFSET: u32 = 8;
    let tag_count = 9_u16 + u16::from(extra.is_some());
    let ifd_size = 2 + u32::from(tag_count) * 12 + 4;
    let offsets_offset = IFD_OFFSET + ifd_size;
    let counts_offset = offsets_offset + 2 * 4;
    let pixels_offset = counts_offset + 2 * 4;

    let mut tags = vec![
        (256_u16, 4_u16, 1_u32, WIDTH),
        (257, 4, 1, HEIGHT),
        (258, 3, 1, 1),
        (259, 3, 1, 1),
        (262, 3, 1, 1),
        (273, 4, 2, offsets_offset),
        (277, 3, 1, 1),
        (278, 4, 1, 2),
        (279, 4, 2, counts_offset),
    ];
    if let Some((tag, value)) = extra {
        tags.push((tag, 3, 1, value));
        tags.sort_by_key(|entry| entry.0);
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&tag_count.to_le_bytes());
    for (tag, field_type, count, value) in tags {
        push_tag(&mut bytes, tag, field_type, count, value);
    }
    bytes.extend_from_slice(&0_u32.to_le_bytes());

    for offset in [pixels_offset, pixels_offset + 4] {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
    for count in [4_u32, 2] {
        bytes.extend_from_slice(&count.to_le_bytes());
    }

    // FillOrder 1: samples are stored most-significant bit first and each row
    // is padded independently to a whole byte.
    bytes.extend_from_slice(&[0x5a, 0xc0, 0xa5, 0x00, 0xf0, 0x80]);
    bytes
}

fn one_bit_tiled_tiff() -> Vec<u8> {
    const WIDTH: u32 = 10;
    const HEIGHT: u32 = 3;
    const TAG_COUNT: u16 = 10;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    const OFFSETS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;
    const COUNTS_OFFSET: u32 = OFFSETS_OFFSET + 4 * 4;
    const PIXELS_OFFSET: u32 = COUNTS_OFFSET + 4 * 4;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, WIDTH);
    push_tag(&mut bytes, 257, 4, 1, HEIGHT);
    push_tag(&mut bytes, 258, 3, 1, 1);
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(&mut bytes, 277, 3, 1, 1);
    push_tag(&mut bytes, 322, 4, 1, 8);
    push_tag(&mut bytes, 323, 4, 1, 2);
    push_tag(&mut bytes, 324, 4, 4, OFFSETS_OFFSET);
    push_tag(&mut bytes, 325, 4, 4, COUNTS_OFFSET);
    bytes.extend_from_slice(&0_u32.to_le_bytes());

    for offset in [
        PIXELS_OFFSET,
        PIXELS_OFFSET + 2,
        PIXELS_OFFSET + 4,
        PIXELS_OFFSET + 6,
    ] {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
    for _ in 0..4 {
        bytes.extend_from_slice(&2_u32.to_le_bytes());
    }

    // Tiles are ordered left-to-right, top-to-bottom. Each 8x2 tile row is
    // one packed byte; pixels outside the image are zero padding.
    bytes.extend_from_slice(&[0x5a, 0xa5, 0xc0, 0x00, 0xf0, 0x00, 0x80, 0x00]);
    bytes
}

fn single_row_packed_tiff(bits_per_sample: u16, width: u32, pixels: &[u8]) -> Vec<u8> {
    const TAG_COUNT: u16 = 9;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    const PIXELS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, width);
    push_tag(&mut bytes, 257, 4, 1, 1);
    push_tag(&mut bytes, 258, 3, 1, u32::from(bits_per_sample));
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(&mut bytes, 273, 4, 1, PIXELS_OFFSET);
    push_tag(&mut bytes, 277, 3, 1, 1);
    push_tag(&mut bytes, 278, 4, 1, 1);
    push_tag(&mut bytes, 279, 4, 1, pixels.len() as u32);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(pixels);
    bytes
}

fn floating_predictor_tiff() -> Vec<u8> {
    const TAG_COUNT: u16 = 11;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    const PIXELS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, 1);
    push_tag(&mut bytes, 257, 4, 1, 1);
    push_tag(&mut bytes, 258, 3, 1, 32);
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(&mut bytes, 273, 4, 1, PIXELS_OFFSET);
    push_tag(&mut bytes, 277, 3, 1, 1);
    push_tag(&mut bytes, 278, 4, 1, 1);
    push_tag(&mut bytes, 279, 4, 1, 4);
    push_tag(&mut bytes, 317, 3, 1, 3);
    push_tag(&mut bytes, 339, 3, 1, 3);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_f32.to_le_bytes());
    bytes
}

fn twelve_bit_stripped_tiff(little_endian: bool) -> Vec<u8> {
    const WIDTH: u32 = 3;
    const HEIGHT: u32 = 2;
    const TAG_COUNT: u16 = 9;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    const OFFSETS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;
    const COUNTS_OFFSET: u32 = OFFSETS_OFFSET + 2 * 4;
    const PIXELS_OFFSET: u32 = COUNTS_OFFSET + 2 * 4;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(if little_endian { b"II" } else { b"MM" });
    if little_endian {
        bytes.extend_from_slice(&42_u16.to_le_bytes());
        bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
        bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
        push_tag(&mut bytes, 256, 4, 1, WIDTH);
        push_tag(&mut bytes, 257, 4, 1, HEIGHT);
        push_tag(&mut bytes, 258, 3, 1, 12);
        push_tag(&mut bytes, 259, 3, 1, 1);
        push_tag(&mut bytes, 262, 3, 1, 1);
        push_tag(&mut bytes, 273, 4, 2, OFFSETS_OFFSET);
        push_tag(&mut bytes, 277, 3, 1, 1);
        push_tag(&mut bytes, 278, 4, 1, 1);
        push_tag(&mut bytes, 279, 4, 2, COUNTS_OFFSET);
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        for offset in [PIXELS_OFFSET, PIXELS_OFFSET + 5] {
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
        for _ in 0..2 {
            bytes.extend_from_slice(&5_u32.to_le_bytes());
        }
    } else {
        bytes.extend_from_slice(&42_u16.to_be_bytes());
        bytes.extend_from_slice(&IFD_OFFSET.to_be_bytes());
        bytes.extend_from_slice(&TAG_COUNT.to_be_bytes());
        push_tag_be(&mut bytes, 256, 4, 1, WIDTH);
        push_tag_be(&mut bytes, 257, 4, 1, HEIGHT);
        push_tag_be(&mut bytes, 258, 3, 1, 12 << 16);
        push_tag_be(&mut bytes, 259, 3, 1, 1 << 16);
        push_tag_be(&mut bytes, 262, 3, 1, 1 << 16);
        push_tag_be(&mut bytes, 273, 4, 2, OFFSETS_OFFSET);
        push_tag_be(&mut bytes, 277, 3, 1, 1 << 16);
        push_tag_be(&mut bytes, 278, 4, 1, 1);
        push_tag_be(&mut bytes, 279, 4, 2, COUNTS_OFFSET);
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        for offset in [PIXELS_OFFSET, PIXELS_OFFSET + 5] {
            bytes.extend_from_slice(&offset.to_be_bytes());
        }
        for _ in 0..2 {
            bytes.extend_from_slice(&5_u32.to_be_bytes());
        }
    }

    // Three 12-bit values per row, MSB-first, followed by four row pad bits.
    bytes.extend_from_slice(&[0x00, 0x1a, 0xbc, 0x12, 0x30]);
    bytes.extend_from_slice(&[0xff, 0xf8, 0x00, 0x45, 0x60]);
    bytes
}

fn one_bit_deflate_tiff(packed: &[u8]) -> Vec<u8> {
    const WIDTH: u32 = 10;
    const HEIGHT: u32 = 2;
    const TAG_COUNT: u16 = 9;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    const PIXELS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;

    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(packed)
        .expect("encode packed Deflate fixture");
    let encoded = encoder.finish().expect("finish packed Deflate fixture");

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, WIDTH);
    push_tag(&mut bytes, 257, 4, 1, HEIGHT);
    push_tag(&mut bytes, 258, 3, 1, 1);
    push_tag(&mut bytes, 259, 3, 1, 8);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(&mut bytes, 273, 4, 1, PIXELS_OFFSET);
    push_tag(&mut bytes, 277, 3, 1, 1);
    push_tag(&mut bytes, 278, 4, 1, HEIGHT);
    push_tag(&mut bytes, 279, 4, 1, encoded.len() as u32);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&encoded);
    bytes
}

fn four_bit_chunky_rgb_tiff() -> Vec<u8> {
    const TAG_COUNT: u16 = 10;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    const BITS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;
    const OFFSETS_OFFSET: u32 = BITS_OFFSET + 3 * 2;
    const COUNTS_OFFSET: u32 = OFFSETS_OFFSET + 2 * 4;
    const PIXELS_OFFSET: u32 = COUNTS_OFFSET + 2 * 4;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, 3);
    push_tag(&mut bytes, 257, 4, 1, 2);
    push_tag(&mut bytes, 258, 3, 3, BITS_OFFSET);
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 2);
    push_tag(&mut bytes, 273, 4, 2, OFFSETS_OFFSET);
    push_tag(&mut bytes, 277, 3, 1, 3);
    push_tag(&mut bytes, 278, 4, 1, 1);
    push_tag(&mut bytes, 279, 4, 2, COUNTS_OFFSET);
    push_tag(&mut bytes, 284, 3, 1, 1);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    for _ in 0..3 {
        bytes.extend_from_slice(&4_u16.to_le_bytes());
    }
    for offset in [PIXELS_OFFSET, PIXELS_OFFSET + 5] {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
    for _ in 0..2 {
        bytes.extend_from_slice(&5_u32.to_le_bytes());
    }
    bytes.extend_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x90]);
    bytes.extend_from_slice(&[0xab, 0xcd, 0xef, 0x01, 0x20]);
    bytes
}

fn four_bit_planar_tiff() -> Vec<u8> {
    const TAG_COUNT: u16 = 10;
    const IFD_OFFSET: u32 = 8;
    const IFD_SIZE: u32 = 2 + TAG_COUNT as u32 * 12 + 4;
    const OFFSETS_OFFSET: u32 = IFD_OFFSET + IFD_SIZE;
    const COUNTS_OFFSET: u32 = OFFSETS_OFFSET + 4 * 4;
    const PIXELS_OFFSET: u32 = COUNTS_OFFSET + 4 * 4;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&IFD_OFFSET.to_le_bytes());
    bytes.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut bytes, 256, 4, 1, 3);
    push_tag(&mut bytes, 257, 4, 1, 2);
    push_tag(&mut bytes, 258, 3, 2, 4 | (4 << 16));
    push_tag(&mut bytes, 259, 3, 1, 1);
    push_tag(&mut bytes, 262, 3, 1, 1);
    push_tag(&mut bytes, 273, 4, 4, OFFSETS_OFFSET);
    push_tag(&mut bytes, 277, 3, 1, 2);
    push_tag(&mut bytes, 278, 4, 1, 1);
    push_tag(&mut bytes, 279, 4, 4, COUNTS_OFFSET);
    push_tag(&mut bytes, 284, 3, 1, 2);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    for offset in [
        PIXELS_OFFSET,
        PIXELS_OFFSET + 2,
        PIXELS_OFFSET + 4,
        PIXELS_OFFSET + 6,
    ] {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
    for _ in 0..4 {
        bytes.extend_from_slice(&2_u32.to_le_bytes());
    }
    bytes.extend_from_slice(&[0x12, 0x30, 0x45, 0x60]);
    bytes.extend_from_slice(&[0xab, 0xc0, 0xde, 0xf0]);
    bytes
}

fn push_tag(bytes: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&field_type.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_tag_be(bytes: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
    bytes.extend_from_slice(&tag.to_be_bytes());
    bytes.extend_from_slice(&field_type.to_be_bytes());
    bytes.extend_from_slice(&count.to_be_bytes());
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn write_u32_le(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
