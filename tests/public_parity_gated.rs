//! Pixel expectations captured from Java Bio-Formats 8.3.0 and confirmed or
//! extended against 8.5.0 where noted by the fixture test.

use std::path::{Path, PathBuf};

use bioformats_rs::{
    open, DimensionOrder, FormatId, ImageReader, LookupTable, PixelType, PlaneCoordinates,
    ReadRequest, Rect, Region,
};
use sha2::{Digest, Sha256};

fn fixture(environment_key: &str) -> PathBuf {
    let path = PathBuf::from(
        std::env::var(environment_key)
            .unwrap_or_else(|_| panic!("set {environment_key} to the downloaded public fixture")),
    );
    assert!(path.exists(), "fixture does not exist: {}", path.display());
    path
}

fn assert_sha256(bytes: &[u8], expected: &str) {
    let actual = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(actual, expected);
}

fn assert_nd2_u16_lookup_table(table: &LookupTable, color: u32) {
    for (component, values, maximum) in [
        ("red", table.red.as_slice(), color & 0xff),
        ("green", table.green.as_slice(), (color >> 8) & 0xff),
        ("blue", table.blue.as_slice(), (color >> 16) & 0xff),
    ] {
        assert_eq!(values.len(), 65_536, "{component} LUT length");
        for (index, &actual) in values.iter().enumerate() {
            let scale = index as f64 / 255.0;
            let expected = (maximum as f64 * scale) as u16;
            assert_eq!(actual, expected, "{component}[{index}]");
        }
    }
}

fn assert_dataset_request_path(
    path: &Path,
    format: FormatId,
    plane_index: u32,
    plane_hash: &str,
    region_hash: &str,
) {
    let dataset = open(path).unwrap();
    assert_eq!(dataset.format(), format);
    let metadata = dataset.series()[0].resolutions()[0].metadata();
    let (z, c, t) = metadata.get_zct_coords(plane_index);
    let full_request = ReadRequest::new(0, PlaneCoordinates::new(z, c, t));
    let info = dataset.plane_info(full_request).unwrap();
    assert_eq!(info.layout.samples_per_pixel, metadata.samples_per_pixel);
    assert_sha256(
        dataset.read_plane(full_request).unwrap().bytes(),
        plane_hash,
    );
    let mut full_destination = vec![0xa5; info.byte_len + 7];
    dataset
        .read_plane_into(full_request, &mut full_destination)
        .unwrap();
    assert_sha256(&full_destination[..info.byte_len], plane_hash);
    assert_eq!(&full_destination[info.byte_len..], &[0xa5; 7]);

    let region_request = full_request.with_region(Region::Rect(Rect::new(17, 19, 16, 12).unwrap()));
    let region = dataset.read_plane(region_request).unwrap();
    assert_sha256(region.bytes(), region_hash);
    let mut region_destination = vec![0x5a; region.info().byte_len + 7];
    dataset
        .read_plane_into(region_request, &mut region_destination)
        .unwrap();
    assert_sha256(&region_destination[..region.info().byte_len], region_hash);
    assert_eq!(&region_destination[region.info().byte_len..], &[0x5a; 7]);
}

fn assert_dataset_coordinate_path(
    path: &Path,
    coordinates: PlaneCoordinates,
    plane_hash: &str,
    region_hash: &str,
) {
    let dataset = open(path).unwrap();
    assert_eq!(dataset.format(), FormatId::Nd2);
    let full_request = ReadRequest::new(0, coordinates);
    let full = dataset.read_plane(full_request).unwrap();
    assert_sha256(full.bytes(), plane_hash);
    let mut full_destination = vec![0xa5; full.info().byte_len + 7];
    dataset
        .read_plane_into(full_request, &mut full_destination)
        .unwrap();
    assert_sha256(&full_destination[..full.info().byte_len], plane_hash);
    assert_eq!(&full_destination[full.info().byte_len..], &[0xa5; 7]);
    let region_request = full_request.with_region(Region::Rect(Rect::new(17, 19, 16, 12).unwrap()));
    let region = dataset.read_plane(region_request).unwrap();
    assert_sha256(region.bytes(), region_hash);
    let mut region_destination = vec![0x5a; region.info().byte_len + 7];
    dataset
        .read_plane_into(region_request, &mut region_destination)
        .unwrap();
    assert_sha256(&region_destination[..region.info().byte_len], region_hash);
    assert_eq!(&region_destination[region.info().byte_len..], &[0x5a; 7]);
}

fn assert_nd2_fixture_parity(
    path: &Path,
    size_xy: (u32, u32),
    size_zct: (u32, u32, u32),
    pixel_type: PixelType,
    significant_bits: u8,
    dimension_order: DimensionOrder,
    cases: &[(u32, PlaneCoordinates, &str, &str)],
) {
    let mut reader = ImageReader::open(path).unwrap();
    assert_eq!(reader.format(), Some(FormatId::Nd2));
    let metadata = reader.metadata().clone();
    assert_eq!((metadata.size_x, metadata.size_y), size_xy);
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        size_zct
    );
    assert_eq!(metadata.image_count, size_zct.0 * size_zct.1 * size_zct.2);
    assert_eq!(metadata.pixel_type, pixel_type);
    assert_eq!(metadata.bits_per_pixel, significant_bits);
    assert_eq!(metadata.dimension_order, dimension_order);
    assert_eq!(metadata.samples_per_pixel, 1);
    assert!(metadata.is_little_endian);

    for &(plane, coordinates, plane_hash, region_hash) in cases {
        assert_eq!(
            metadata.get_index(coordinates.z, coordinates.c, coordinates.t),
            plane
        );
        assert_sha256(&reader.open_bytes(plane).unwrap(), plane_hash);
        assert_sha256(
            &reader.open_bytes_region(plane, 17, 19, 16, 12).unwrap(),
            region_hash,
        );
        assert_dataset_coordinate_path(path, coordinates, plane_hash, region_hash);
    }
}

#[test]
#[ignore = "requires the public BF007 ND2 fixture; see tests/data/README.md"]
fn nd2_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_ND2_PUBLIC_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.format(), Some(FormatId::Nd2));
    let metadata = reader.metadata().clone();
    assert_eq!((metadata.size_x, metadata.size_y), (164, 156));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (1, 1, 1)
    );
    assert_eq!(metadata.image_count, 1);
    assert_eq!(metadata.pixel_type, PixelType::Uint16);
    assert_eq!(metadata.bits_per_pixel, 16);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYZCT);
    assert!(metadata.is_little_endian);
    assert!(metadata.is_indexed);
    assert!(metadata.is_false_color);
    assert_eq!(
        metadata.channel_metadata[0].name.as_deref(),
        Some("405/488/561/633nm")
    );
    assert_eq!(metadata.channel_metadata[0].color, Some(0x00ff_1e00));
    assert_nd2_u16_lookup_table(metadata.lookup_table.as_ref().unwrap(), 0x00ff_1e00);

    let plane = reader.open_bytes(0).unwrap();
    assert_eq!(plane.len(), 51_168);
    assert_sha256(
        &plane,
        "622378014f3a8bd38dd02a31119bb7f8c334b8d709db268ccfbe39b958586bcd",
    );

    let region = reader.open_bytes_region(0, 17, 19, 16, 12).unwrap();
    assert_eq!(region.len(), 384);
    assert_sha256(
        &region,
        "04ee3391751c92df215fa59b4d87df6984723c2a3790c57ba0ebbbf8cef45798",
    );
    assert_dataset_request_path(
        &path,
        FormatId::Nd2,
        0,
        "622378014f3a8bd38dd02a31119bb7f8c334b8d709db268ccfbe39b958586bcd",
        "04ee3391751c92df215fa59b4d87df6984723c2a3790c57ba0ebbbf8cef45798",
    );
}

#[test]
#[ignore = "requires the public MRAP1 ND2 fixture; see tests/data/README.md"]
fn nd2_shared_multichannel_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_ND2_MULTICHANNEL_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    let metadata = reader.metadata().clone();
    assert_eq!((metadata.size_x, metadata.size_y), (2_880, 2_048));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (1, 3, 1)
    );
    assert_eq!(metadata.image_count, 3);
    assert_eq!(metadata.pixel_type, PixelType::Uint8);
    assert_eq!(metadata.bits_per_pixel, 8);
    assert_eq!(metadata.samples_per_pixel, 1);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);
    assert!(!metadata.is_rgb);
    assert!(!metadata.is_interleaved);

    for (plane, coordinates, plane_hash, region_hash) in [
        (
            0,
            PlaneCoordinates::new(0, 0, 0),
            "6cedb493f3c8c32afd57032dfdd97a17d7ba8c333b524fe00d89dbe2d61215f7",
            "34de729c58b0a9bb6903b2a4fbcecf5f8a921819ee1b37a3250c0d737a537114",
        ),
        (
            1,
            PlaneCoordinates::new(0, 1, 0),
            "ad631b9589f41e8412638509026e219399a17c43940aaeb02cad32c077f5185a",
            "8ee35f0a3d164c682dd0c19ece0a2884cf80b1933fefed8bf1d818da728c18d8",
        ),
        (
            2,
            PlaneCoordinates::new(0, 2, 0),
            "8f3fd5239df16b2e1da2bac5c0c598e2e56662c5b626dfc4bda9b3be6501578a",
            "974a60fa183ff3302c8eda1c189196239400310bfcba78af31ca9aac0d8dac21",
        ),
    ] {
        assert_eq!(
            metadata.get_index(coordinates.z, coordinates.c, coordinates.t),
            plane
        );
        assert_sha256(&reader.open_bytes(plane).unwrap(), plane_hash);
        assert_sha256(
            &reader.open_bytes_region(plane, 17, 19, 16, 12).unwrap(),
            region_hash,
        );
        assert_dataset_coordinate_path(&path, coordinates, plane_hash, region_hash);
    }
}

#[test]
#[ignore = "requires the public Exception_2 ND2 fixture; see tests/data/README.md"]
fn nd2_zlib_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_ND2_ZLIB_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    let metadata = reader.metadata().clone();
    assert_eq!((metadata.size_x, metadata.size_y), (696, 520));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (31, 1, 1)
    );
    assert_eq!(metadata.image_count, 31);
    assert_eq!(metadata.pixel_type, PixelType::Uint16);
    assert_eq!(metadata.bits_per_pixel, 14);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYZCT);

    for (plane, plane_hash, region_hash) in [
        (
            0,
            "1bf675dd832595318dadc3c58bbb71efbc29e4e707349f44139e9cb0efd4ae20",
            "d83157d08d9b257cc09ef8d8fa1e269b75e9e58281b48d0b5a530889c6760f35",
        ),
        (
            15,
            "083ca9a1c376e183cddbffd47f3a53ddedca83b86578e54f5220d4fb2cfab377",
            "8161c5167cfe89a09cc46ebfafe2e055af7dd43671c6e4ded757cbecec107fbd",
        ),
        (
            30,
            "b75f247270dd178527d5ddd6bb1bf69965c271c215ed35e91828bd135dba1448",
            "73dcfd89db6099b4614ad6e1d5424614907f95b43cf55833ac2fdf08a58e41bc",
        ),
    ] {
        let coordinates = PlaneCoordinates::new(plane, 0, 0);
        assert_eq!(metadata.get_index(coordinates.z, 0, 0), plane);
        assert_sha256(&reader.open_bytes(plane).unwrap(), plane_hash);
        assert_sha256(
            &reader.open_bytes_region(plane, 17, 19, 16, 12).unwrap(),
            region_hash,
        );
        assert_dataset_coordinate_path(&path, coordinates, plane_hash, region_hash);
    }
}

#[test]
#[ignore = "requires the public control002 ND2 fixture; see tests/data/README.md"]
fn nd2_padded_zt_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_ND2_PADDED_ZT_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    let metadata = reader.metadata().clone();
    assert_eq!((metadata.size_x, metadata.size_y), (247, 152));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (9, 1, 65)
    );
    assert_eq!(metadata.image_count, 585);
    assert_eq!(metadata.pixel_type, PixelType::Uint16);
    assert_eq!(metadata.bits_per_pixel, 14);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);

    for (plane, coordinates, plane_hash, region_hash) in [
        (
            0,
            PlaneCoordinates::new(0, 0, 0),
            "49280d49aa1addbfd15a2cd6abde8b8e7c07577c6a7f7d0c6f748e92cf81ad18",
            "6df45b85401b9c0510fb739fe385ec15f4d4d855fad8debb68d1ed937e073739",
        ),
        (
            292,
            PlaneCoordinates::new(4, 0, 32),
            "89c04de3be4efb39f7c214001597d97068b7c1678f278f83947d690f47c53eb4",
            "faef2c783928de70545c4ff27f94d6a9947e8bfaf7ce81e29fd45bffd6f83a72",
        ),
        (
            584,
            PlaneCoordinates::new(8, 0, 64),
            "6aa635fe524f4df84173de2424ba7670d9dbea19191a9a1e1e8b10b0abf24b6d",
            "a7a64e713945974c432a808b660b105d3f851f019367546b6882738d660b7fbd",
        ),
    ] {
        assert_eq!(
            metadata.get_index(coordinates.z, coordinates.c, coordinates.t),
            plane
        );
        assert_sha256(&reader.open_bytes(plane).unwrap(), plane_hash);
        assert_sha256(
            &reader.open_bytes_region(plane, 17, 19, 16, 12).unwrap(),
            region_hash,
        );
        assert_dataset_coordinate_path(&path, coordinates, plane_hash, region_hash);
    }
}

#[test]
#[ignore = "requires the public MeOh ND2 fixture; see tests/data/README.md"]
fn nd2_planned_spectral_loop_matches_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_ND2_PLANNED_LOOP_FIXTURE");
    assert_nd2_fixture_parity(
        &path,
        (800, 600),
        (1, 1, 13),
        PixelType::Uint16,
        12,
        DimensionOrder::XYCZT,
        &[
            (
                0,
                PlaneCoordinates::new(0, 0, 0),
                "76a8c9d09190f4bbd0f01ac6d5210be919c51c31490b5e365f63f13b2172ada8",
                "040459bbdf631d0429721e806e046f5ab14f12f4bdc0b1d2036ce4017dfbe2d1",
            ),
            (
                6,
                PlaneCoordinates::new(0, 0, 6),
                "4e7d8ea7335aa79c5a0243d480a150ae21f1679ae5e35d3bc78e6c1c763ba1a7",
                "41d0f162d412f79c684dbc39a42e8acfd60551f8b02b7a02a00055abdecdf609",
            ),
            (
                12,
                PlaneCoordinates::new(0, 0, 12),
                "4b0d51fb61322e251fd3f7d984b376ff25e5269346d1066bc898b05639408b38",
                "d1cab9a78804f76d785d0890f10e4c50f57c2c0ae8b70d054f7dd78d2c969269",
            ),
        ],
    );

    let metadata = ImageReader::open(&path).unwrap().metadata().clone();
    assert!(metadata.is_indexed);
    assert!(metadata.is_false_color);
    assert_eq!(
        metadata.channel_metadata[0].name.as_deref(),
        Some("pdt-405")
    );
    assert_eq!(metadata.channel_metadata[0].color, Some(0x0000_00ff));
    assert_nd2_u16_lookup_table(metadata.lookup_table.as_ref().unwrap(), 0x0000_00ff);
}

#[test]
#[ignore = "requires the public header_test2 ND2 fixture; see tests/data/README.md"]
fn nd2_ne_time_period_selection_matches_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_ND2_NETIME_FIXTURE");
    assert_nd2_fixture_parity(
        &path,
        (696, 520),
        (5, 1, 4),
        PixelType::Uint16,
        14,
        DimensionOrder::XYCZT,
        &[
            (
                0,
                PlaneCoordinates::new(0, 0, 0),
                "b88a49da7f161fe0fc1f35a875e3ed6ee42d4ba221c197156ef560a93e19ae74",
                "012b8699ca2ad966b66cc069815158be2888dfcfe91693049f7e073ce49524e0",
            ),
            (
                10,
                PlaneCoordinates::new(0, 0, 2),
                "42aed7a8445a729257ae28127a7192f560cc2e104882c722eee3edfdbd78ac86",
                "3a026cad14da6f2d765a7286eae8bd7e3cb06bc3165b1d471ef25259717719bb",
            ),
            (
                19,
                PlaneCoordinates::new(4, 0, 3),
                "7b1c473e8f794bfccb31226c809c56aecf859071335f9a9af9f1f0e1958a71a3",
                "8b418822bcfd903b68c9871d203cdae5233909a874ab1a8e13fc521a9165bd81",
            ),
        ],
    );
}

#[test]
#[ignore = "requires the public Exception61 ND2 fixture; see tests/data/README.md"]
fn nd2_final_metadata_precedence_matches_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_ND2_FINAL_METADATA_FIXTURE");
    assert_nd2_fixture_parity(
        &path,
        (696, 520),
        (13, 1, 11),
        PixelType::Uint16,
        14,
        DimensionOrder::XYZCT,
        &[
            (
                0,
                PlaneCoordinates::new(0, 0, 0),
                "c8241217d4812f3befb377c011c3c21a243f6c913aec8e9179a42e4160b01d4b",
                "4676ca8ad87ca3fa20371df32111722391d832b1a04ddd18df7f13dda0a62a79",
            ),
            (
                71,
                PlaneCoordinates::new(6, 0, 5),
                "09d2fc3c524247c3a64f0fb04be06a4ea4c64b598ee2274e1c7a826ab5bee35f",
                "f2005374f77694e440c4062bf8fd0f35a39b128ae286b250f2cfef26b567af36",
            ),
            (
                142,
                PlaneCoordinates::new(12, 0, 10),
                "9b4d739e47af52efaf2f6fa0b79b364b6417f0da563eb31fd1f2040f06aded45",
                "8c0f83e377f4e894f9716c476456bebb7469966b50b9d7e9e7e010294679700b",
            ),
        ],
    );
}

#[test]
#[ignore = "requires the public Experiment_0001 ND2 fixture; see tests/data/README.md"]
fn nd2_binary_lv_and_four_byte_padding_match_java_bioformats_8_5() {
    let path = fixture("BIOFORMATS_RS_ND2_BINARY_LV_FIXTURE");
    assert_nd2_fixture_parity(
        &path,
        (1_750, 1_664),
        (17, 1, 1),
        PixelType::Uint8,
        8,
        DimensionOrder::XYZCT,
        &[
            (
                0,
                PlaneCoordinates::new(0, 0, 0),
                "567099b31088b331b1f8c80aca6aca02085c819d214f7546ee7f05096f2912a0",
                "8dcdee5faaa5dad17fc51918c0b6bb001b59ac3edcd264645e0d05bb57afce0a",
            ),
            (
                8,
                PlaneCoordinates::new(8, 0, 0),
                "bb363cb51a39faff2a1d053a9d6eb56b36e0798e47070cba90b5ed47221b101e",
                "f80365b5a718e4f0f5733ef378641aedd9f2db10388aefa975c8ac4e75c90458",
            ),
            (
                16,
                PlaneCoordinates::new(16, 0, 0),
                "1f3364e0809019f9544edcbd628f36d3d7b5455b55465ce5aee80c5d1ad7a971",
                "69dde66df340dcfc014983aff61e02a418b1e3a67ab534d346c416b906e0a94b",
            ),
        ],
    );
}

#[test]
#[ignore = "requires the public idr0011 CZI fixture; see tests/data/README.md"]
fn czi_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_CZI_PUBLIC_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.format(), Some(FormatId::Czi));
    let metadata = reader.metadata();
    assert_eq!((metadata.size_x, metadata.size_y), (672, 512));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (21, 3, 1)
    );
    assert_eq!(metadata.image_count, 63);
    assert_eq!(metadata.samples_per_pixel, 1);
    assert_eq!(metadata.pixel_type, PixelType::Uint16);
    assert_eq!(metadata.bits_per_pixel, 12);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);
    assert!(!metadata.is_rgb);
    assert!(!metadata.is_interleaved);
    assert!(metadata.is_false_color);
    assert!(metadata.is_little_endian);

    for (plane, expected) in [
        (
            0,
            "6b77f819b045b3ec64ada12c0c47bea74ebdef8c867c15acf678f2c52ccb6f1e",
        ),
        (
            31,
            "123d605f92fbdd7cc34b6233f20f7a5ccb03909f1100496c2fef91fdb30507b4",
        ),
        (
            62,
            "b7d99f883d030ecaf13e6d601a3959f53c2cdaae61cd369c0221a5c290f63cbf",
        ),
    ] {
        let bytes = reader.open_bytes(plane).unwrap();
        assert_eq!(bytes.len(), 688_128);
        assert_sha256(&bytes, expected);
    }

    let region = reader.open_bytes_region(31, 17, 19, 16, 12).unwrap();
    assert_eq!(region.len(), 384);
    assert_sha256(
        &region,
        "286888bfe31bc8fcea67ccbd9bce4638b602504e6e90fe6fd1c80631c064e732",
    );
    assert_dataset_request_path(
        &path,
        FormatId::Czi,
        31,
        "123d605f92fbdd7cc34b6233f20f7a5ccb03909f1100496c2fef91fdb30507b4",
        "286888bfe31bc8fcea67ccbd9bce4638b602504e6e90fe6fd1c80631c064e732",
    );
}

#[test]
#[ignore = "requires the public dt-helix NRRD fixture; see tests/data/README.md"]
fn nrrd_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_NRRD_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.format(), Some(FormatId::Nrrd));
    let metadata = reader.metadata();
    assert_eq!((metadata.size_x, metadata.size_y), (38, 39));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (40, 7, 1)
    );
    assert_eq!(metadata.image_count, 40);
    assert_eq!(metadata.samples_per_pixel, 7);
    assert_eq!(metadata.pixel_type, PixelType::Float32);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);
    assert!(metadata.is_rgb);
    assert!(metadata.is_interleaved);
    assert!(!metadata.is_little_endian);

    for (plane, expected) in [
        (
            0,
            "0ce459cbfc805defc28f5bc867bb5129dfed2841e78589e3f19c34c23be2b05c",
        ),
        (
            20,
            "4285eeebc732ad584b480cbd85d22418b55228a0fc76420320ec4109c7edb730",
        ),
        (
            39,
            "07333da0227c6d5bdbe7fc273857d78b9daf36752706565d4ba546035bf8de14",
        ),
    ] {
        assert_sha256(&reader.open_bytes(plane).unwrap(), expected);
    }
    let region = reader.open_bytes_region(20, 17, 19, 16, 12).unwrap();
    assert_eq!(region.len(), 5_376);
    assert_sha256(
        &region,
        "ec93997cbb805376ca30322957a97b767cf77844aebc4b72b64bc824b6a74f8b",
    );
    assert_dataset_request_path(
        &path,
        FormatId::Nrrd,
        20,
        "4285eeebc732ad584b480cbd85d22418b55228a0fc76420320ec4109c7edb730",
        "ec93997cbb805376ca30322957a97b767cf77844aebc4b72b64bc824b6a74f8b",
    );
}

#[test]
#[ignore = "requires the public EMD-2225 MRC fixture; see tests/data/README.md"]
fn mrc_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_MRC_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.format(), Some(FormatId::Mrc));
    let metadata = reader.metadata();
    assert_eq!((metadata.size_x, metadata.size_y), (128, 128));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (128, 1, 1)
    );
    assert_eq!(metadata.image_count, 128);
    assert_eq!(metadata.samples_per_pixel, 1);
    assert_eq!(metadata.pixel_type, PixelType::Float32);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYZTC);
    assert!(metadata.is_little_endian);

    for (plane, expected) in [
        (
            0,
            "a45e67249fe3df7f86d55a462e9def2db3635df187c81ac2f344a90338671874",
        ),
        (
            64,
            "baaa5fe2259d08eacc103469115c0b0b56107c02e6e508ef4b99b04122c765c0",
        ),
        (
            127,
            "2d7e458d647c7587e77fe7c10fda45941c1c4fe084e402a98a987d4cd2a646b3",
        ),
    ] {
        assert_sha256(&reader.open_bytes(plane).unwrap(), expected);
    }
    let region = reader.open_bytes_region(64, 17, 19, 16, 12).unwrap();
    assert_eq!(region.len(), 768);
    assert_sha256(
        &region,
        "dab1e6bc8957602dd7449a7b91e855c1cb0607596fd0a5d5c3d42d2c7dcd4166",
    );
    assert_dataset_request_path(
        &path,
        FormatId::Mrc,
        64,
        "baaa5fe2259d08eacc103469115c0b0b56107c02e6e508ef4b99b04122c765c0",
        "dab1e6bc8957602dd7449a7b91e855c1cb0607596fd0a5d5c3d42d2c7dcd4166",
    );
}

#[test]
#[ignore = "requires the public Cell07 DCIMG fixture; see tests/data/README.md"]
fn dcimg_pixels_match_java_bioformats() {
    let path = fixture("BIOFORMATS_RS_DCIMG_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.format(), Some(FormatId::Dcimg));
    let metadata = reader.metadata();
    assert_eq!((metadata.size_x, metadata.size_y), (2_048, 168));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (1, 1, 10)
    );
    assert_eq!(metadata.image_count, 10);
    assert_eq!(metadata.samples_per_pixel, 1);
    assert_eq!(metadata.pixel_type, PixelType::Uint16);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYZCT);
    assert!(metadata.is_little_endian);

    for (plane, expected) in [
        (
            0,
            "75b696da6cee8a0ce5267a1cb25c285f2fa51f9978fc9100a20eeaa871d70875",
        ),
        (
            5,
            "0fcde0a59810a33f28aea165a981c3c24ee2e5bb1926aac585c9cad136e28441",
        ),
        (
            9,
            "0dd22d89277f63b1d853d30fe6f1aba9bc69aed5d4b276458f0fce09bff106fd",
        ),
    ] {
        assert_sha256(&reader.open_bytes(plane).unwrap(), expected);
    }
    let region = reader.open_bytes_region(5, 17, 19, 16, 12).unwrap();
    assert_eq!(region.len(), 384);
    assert_sha256(
        &region,
        "c56e1599719007205d80d3b396b3b026bc1ef746f36a69345dca48f611a7b93c",
    );
    assert_dataset_request_path(
        &path,
        FormatId::Dcimg,
        5,
        "0fcde0a59810a33f28aea165a981c3c24ee2e5bb1926aac585c9cad136e28441",
        "c56e1599719007205d80d3b396b3b026bc1ef746f36a69345dca48f611a7b93c",
    );
}

#[test]
#[ignore = "requires the public bead_bot4_018 DCIMG group; see tests/data/README.md"]
fn grouped_dcimg_z_stack_matches_java_bioformats_8_5() {
    let path = fixture("BIOFORMATS_RS_DCIMG_GROUP_FIXTURE");
    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.format(), Some(FormatId::Dcimg));
    let metadata = reader.metadata();
    assert_eq!((metadata.size_x, metadata.size_y), (2_048, 200));
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (11, 1, 1)
    );
    assert_eq!(metadata.image_count, 11);
    assert_eq!(metadata.samples_per_pixel, 1);
    assert_eq!(metadata.pixel_type, PixelType::Uint16);
    assert_eq!(metadata.bits_per_pixel, 16);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYZCT);
    assert!(metadata.is_little_endian);
    assert_eq!(reader.used_files().len(), 11);
    assert_eq!(reader.used_sources().len(), 11);
    for (index, used_file) in reader.used_files().iter().enumerate() {
        let expected_name = format!("bead_bot4__560_00000_{index:05}.dcimg");
        assert_eq!(
            used_file.file_name().and_then(|name| name.to_str()),
            Some(expected_name.as_str())
        );
    }

    for (plane, expected) in [
        (
            0,
            "747cef1be18aecb9d21e74e16a177c0988ce4ffe5df3557e7d52982a82e00da5",
        ),
        (
            5,
            "c7b3ca237325ac6816835a5c7d46aee92681aec77322fcd0112a71ea4267369a",
        ),
        (
            10,
            "716e651150df4b99b4c73c7d677dd86184427d7f5a086800a346a09faf2fdcf0",
        ),
    ] {
        let bytes = reader.open_bytes(plane).unwrap();
        assert_eq!(bytes.len(), 819_200);
        assert_sha256(&bytes, expected);
    }
    let region = reader.open_bytes_region(5, 17, 19, 16, 12).unwrap();
    assert_eq!(region.len(), 384);
    assert_sha256(
        &region,
        "67bcb14489d1d4151d131ef05ae9d2527069fdf30a93bb097bc3a602d3acd72a",
    );
    assert_dataset_request_path(
        &path,
        FormatId::Dcimg,
        5,
        "c7b3ca237325ac6816835a5c7d46aee92681aec77322fcd0112a71ea4267369a",
        "67bcb14489d1d4151d131ef05ae9d2527069fdf30a93bb097bc3a602d3acd72a",
    );
}
