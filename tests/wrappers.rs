use std::path::Path;

use bioformats_rs::{
    ChannelMerger, ChannelSeparator, DimensionOrder, DimensionSwapper, FormatReader, ImageMetadata,
    MetadataValue,
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
}

#[test]
fn metadata_index_round_trip_uses_effective_channels() {
    let mut metadata = ImageMetadata::default();
    metadata.size_z = 2;
    metadata.size_c = 6;
    metadata.size_t = 4;
    metadata.image_count = 16;
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
    metadata.is_rgb = true;
    metadata.is_interleaved = true;
    metadata.dimension_order = DimensionOrder::XYZTC;

    let source = SyntheticReader::new(metadata, vec![vec![10, 20, 30]]);
    let mut reader = ChannelSeparator::new(source);

    assert_eq!(reader.image_count(), 3);
    assert_eq!(reader.size_c(), 3);
    assert!(!reader.is_rgb());
    assert_eq!(reader.get_original_index(0), 0);
    assert_eq!(reader.get_original_index(1), 0);
    assert_eq!(reader.get_original_index(2), 0);

    assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![20]);
    assert_eq!(reader.open_bytes(2).unwrap(), vec![30]);
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
    assert!(reader.is_rgb());
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 20, 30]);
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
    reader.swap_dimensions(DimensionOrder::XYTCZ);

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
