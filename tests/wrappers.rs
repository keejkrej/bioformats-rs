use std::path::Path;

use bioformats_rs::{
    ChannelFiller, ChannelMerger, ChannelSeparator, DimensionOrder, DimensionSwapper, FileStitcher,
    FormatReader, ImageMetadata, LookupTable, MetadataValue, MinMaxCalculator, ReaderWrapper,
};

#[derive(Clone)]
struct SyntheticReader {
    metadata: ImageMetadata,
    planes: Vec<Vec<u8>>,
}

impl SyntheticReader {
    fn new(metadata: ImageMetadata, planes: Vec<Vec<u8>>) -> Self {
        Self { metadata, planes }
    }
}

impl FormatReader for SyntheticReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        false
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> bioformats_rs::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> bioformats_rs::Result<()> {
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, series: usize) -> bioformats_rs::Result<()> {
        assert_eq!(series, 0);
        Ok(())
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        &self.metadata
    }

    fn open_bytes(&mut self, plane_index: u32) -> bioformats_rs::Result<Vec<u8>> {
        Ok(self.planes[plane_index as usize].clone())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> bioformats_rs::Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> bioformats_rs::Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn clone_boxed(&self) -> bioformats_rs::Result<Box<dyn FormatReader>> {
        Ok(Box::new(self.clone()))
    }
}

#[test]
fn metadata_index_round_trip_uses_effective_channels() {
    let mut metadata = ImageMetadata::default();
    metadata.size_z = 2;
    metadata.size_c = 6;
    metadata.size_t = 4;
    metadata.image_count = 16;
    metadata.samples_per_pixel = 3;
    metadata.is_rgb = true;
    metadata.dimension_order = DimensionOrder::XYZCT;

    assert_eq!(metadata.effective_size_c(), 2);
    assert_eq!(metadata.rgb_channel_count(), 3);

    let index = metadata.get_index(1, 1, 3);
    assert_eq!(metadata.get_zct_coords(index), (1, 1, 3));
}

#[test]
fn channel_separator_splits_rgb_planes() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_z = 1;
    metadata.size_c = 3;
    metadata.size_t = 1;
    metadata.image_count = 1;
    metadata.samples_per_pixel = 3;
    metadata.is_rgb = true;
    metadata.is_interleaved = true;
    metadata.dimension_order = DimensionOrder::XYZTC;

    let source = SyntheticReader::new(metadata, vec![vec![10, 20, 30]]);
    let mut reader = ChannelSeparator::new(source);

    assert_eq!(reader.image_count(), 3);
    assert_eq!(reader.size_c(), 3);
    assert_eq!(reader.metadata().samples_per_pixel, 1);
    assert_eq!(reader.dimension_order(), DimensionOrder::XYCZT);
    assert!(!reader.is_rgb());
    assert_eq!(reader.get_original_index(0), 0);
    assert_eq!(reader.get_original_index(1), 0);
    assert_eq!(reader.get_original_index(2), 0);

    assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![20]);
    assert_eq!(reader.open_bytes(2).unwrap(), vec![30]);
}

#[test]
fn channel_separator_splits_planar_rgb_planes() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 2;
    metadata.size_y = 1;
    metadata.size_c = 3;
    metadata.image_count = 1;
    metadata.samples_per_pixel = 3;
    metadata.is_rgb = true;
    metadata.is_interleaved = false;

    let source = SyntheticReader::new(metadata, vec![vec![1, 2, 10, 20, 100, 200]]);
    let mut reader = ChannelSeparator::new(source);

    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![10, 20]);
    assert_eq!(reader.open_bytes(2).unwrap(), vec![100, 200]);
}

#[test]
fn channel_separator_moves_channel_axis_first_and_preserves_zt_order() {
    for (source_order, expected_order) in [
        (DimensionOrder::XYCTZ, DimensionOrder::XYCTZ),
        (DimensionOrder::XYCZT, DimensionOrder::XYCZT),
        (DimensionOrder::XYTCZ, DimensionOrder::XYCTZ),
        (DimensionOrder::XYTZC, DimensionOrder::XYCTZ),
        (DimensionOrder::XYZCT, DimensionOrder::XYCZT),
        (DimensionOrder::XYZTC, DimensionOrder::XYCZT),
    ] {
        let mut metadata = ImageMetadata::default();
        metadata.size_x = 1;
        metadata.size_y = 1;
        metadata.size_z = 2;
        metadata.size_c = 3;
        metadata.size_t = 2;
        metadata.image_count = 4;
        metadata.samples_per_pixel = 3;
        metadata.is_rgb = true;
        metadata.is_interleaved = true;
        metadata.dimension_order = source_order;

        let planes = (0_u8..4)
            .map(|plane| vec![plane * 10 + 1, plane * 10 + 2, plane * 10 + 3])
            .collect::<Vec<_>>();
        let source = SyntheticReader::new(metadata.clone(), planes.clone());
        let mut reader = ChannelSeparator::new(source);
        assert_eq!(reader.dimension_order(), expected_order);

        for z in 0..2 {
            for t in 0..2 {
                let source_index = metadata.get_index(z, 0, t);
                for c in 0..3 {
                    let separated_index = reader.get_index(z, c, t);
                    assert_eq!(reader.get_original_index(separated_index), source_index);
                    assert_eq!(
                        reader.open_bytes(separated_index).unwrap(),
                        vec![planes[source_index as usize][c as usize]]
                    );
                }
            }
        }
    }
}

#[test]
fn channel_merger_merges_planes_into_rgb_plane() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_z = 1;
    metadata.size_c = 3;
    metadata.size_t = 1;
    metadata.image_count = 3;
    metadata.is_rgb = false;
    metadata.dimension_order = DimensionOrder::XYZCT;

    let source = SyntheticReader::new(metadata, vec![vec![10], vec![20], vec![30]]);
    let mut reader = ChannelMerger::new(source);

    assert_eq!(reader.image_count(), 1);
    assert_eq!(reader.size_c(), 3);
    assert_eq!(reader.metadata().samples_per_pixel, 3);
    assert_eq!(reader.dimension_order(), DimensionOrder::XYCZT);
    assert!(reader.is_rgb());
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 20, 30]);
}

#[test]
fn channel_merger_moves_channel_axis_first_and_preserves_zt_order() {
    for (source_order, expected_order) in [
        (DimensionOrder::XYCTZ, DimensionOrder::XYCTZ),
        (DimensionOrder::XYCZT, DimensionOrder::XYCZT),
        (DimensionOrder::XYTCZ, DimensionOrder::XYCTZ),
        (DimensionOrder::XYTZC, DimensionOrder::XYCTZ),
        (DimensionOrder::XYZCT, DimensionOrder::XYCZT),
        (DimensionOrder::XYZTC, DimensionOrder::XYCZT),
    ] {
        let mut metadata = ImageMetadata::default();
        metadata.size_x = 1;
        metadata.size_y = 1;
        metadata.size_c = 3;
        metadata.image_count = 3;
        metadata.dimension_order = source_order;

        let source = SyntheticReader::new(metadata, vec![vec![10], vec![20], vec![30]]);
        let reader = ChannelMerger::new(source);
        assert_eq!(reader.dimension_order(), expected_order);
    }
}

#[test]
fn channel_merger_passes_through_non_rgb_multi_sample_planes() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_z = 1;
    metadata.size_c = 4;
    metadata.size_t = 1;
    metadata.image_count = 2;
    metadata.samples_per_pixel = 2;
    metadata.is_rgb = false;
    metadata.is_interleaved = true;

    let source = SyntheticReader::new(metadata, vec![vec![10, 20], vec![30, 40]]);
    let mut reader = ChannelMerger::new(source);

    assert_eq!(reader.image_count(), 2);
    assert_eq!(reader.size_c(), 4);
    assert_eq!(reader.metadata().samples_per_pixel, 2);
    assert!(!reader.is_rgb());
    assert_eq!(reader.open_bytes(1).unwrap(), vec![30, 40]);
}

#[test]
fn channel_filler_exposes_lookup_table_component_count() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 2;
    metadata.size_y = 1;
    metadata.size_c = 1;
    metadata.image_count = 1;
    metadata.is_indexed = true;
    metadata.is_false_color = false;
    metadata.is_interleaved = true;
    metadata.lookup_table = Some(LookupTable {
        red: vec![10, 20],
        green: vec![30, 40],
        blue: vec![50, 60],
    });

    let source = SyntheticReader::new(metadata, vec![vec![0, 1]]);
    let mut reader = ChannelFiller::new(source);

    assert_eq!(reader.metadata().samples_per_pixel, 3);
    assert_eq!(reader.size_c(), 3);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 30, 50, 20, 40, 60]);
}

#[test]
fn pass_through_wrappers_preserve_non_rgb_sample_count() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_c = 2;
    metadata.image_count = 1;
    metadata.samples_per_pixel = 2;
    metadata.is_rgb = false;
    metadata.is_interleaved = true;

    let source = SyntheticReader::new(metadata, vec![vec![10, 20]]);
    let wrapper = ReaderWrapper::new(source);
    assert_eq!(wrapper.metadata().samples_per_pixel, 2);

    let separator = ChannelSeparator::new(wrapper);
    assert_eq!(separator.metadata().samples_per_pixel, 2);
    assert_eq!(separator.image_count(), 1);

    let filler = ChannelFiller::new(separator);
    assert_eq!(filler.metadata().samples_per_pixel, 2);
    assert_eq!(filler.image_count(), 1);

    let swapper = DimensionSwapper::new(filler);
    assert_eq!(swapper.metadata().samples_per_pixel, 2);
}

#[test]
fn min_max_tracks_non_rgb_sample_components() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 2;
    metadata.size_y = 1;
    metadata.size_c = 2;
    metadata.image_count = 1;
    metadata.samples_per_pixel = 2;
    metadata.is_rgb = false;
    metadata.is_interleaved = true;

    let source = SyntheticReader::new(metadata, vec![vec![10, 100, 20, 80]]);
    let mut reader = MinMaxCalculator::new(source);
    reader.open_bytes(0).unwrap();

    assert_eq!(reader.get_plane_minimum(0).unwrap(), Some(vec![10.0, 80.0]));
    assert_eq!(
        reader.get_plane_maximum(0).unwrap(),
        Some(vec![20.0, 100.0])
    );
    assert_eq!(reader.get_channel_global_minimum(0).unwrap(), Some(10.0));
    assert_eq!(reader.get_channel_global_minimum(1).unwrap(), Some(80.0));
}

#[test]
fn file_stitcher_preserves_samples_while_extending_logical_channels() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_c = 2;
    metadata.image_count = 1;
    metadata.samples_per_pixel = 2;
    metadata.is_rgb = false;
    metadata.is_interleaved = true;

    let source = SyntheticReader::new(metadata, vec![vec![10, 20]]);
    let mut reader = FileStitcher::with_reader(source);
    reader
        .set_id(Path::new("/tmp/bioformats-rs-wrapper-C<0-1>.fake"))
        .unwrap();

    assert_eq!(reader.metadata().samples_per_pixel, 2);
    assert_eq!(reader.size_c(), 4);
    assert_eq!(reader.image_count(), 2);
}

#[test]
fn dimension_swapper_changes_output_plane_order() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_z = 2;
    metadata.size_c = 3;
    metadata.size_t = 4;
    metadata.image_count = 24;
    metadata.dimension_order = DimensionOrder::XYZCT;
    metadata
        .series_metadata
        .insert("kind".into(), MetadataValue::String("synthetic".into()));

    let planes: Vec<Vec<u8>> = (0..24).map(|index| vec![index as u8]).collect();
    let source = SyntheticReader::new(metadata.clone(), planes);
    let mut reader = DimensionSwapper::new(source);
    reader.set_output_order(DimensionOrder::XYCZT);

    let wrapper_index = reader.get_index(1, 2, 3);
    let expected_source_index = metadata.get_index(1, 2, 3);

    assert_eq!(
        reader.open_bytes(wrapper_index).unwrap(),
        vec![expected_source_index as u8]
    );
}

#[test]
fn dimension_swapper_reinterprets_input_order() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_z = 2;
    metadata.size_c = 3;
    metadata.size_t = 4;
    metadata.image_count = 24;
    metadata.dimension_order = DimensionOrder::XYZCT;

    let planes: Vec<Vec<u8>> = (0..24).map(|index| vec![index as u8]).collect();
    let source = SyntheticReader::new(metadata, planes);
    let mut reader = DimensionSwapper::new(source);
    reader.swap_dimensions(DimensionOrder::XYTCZ).unwrap();

    assert_eq!(reader.metadata().size_z, 4);
    assert_eq!(reader.metadata().size_c, 3);
    assert_eq!(reader.metadata().size_t, 2);
    assert_eq!(reader.input_order(), DimensionOrder::XYTCZ);
    assert_eq!(reader.output_order(), DimensionOrder::XYZCT);

    let index = reader.get_index(3, 2, 1);
    let mut source_space = reader.metadata().clone();
    source_space.dimension_order = reader.input_order();
    let expected_source_index = source_space.get_index(3, 2, 1);

    assert_eq!(
        reader.open_bytes(index).unwrap(),
        vec![expected_source_index as u8]
    );
}

#[test]
fn dimension_swapper_rejects_moving_the_channel_axis_for_multi_sample_pixels() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    metadata.size_z = 2;
    metadata.size_c = 3;
    metadata.size_t = 4;
    metadata.image_count = 8;
    metadata.samples_per_pixel = 3;
    metadata.is_rgb = true;
    metadata.dimension_order = DimensionOrder::XYZCT;

    let source = SyntheticReader::new(metadata, vec![vec![0]; 8]);
    let mut reader = DimensionSwapper::new(source);
    let error = reader
        .swap_dimensions(DimensionOrder::XYCZT)
        .expect_err("moving C must be rejected for multi-sample pixels");

    assert!(matches!(
        error,
        bioformats_rs::BioFormatsError::InvalidData(_)
    ));
    assert_eq!(reader.input_order(), DimensionOrder::XYZCT);
    assert_eq!(reader.dimension_order(), DimensionOrder::XYZCT);
    assert_eq!(
        (reader.size_z(), reader.size_c(), reader.size_t()),
        (2, 3, 4)
    );
}
