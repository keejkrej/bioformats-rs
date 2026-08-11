use std::path::{Path, PathBuf};

use bioformats_rs::{
    open, BioFormatsError, FormatId, FormatReader, ImageReader, PlaneCoordinates, ReadRequest,
    Rect, Region,
};

const SEGMENT_HEADER: usize = 32;
const FILE_HEADER_BODY: usize = 80;
const DIRECTORY_HEADER: usize = 128;
const SUBBLOCK_HEADER: usize = 256;
const PADDED_JPEG_3X3_GRAY: &[u8] = &[
    255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 255, 219, 0, 67, 0, 3,
    2, 2, 2, 2, 2, 3, 2, 2, 2, 3, 3, 3, 3, 4, 6, 4, 4, 4, 4, 4, 8, 6, 6, 5, 6, 9, 8, 10, 10, 9, 8,
    9, 9, 10, 12, 15, 12, 10, 11, 14, 11, 9, 9, 13, 17, 13, 14, 15, 16, 16, 17, 16, 10, 12, 18, 19,
    18, 16, 19, 15, 16, 16, 16, 255, 192, 0, 11, 8, 0, 3, 0, 3, 1, 1, 17, 0, 255, 196, 0, 31, 0, 0,
    1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 255, 196, 0,
    181, 16, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125, 1, 2, 3, 0, 4, 17, 5, 18, 33, 49,
    65, 6, 19, 81, 97, 7, 34, 113, 20, 50, 129, 145, 161, 8, 35, 66, 177, 193, 21, 82, 209, 240,
    36, 51, 98, 114, 130, 9, 10, 22, 23, 24, 25, 26, 37, 38, 39, 40, 41, 42, 52, 53, 54, 55, 56,
    57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103,
    104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 131, 132, 133, 134, 135, 136, 137, 138,
    146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178,
    179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211,
    212, 213, 214, 215, 216, 217, 218, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 241, 242,
    243, 244, 245, 246, 247, 248, 249, 250, 255, 218, 0, 8, 1, 1, 0, 0, 63, 0, 248, 106, 191, 255,
    217,
];

#[derive(Clone)]
struct Tile {
    pixel_type: i32,
    channel: i32,
    x: i32,
    y: i32,
    logical_width: i32,
    logical_height: i32,
    stored_width: i32,
    stored_height: i32,
    z: i32,
    mosaic: i32,
    pyramid_type: u8,
    compression: i32,
    pixels: Vec<u8>,
}

impl Tile {
    #[allow(clippy::too_many_arguments)] // Mirrors the CZI dimension entry fields at call sites.
    fn raw(
        x: i32,
        y: i32,
        logical_width: i32,
        logical_height: i32,
        stored_width: i32,
        stored_height: i32,
        z: i32,
        mosaic: i32,
        pixels: &[u8],
    ) -> Self {
        Self {
            pixel_type: 0,
            channel: 0,
            x,
            y,
            logical_width,
            logical_height,
            stored_width,
            stored_height,
            z,
            mosaic,
            pyramid_type: u8::from(
                logical_width != stored_width || logical_height != stored_height,
            ),
            compression: 0,
            pixels: pixels.to_vec(),
        }
    }

    fn with_pixel_type(mut self, pixel_type: i32) -> Self {
        self.pixel_type = pixel_type;
        self
    }

    fn with_channel(mut self, channel: i32) -> Self {
        self.channel = channel;
        self
    }
}

struct TemporaryCzi {
    path: PathBuf,
}

impl TemporaryCzi {
    fn new(name: &str, tiles: &[Tile]) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bioformats-rs-{}-{unique}-{name}.czi",
            std::process::id()
        ));
        std::fs::write(&path, generated_czi(tiles)).expect("write generated CZI fixture");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn rename(&mut self, name: &str) {
        let original_name = self.path.file_name().expect("generated CZI filename");
        let destination = self
            .path
            .with_file_name(format!("{name}-{}", original_name.to_string_lossy()));
        std::fs::rename(&self.path, &destination).expect("relocate generated CZI fixture");
        self.path = destination;
    }
}

impl Drop for TemporaryCzi {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn pyramid_tiles() -> Vec<Tile> {
    vec![
        Tile::raw(
            -4,
            -8,
            4,
            4,
            4,
            4,
            0,
            0,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        ),
        Tile::raw(
            2,
            -8,
            4,
            4,
            4,
            4,
            0,
            1,
            &[
                101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116,
            ],
        ),
        Tile::raw(
            -4,
            -8,
            4,
            4,
            4,
            4,
            1,
            0,
            &[
                31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46,
            ],
        ),
        Tile::raw(
            2,
            -8,
            4,
            4,
            4,
            4,
            1,
            1,
            &[
                131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146,
            ],
        ),
        Tile::raw(-4, -8, 4, 4, 2, 2, 0, 0, &[21, 22, 23, 24]),
        Tile::raw(2, -8, 4, 4, 2, 2, 0, 1, &[121, 122, 123, 124]),
        Tile::raw(-4, -8, 4, 4, 2, 2, 1, 0, &[51, 52, 53, 54]),
        Tile::raw(2, -8, 4, 4, 2, 2, 1, 1, &[151, 152, 153, 154]),
    ]
}

#[test]
fn czi_mosaic_pyramid_is_exposed_as_one_series_with_two_resolutions() {
    let mut fixture = TemporaryCzi::new("mosaic-pyramid", &pyramid_tiles());

    let mut reader = ImageReader::open(fixture.path()).expect("open generated CZI");
    assert_eq!(reader.format(), Some(FormatId::Czi));

    // The lower-level API retains Bio-Formats' default flattened view.
    assert_eq!(reader.series_count(), 2);
    assert_eq!(
        (reader.metadata().size_x, reader.metadata().size_y),
        (10, 4)
    );
    reader
        .set_series(1)
        .expect("select flattened pyramid level");
    assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (5, 2));

    reader
        .set_flattened_resolutions(false)
        .expect("select hierarchical resolutions");
    assert_eq!(reader.series(), 0);
    assert_eq!(reader.resolution(), 1);
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.resolution_count(), 2);
    reader.set_resolution(1).expect("select reduced level");
    assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (5, 2));
    assert_eq!(reader.metadata().resolution_count, 2);
    assert_eq!(
        reader.open_bytes(0).expect("read reduced mosaic"),
        [21, 22, 0, 121, 122, 23, 24, 0, 123, 124]
    );
    assert_eq!(
        reader
            .open_bytes_region(1, 1, 0, 3, 2)
            .expect("read reduced Z1 region across the gap"),
        [52, 0, 151, 54, 0, 153]
    );

    let mut snapshot = reader.snapshot().expect("snapshot hierarchical CZI reader");
    fixture.rename("relocated");
    snapshot.retarget_path(fixture.path());
    let mut restored = snapshot
        .into_reader()
        .expect("restore relocated hierarchical CZI reader");
    assert!(!restored.flattened_resolutions());
    assert_eq!(restored.resolution(), 1);
    assert_eq!(
        restored
            .open_bytes(0)
            .expect("read restored reduced mosaic"),
        [21, 22, 0, 121, 122, 23, 24, 0, 123, 124]
    );
    restored
        .set_flattened_resolutions(true)
        .expect("restore flattened view without changing the active level");
    assert_eq!(restored.series(), 1);
    assert_eq!(
        (restored.metadata().size_x, restored.metadata().size_y),
        (5, 2)
    );

    let dataset = open(fixture.path()).expect("open generated CZI dataset");
    assert_eq!(dataset.series().len(), 1);
    assert_eq!(dataset.series()[0].resolutions().len(), 2);
    assert_eq!(
        (
            dataset.series()[0].resolutions()[0].metadata().size_x,
            dataset.series()[0].resolutions()[0].metadata().size_y,
        ),
        (10, 4)
    );
    assert_eq!(
        (
            dataset.series()[0].resolutions()[1].metadata().size_x,
            dataset.series()[0].resolutions()[1].metadata().size_y,
        ),
        (5, 2)
    );

    let base_region = dataset
        .read_plane(
            ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)).with_region(Region::Rect(
                Rect::new(3, 1, 4, 2).expect("valid base region"),
            )),
        )
        .expect("compose a base region across both tiles");
    assert_eq!(base_region.bytes(), [8, 0, 0, 105, 12, 0, 0, 109]);

    let reduced_region = dataset
        .read_plane(
            ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0))
                .with_resolution(1)
                .with_region(Region::Rect(
                    Rect::new(1, 0, 3, 2).expect("valid reduced region"),
                )),
        )
        .expect("compose a reduced region across both tiles");
    assert_eq!(reduced_region.bytes(), [22, 0, 121, 24, 0, 123]);
}

#[test]
fn czi_region_decodes_only_intersecting_mosaic_tiles() {
    let mut tiles = pyramid_tiles();
    for tile in &mut tiles {
        if tile.mosaic == 1 {
            tile.compression = 4; // JPEG-XR remains unsupported.
        }
    }
    let fixture = TemporaryCzi::new("lazy-mosaic-region", &tiles);
    let dataset = open(fixture.path()).expect("metadata does not require tile decoding");

    let left = dataset
        .read_plane(
            ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)).with_region(Region::Rect(
                Rect::new(0, 0, 4, 1).expect("valid left-tile region"),
            )),
        )
        .expect("non-intersecting JPEG-XR tile must not be decoded");
    assert_eq!(left.bytes(), [1, 2, 3, 4]);

    let gap = dataset
        .read_plane(
            ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)).with_region(Region::Rect(
                Rect::new(4, 0, 2, 1).expect("valid missing-tile region"),
            )),
        )
        .expect("a missing-tile region should use the fill value");
    assert_eq!(gap.bytes(), [0, 0]);

    assert!(matches!(
        dataset.read_plane(ReadRequest::new(
            0,
            PlaneCoordinates::new(0, 0, 0)
        )),
        Err(BioFormatsError::UnsupportedFormat(message))
            if message.contains("JPEG-XR")
    ));
}

#[test]
fn czi_thumbnail_falls_back_to_the_readable_active_resolution() {
    let mut reduced = Tile::raw(0, 0, 2, 2, 1, 1, 0, 0, &[0]);
    reduced.compression = 4; // The available pyramid level uses JPEG-XR.
    let tiles = vec![Tile::raw(0, 0, 2, 2, 2, 2, 0, 0, &[1, 2, 3, 4]), reduced];
    let fixture = TemporaryCzi::new("thumbnail-codec-fallback", &tiles);
    let mut reader = ImageReader::open(fixture.path()).expect("open mixed-codec CZI pyramid");

    assert_eq!(
        reader
            .open_thumb_bytes(0)
            .expect("fall back from JPEG-XR pyramid to the raw active level"),
        [1, 2, 3, 4]
    );
}

#[test]
fn czi_thumbnail_keeps_the_full_smallest_level_when_it_is_active() {
    let native = vec![1; 514 * 2];
    let reduced = vec![2; 257];
    let tiles = vec![
        Tile::raw(0, 0, 514, 2, 514, 2, 0, 0, &native),
        Tile::raw(0, 0, 514, 2, 257, 1, 0, 0, &reduced),
    ];
    let fixture = TemporaryCzi::new("active-smallest-thumbnail", &tiles);
    let mut reader = ImageReader::open(fixture.path()).expect("open wide CZI pyramid");
    reader
        .set_series(1)
        .expect("select the flattened smallest level");

    let thumbnail = reader
        .open_thumb_bytes(0)
        .expect("read the complete active smallest level");
    assert_eq!(thumbnail.len(), 257);
    assert!(thumbnail.iter().all(|pixel| *pixel == 2));
}

#[test]
fn czi_identical_mosaic_layouts_remain_separate_series() {
    let tiles = vec![
        Tile::raw(0, 0, 2, 1, 2, 1, 0, 0, &[1, 2]),
        Tile::raw(0, 0, 2, 1, 2, 1, 0, 1, &[11, 12]),
    ];
    let fixture = TemporaryCzi::new("independent-mosaics", &tiles);
    let dataset = open(fixture.path()).expect("open independent CZI mosaics");

    assert_eq!(dataset.series().len(), 2);
    assert_eq!(
        dataset
            .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
            .expect("read first mosaic series")
            .bytes(),
        [1, 2]
    );
    assert_eq!(
        dataset
            .read_plane(ReadRequest::new(1, PlaneCoordinates::default()))
            .expect("read second mosaic series")
            .bytes(),
        [11, 12]
    );
}

#[test]
fn czi_sparse_planes_do_not_fuse_spatially_identical_mosaic_series() {
    let tiles = vec![
        Tile::raw(0, 0, 2, 1, 2, 1, 0, 0, &[1, 2]),
        Tile::raw(0, 0, 2, 1, 2, 1, 1, 0, &[3, 4]),
        Tile::raw(0, 0, 2, 1, 2, 1, 0, 1, &[11, 12]),
    ];
    let fixture = TemporaryCzi::new("sparse-independent-mosaics", &tiles);
    let dataset = open(fixture.path()).expect("open sparse independent CZI mosaics");

    assert_eq!(dataset.series().len(), 2);
    assert_eq!(dataset.series()[1].resolutions()[0].metadata().size_z, 2);
    assert_eq!(
        dataset
            .read_plane(ReadRequest::new(1, PlaneCoordinates::new(0, 0, 0)))
            .expect("read present plane from second mosaic series")
            .bytes(),
        [11, 12]
    );
    assert_eq!(
        dataset
            .read_plane(ReadRequest::new(1, PlaneCoordinates::new(1, 0, 0)))
            .expect("fill missing plane from second mosaic series")
            .bytes(),
        [0, 0]
    );
}

#[test]
fn czi_native_planes_share_mosaic_coordinates_when_an_outer_tile_is_missing() {
    let tiles = vec![
        Tile::raw(0, 0, 4, 1, 4, 1, 0, 0, &[1, 2, 3, 4]),
        Tile::raw(4, 0, 4, 1, 4, 1, 0, 1, &[5, 6, 7, 8]),
        Tile::raw(4, 0, 4, 1, 4, 1, 0, 1, &[15, 16, 17, 18]).with_channel(1),
    ];
    let fixture = TemporaryCzi::new("native-missing-outer-tile", &tiles);
    let dataset = open(fixture.path()).expect("open sparse native CZI mosaic");

    assert_eq!(dataset.series()[0].resolutions()[0].metadata().size_x, 8);
    let second_channel = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 1, 0)))
        .expect("preserve the missing left tile in channel one");
    assert_eq!(second_channel.bytes(), [0, 0, 0, 0, 15, 16, 17, 18]);
}

#[test]
fn czi_missing_native_coordinate_plane_uses_the_series_fill() {
    let tiles = vec![
        Tile::raw(0, 0, 2, 1, 2, 1, 0, 0, &[1, 2]),
        Tile::raw(0, 0, 2, 1, 2, 1, 2, 0, &[5, 6]),
    ];
    let fixture = TemporaryCzi::new("missing-native-plane", &tiles);
    let dataset = open(fixture.path()).expect("open sparse native CZI stack");

    assert_eq!(dataset.series()[0].resolutions()[0].metadata().size_z, 3);
    let missing = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(1, 0, 0)))
        .expect("fill the absent native Z coordinate");
    assert_eq!(missing.bytes(), [0, 0]);
}

#[test]
fn czi_jpeg_vendor_padding_is_cropped_to_the_stored_tile() {
    let tile = Tile {
        pixel_type: 0,
        channel: 0,
        x: 0,
        y: 0,
        logical_width: 2,
        logical_height: 2,
        stored_width: 2,
        stored_height: 2,
        z: 0,
        mosaic: 0,
        pyramid_type: 0,
        compression: 1,
        pixels: PADDED_JPEG_3X3_GRAY.to_vec(),
    };
    let fixture = TemporaryCzi::new("padded-jpeg", &[tile]);
    let dataset = open(fixture.path()).expect("open padded-JPEG CZI");

    let plane = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
        .expect("crop vendor-padded JPEG tile");
    assert_eq!(plane.bytes(), [42, 42, 42, 42]);
}

#[test]
fn czi_rgb_pyramid_uses_white_missing_tile_fill_and_returns_rgb_order() {
    let tiles = vec![
        Tile::raw(
            0,
            0,
            2,
            2,
            2,
            2,
            0,
            0,
            &[3, 2, 1, 3, 2, 1, 3, 2, 1, 3, 2, 1],
        )
        .with_pixel_type(3),
        Tile::raw(
            6,
            0,
            2,
            2,
            2,
            2,
            0,
            1,
            &[6, 5, 4, 6, 5, 4, 6, 5, 4, 6, 5, 4],
        )
        .with_pixel_type(3),
        Tile::raw(0, 0, 2, 2, 1, 1, 0, 0, &[3, 2, 1]).with_pixel_type(3),
        Tile::raw(6, 0, 2, 2, 1, 1, 0, 1, &[6, 5, 4]).with_pixel_type(3),
    ];
    let fixture = TemporaryCzi::new("rgb-pyramid-fill", &tiles);
    let dataset = open(fixture.path()).expect("open RGB CZI pyramid");

    let reduced = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()).with_resolution(1))
        .expect("compose RGB pyramid with missing tiles");
    assert_eq!(
        reduced.bytes(),
        [1, 2, 3, 255, 255, 255, 255, 255, 255, 4, 5, 6]
    );
}

#[test]
fn czi_pyramid_canvas_uses_logical_extent_while_decoding_rounded_edge_tiles() {
    let tiles = vec![
        Tile::raw(0, 0, 4, 2, 4, 2, 0, 0, &[11, 12, 13, 14, 21, 22, 23, 24]),
        Tile::raw(4, 0, 3, 2, 3, 2, 0, 1, &[15, 16, 17, 25, 26, 27]),
        // 3 / 2 rounds to the same 2x level. The stored edge tile extends
        // one pixel past the 7 / 2 logical canvas and must be clipped.
        Tile::raw(4, 0, 3, 2, 2, 1, 0, 1, &[3, 4]),
        Tile::raw(0, 0, 4, 2, 2, 1, 0, 0, &[1, 2]),
    ];
    let fixture = TemporaryCzi::new("rounded-edge-pyramid", &tiles);
    let dataset = open(fixture.path()).expect("open rounded-edge CZI pyramid");

    assert_eq!(dataset.series()[0].resolutions()[0].metadata().size_x, 7);
    assert_eq!(dataset.series()[0].resolutions()[1].metadata().size_x, 3);
    let reduced = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()).with_resolution(1))
        .expect("read clipped rounded edge tile");
    assert_eq!(reduced.bytes(), [1, 2, 3]);
}

#[test]
fn czi_pyramid_accepts_a_one_pixel_quantized_edge_axis() {
    let tiles = vec![
        Tile::raw(0, 0, 3, 2, 3, 2, 0, 0, &[1, 2, 3, 4, 5, 6]),
        Tile::raw(3, 0, 1, 2, 1, 2, 0, 1, &[7, 8]),
        Tile::raw(0, 0, 3, 2, 2, 1, 0, 0, &[10, 11]),
        // X cannot shrink below one pixel, while Y still identifies this as
        // a factor-2 edge tile.
        Tile::raw(3, 0, 1, 2, 1, 1, 0, 1, &[20]),
    ];
    let fixture = TemporaryCzi::new("quantized-edge-axis", &tiles);
    let dataset = open(fixture.path()).expect("open quantized-edge CZI pyramid");

    let reduced = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()).with_resolution(1))
        .expect("read quantized edge tile");
    assert_eq!(reduced.bytes(), [10, 20]);
}

#[test]
fn czi_factor_three_layer_reconciles_a_square_edge_that_rounds_to_two() {
    let native_main = vec![1; 100];
    let native_edge = vec![2; 16];
    let tiles = vec![
        Tile::raw(0, 0, 10, 10, 10, 10, 0, 0, &native_main),
        Tile::raw(10, 0, 4, 4, 4, 4, 0, 1, &native_edge),
        Tile::raw(0, 0, 10, 10, 3, 3, 0, 0, &[1, 2, 3, 4, 5, 6, 7, 8, 9]),
        // 4 / 2 rounds to 2 in isolation, but this is an edge tile in the
        // factor-3 layer established by the larger sibling.
        Tile::raw(10, 0, 4, 4, 2, 2, 0, 1, &[41, 42, 43, 44]),
    ];
    let fixture = TemporaryCzi::new("factor-three-square-edge", &tiles);
    let dataset = open(fixture.path()).expect("open quantized factor-three CZI pyramid");

    assert_eq!(dataset.series()[0].resolutions().len(), 2);
    let reduced = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()).with_resolution(1))
        .expect("compose square edge in its sibling's factor-three level");
    assert_eq!(reduced.bytes(), [1, 2, 3, 41, 4, 5, 6, 43, 7, 8, 9, 0]);
}

#[test]
fn czi_ignores_the_legacy_pyramid_marker_when_stored_size_is_native() {
    let mut tile = Tile::raw(0, 0, 2, 2, 2, 2, 0, 0, &[1, 2, 3, 4]);
    tile.pyramid_type = 1;
    let fixture = TemporaryCzi::new("legacy-pyramid-marker", &[tile]);
    let dataset = open(fixture.path()).expect("open legacy-marked native CZI");

    assert_eq!(dataset.series()[0].resolutions().len(), 1);
    assert_eq!(
        dataset
            .read_plane(ReadRequest::new(0, PlaneCoordinates::default()))
            .expect("read native-sized legacy-marked tile")
            .bytes(),
        [1, 2, 3, 4]
    );
}

#[test]
fn czi_reduced_level_keeps_base_canvas_when_an_outer_tile_is_missing() {
    let tiles = vec![
        Tile::raw(
            10,
            20,
            4,
            4,
            4,
            4,
            0,
            0,
            &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
        ),
        Tile::raw(
            16,
            20,
            4,
            4,
            4,
            4,
            0,
            1,
            &[2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
        ),
        // The reduced left tile is absent. Placement must still be relative to
        // the base plane's X=10 origin instead of rebasing this tile to X=0.
        Tile::raw(16, 20, 4, 4, 2, 2, 0, 1, &[7, 8, 9, 10]),
    ];
    let fixture = TemporaryCzi::new("missing-outer-pyramid-tile", &tiles);
    let dataset = open(fixture.path()).expect("open incomplete CZI pyramid");

    assert_eq!(dataset.series()[0].resolutions()[1].metadata().size_x, 5);
    let reduced = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()).with_resolution(1))
        .expect("fill the absent outer tile");
    assert_eq!(reduced.bytes(), [0, 0, 0, 7, 8, 0, 0, 0, 9, 10]);
}

#[test]
fn czi_factor_three_level_fills_an_entire_missing_reduced_plane() {
    let tiles = vec![
        Tile::raw(0, 0, 3, 3, 3, 3, 0, 0, &[1, 2, 3, 4, 5, 6, 7, 8, 9]),
        Tile::raw(
            0,
            0,
            3,
            3,
            3,
            3,
            1,
            0,
            &[11, 12, 13, 14, 15, 16, 17, 18, 19],
        ),
        Tile::raw(0, 0, 3, 3, 1, 1, 0, 0, &[42]),
    ];
    let fixture = TemporaryCzi::new("factor-three-missing-plane", &tiles);
    let mut reader = ImageReader::open(fixture.path()).expect("open thumbnail reader");
    assert_eq!(
        reader
            .open_thumb_bytes(1)
            .expect("fall back from an empty reduced plane to native pixels"),
        [11, 12, 13, 14, 15, 16, 17, 18, 19]
    );
    let dataset = open(fixture.path()).expect("open factor-three CZI pyramid");

    assert_eq!(dataset.series()[0].resolutions().len(), 2);
    assert_eq!(
        (
            dataset.series()[0].resolutions()[1].metadata().size_x,
            dataset.series()[0].resolutions()[1].metadata().size_y,
        ),
        (1, 1)
    );
    let present = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)).with_resolution(1))
        .expect("read present factor-three plane");
    assert_eq!(present.bytes(), [42]);

    let missing = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(1, 0, 0)).with_resolution(1))
        .expect("fill missing factor-three plane");
    assert_eq!(missing.bytes(), [0]);
}

#[test]
fn czi_rejects_inconsistent_pyramid_scales() {
    let mut tiles = pyramid_tiles();
    for tile in &mut tiles {
        if tile.pyramid_type != 0 {
            tile.stored_height = 4;
            tile.pixels.resize(8, 0);
        }
    }
    let fixture = TemporaryCzi::new("bad-pyramid-scale", &tiles);
    assert!(matches!(
        open(fixture.path()),
        Err(BioFormatsError::InvalidData(message))
            | Err(BioFormatsError::UnsupportedFormat(message))
            if message.contains("scale")
    ));
}

fn generated_czi(tiles: &[Tile]) -> Vec<u8> {
    const DIMENSION_COUNT: usize = 6;
    const ENTRY_SIZE: usize = 32 + DIMENSION_COUNT * 20;

    let directory_position = align_segment(SEGMENT_HEADER + FILE_HEADER_BODY);
    let directory_used = DIRECTORY_HEADER + tiles.len() * ENTRY_SIZE;
    let mut subblock_positions = Vec::with_capacity(tiles.len());
    let mut next_position = align_segment(directory_position + SEGMENT_HEADER + directory_used);
    for tile in tiles {
        subblock_positions.push(next_position);
        next_position =
            align_segment(next_position + SEGMENT_HEADER + SUBBLOCK_HEADER + tile.pixels.len());
    }

    let mut bytes = vec![0_u8; next_position];
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
    bytes[directory_body..directory_body + 4].copy_from_slice(&(tiles.len() as i32).to_le_bytes());

    for (index, (tile, subblock_position)) in tiles.iter().zip(subblock_positions).enumerate() {
        let directory_entry = directory_body + DIRECTORY_HEADER + index * ENTRY_SIZE;
        write_directory_entry(
            &mut bytes[directory_entry..directory_entry + ENTRY_SIZE],
            tile,
            subblock_position,
        );

        let subblock_used = SUBBLOCK_HEADER + tile.pixels.len();
        write_segment_header(
            &mut bytes,
            subblock_position,
            b"ZISRAWSUBBLOCK",
            subblock_used as u64,
        );
        let body = subblock_position + SEGMENT_HEADER;
        bytes[body + 8..body + 16].copy_from_slice(&(tile.pixels.len() as u64).to_le_bytes());

        // A CZI SubBlock repeats its directory entry before the 256-byte
        // header padding. Keeping this copy makes the generated fixture
        // readable by the Java reference implementation as well as this port.
        write_directory_entry(
            &mut bytes[body + 16..body + 16 + ENTRY_SIZE],
            tile,
            subblock_position,
        );
        bytes[body + SUBBLOCK_HEADER..body + SUBBLOCK_HEADER + tile.pixels.len()]
            .copy_from_slice(&tile.pixels);
    }

    bytes
}

fn write_directory_entry(bytes: &mut [u8], tile: &Tile, subblock_position: usize) {
    bytes[0..2].copy_from_slice(b"DV");
    bytes[2..6].copy_from_slice(&tile.pixel_type.to_le_bytes());
    bytes[6..14].copy_from_slice(&(subblock_position as i64).to_le_bytes());
    bytes[18..22].copy_from_slice(&tile.compression.to_le_bytes());
    bytes[22] = tile.pyramid_type;
    bytes[28..32].copy_from_slice(&6_i32.to_le_bytes());

    for (index, (name, start, logical_size, stored_size)) in [
        (b"X\0\0\0", tile.x, tile.logical_width, tile.stored_width),
        (b"Y\0\0\0", tile.y, tile.logical_height, tile.stored_height),
        (b"Z\0\0\0", tile.z, 1, 1),
        (b"C\0\0\0", tile.channel, 1, 1),
        (b"T\0\0\0", 0, 1, 1),
        (b"M\0\0\0", tile.mosaic, 1, 1),
    ]
    .into_iter()
    .enumerate()
    {
        let dimension = 32 + index * 20;
        bytes[dimension..dimension + 4].copy_from_slice(name);
        bytes[dimension + 4..dimension + 8].copy_from_slice(&start.to_le_bytes());
        bytes[dimension + 8..dimension + 12].copy_from_slice(&logical_size.to_le_bytes());
        bytes[dimension + 16..dimension + 20].copy_from_slice(&stored_size.to_le_bytes());
    }
}

fn write_segment_header(bytes: &mut [u8], offset: usize, kind: &[u8], used: u64) {
    bytes[offset..offset + kind.len()].copy_from_slice(kind);
    bytes[offset + 16..offset + 24].copy_from_slice(&used.to_le_bytes());
    bytes[offset + 24..offset + 32].copy_from_slice(&used.to_le_bytes());
}

fn align_segment(position: usize) -> usize {
    position
        .checked_add(31)
        .expect("generated CZI position overflow")
        / 32
        * 32
}
