use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bioformats_rs::{
    ChannelFiller, DimensionOrder, FilePattern, FileStitcher, FormatReader, ImageMetadata,
    LookupTable, Memoizer, MinMaxCalculator, MinMaxStore, PixelType,
};

#[derive(Clone)]
struct IndexedReader {
    metadata: ImageMetadata,
    planes: Vec<Vec<u8>>,
}

impl IndexedReader {
    fn new(metadata: ImageMetadata, planes: Vec<Vec<u8>>) -> Self {
        Self { metadata, planes }
    }
}

impl FormatReader for IndexedReader {
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

#[derive(Clone)]
struct GridReader {
    metadata: ImageMetadata,
    planes: Vec<Vec<u8>>,
}

impl GridReader {
    fn new(metadata: ImageMetadata, planes: Vec<Vec<u8>>) -> Self {
        Self { metadata, planes }
    }
}

impl FormatReader for GridReader {
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
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> bioformats_rs::Result<Vec<u8>> {
        let plane = &self.planes[plane_index as usize];
        let row_bytes = self.metadata.size_x as usize;
        let mut out = Vec::with_capacity((w * h) as usize);
        for row in 0..h as usize {
            let start = (y as usize + row) * row_bytes + x as usize;
            out.extend_from_slice(&plane[start..start + w as usize]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> bioformats_rs::Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

#[derive(Clone)]
struct PatternReader {
    metadata: ImageMetadata,
    marker: u8,
}

impl PatternReader {
    fn new() -> Self {
        let mut metadata = ImageMetadata::default();
        metadata.size_x = 1;
        metadata.size_y = 1;
        metadata.size_z = 1;
        metadata.size_c = 1;
        metadata.size_t = 1;
        metadata.image_count = 1;
        metadata.pixel_type = PixelType::Uint8;
        metadata.dimension_order = DimensionOrder::XYZCT;
        Self {
            metadata,
            marker: 0,
        }
    }
}

impl FormatReader for PatternReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|ext| ext.to_str()) == Some("fake")
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> bioformats_rs::Result<()> {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let z = find_axis_digit(name, "Z").unwrap_or(0);
        let t = find_axis_digit(name, "T").unwrap_or(0);
        let c = find_axis_digit(name, "C").unwrap_or(0);
        self.marker = (z * 100 + t * 10 + c) as u8;
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

    fn open_bytes(&mut self, _plane_index: u32) -> bioformats_rs::Result<Vec<u8>> {
        Ok(vec![self.marker])
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

#[derive(Clone)]
struct RecordingStore {
    values: Arc<Mutex<Vec<(usize, f64, f64, usize)>>>,
}

impl RecordingStore {
    fn new() -> Self {
        Self {
            values: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl MinMaxStore for RecordingStore {
    fn set_channel_global_min_max(
        &mut self,
        channel: usize,
        minimum: f64,
        maximum: f64,
        series: usize,
    ) {
        self.values
            .lock()
            .unwrap()
            .push((channel, minimum, maximum, series));
    }
}

#[test]
fn channel_filler_expands_palette_indices() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 2;
    metadata.size_y = 1;
    metadata.size_c = 1;
    metadata.image_count = 1;
    metadata.pixel_type = PixelType::Uint8;
    metadata.is_indexed = true;
    metadata.is_false_color = false;
    metadata.is_interleaved = true;
    metadata.lookup_table = Some(LookupTable {
        red: vec![10, 20],
        green: vec![30, 40],
        blue: vec![50, 60],
    });

    let source = IndexedReader::new(metadata, vec![vec![0, 1]]);
    let mut reader = ChannelFiller::new(source);

    assert!(reader.is_filled());
    assert!(reader.is_rgb());
    assert!(!reader.is_indexed());
    assert_eq!(reader.size_c(), 3);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 30, 50, 20, 40, 60]);
}

#[test]
fn min_max_calculator_tracks_partial_and_full_plane_reads() {
    let mut metadata = ImageMetadata::default();
    metadata.size_x = 2;
    metadata.size_y = 2;
    metadata.size_c = 1;
    metadata.image_count = 1;
    metadata.pixel_type = PixelType::Uint8;

    let source = GridReader::new(metadata, vec![vec![1, 2, 3, 4]]);
    let mut reader = MinMaxCalculator::new(source);
    let store = RecordingStore::new();
    let values = store.values.clone();
    reader.set_min_max_store(Box::new(store));

    reader.open_bytes_region(0, 0, 0, 2, 1).unwrap();
    assert_eq!(reader.get_plane_minimum(0).unwrap(), Some(vec![1.0]));
    assert_eq!(reader.get_plane_maximum(0).unwrap(), Some(vec![2.0]));
    assert_eq!(reader.get_channel_known_minimum(0).unwrap(), Some(1.0));
    assert_eq!(reader.get_channel_known_maximum(0).unwrap(), Some(2.0));
    assert_eq!(reader.get_channel_global_minimum(0).unwrap(), None);
    assert!(!reader.is_min_max_populated());

    reader.open_bytes(0).unwrap();
    assert_eq!(reader.get_plane_minimum(0).unwrap(), Some(vec![1.0]));
    assert_eq!(reader.get_plane_maximum(0).unwrap(), Some(vec![4.0]));
    assert_eq!(reader.get_channel_global_minimum(0).unwrap(), Some(1.0));
    assert_eq!(reader.get_channel_global_maximum(0).unwrap(), Some(4.0));
    assert!(reader.is_min_max_populated());
    assert_eq!(values.lock().unwrap().as_slice(), &[(0, 1.0, 4.0, 0)]);
}

#[test]
fn file_pattern_expands_expected_files() {
    let pattern = FilePattern::parse("/tmp/sample_Z<0-1>T<0-1>C<0-2>.fake").unwrap();
    let files = pattern.files();
    assert_eq!(files.len(), 12);
    assert!(files[0].to_string_lossy().contains("Z0T0C0"));
    assert!(files[11].to_string_lossy().contains("Z1T1C2"));
}

#[test]
fn file_stitcher_maps_planes_across_pattern_axes() {
    let root = temp_dir("file_stitcher");
    for z in 0..2 {
        for t in 0..2 {
            for c in 0..3 {
                let path = root.join(format!("sample_Z{}T{}C{}.fake", z, t, c));
                fs::write(path, []).unwrap();
            }
        }
    }

    let mut reader = FileStitcher::with_reader(PatternReader::new());
    let pattern_path = root.join("sample_Z<0-1>T<0-1>C<0-2>.fake");
    reader.set_id(&pattern_path).unwrap();

    assert_eq!(reader.size_z(), 2);
    assert_eq!(reader.size_t(), 2);
    assert_eq!(reader.size_c(), 3);
    assert_eq!(reader.image_count(), 12);

    let index = reader.get_index(1, 2, 1);
    assert_eq!(reader.open_bytes(index).unwrap(), vec![112]);
}

#[test]
fn memoizer_saves_loads_and_relocates_memo_files() {
    let root = temp_dir("memoizer");
    let image_path = root.join("tiny.tif");
    write_tiny_tiff(&image_path);

    let mut memoizer = Memoizer::with_minimum_elapsed(0);
    memoizer.set_id(&image_path).unwrap();
    let memo_path = memoizer.memo_file().unwrap().to_path_buf();
    assert!(memo_path.exists());
    assert!(!memoizer.is_loaded_from_memo());
    assert!(memoizer.is_saved_to_memo());
    memoizer.close().unwrap();

    memoizer.set_id(&image_path).unwrap();
    assert!(memoizer.is_loaded_from_memo());
    assert!(!memoizer.is_saved_to_memo());
    memoizer.close().unwrap();

    let moved_root = root.with_extension("moved");
    fs::rename(&root, &moved_root).unwrap();
    let moved_image = moved_root.join("tiny.tif");

    memoizer.set_id(&moved_image).unwrap();
    assert!(memoizer.is_loaded_from_memo());
    assert!(!memoizer.is_saved_to_memo());
}

#[test]
fn memoizer_rebuilds_an_incompatible_payload() {
    let root = temp_dir("memoizer_incompatible");
    let image_path = root.join("tiny.tif");
    write_tiny_tiff(&image_path);
    let memo_path = root.join(".tiny.tif.bfmemo");
    fs::write(&memo_path, b"not a compatible memo payload").unwrap();

    let mut rebuilt = Memoizer::with_minimum_elapsed(0);
    rebuilt.set_id(&image_path).unwrap();
    assert!(!rebuilt.is_loaded_from_memo());
    assert!(rebuilt.is_saved_to_memo());
    assert_eq!(rebuilt.open_bytes(0).unwrap(), [7]);

    let mut loaded = Memoizer::with_minimum_elapsed(0);
    loaded.set_id(&image_path).unwrap();
    assert!(loaded.is_loaded_from_memo());
    assert!(!loaded.is_saved_to_memo());
    assert_eq!(loaded.open_bytes(0).unwrap(), [7]);
}

#[test]
fn memoizer_keeps_readers_without_snapshot_support_usable() {
    let root = temp_dir("memoizer_without_snapshot");
    let image_path = root.join("synthetic.fake");
    fs::write(&image_path, []).unwrap();

    let mut metadata = ImageMetadata::default();
    metadata.size_x = 1;
    metadata.size_y = 1;
    let source = GridReader::new(metadata, vec![vec![7]]);
    let mut memoizer = Memoizer::with_config(source, Duration::ZERO, None);

    memoizer.set_id(&image_path).unwrap();
    assert!(!memoizer.is_saved_to_memo());
    assert!(!memoizer.is_loaded_from_memo());
    assert_eq!(memoizer.open_bytes(0).unwrap(), vec![7]);
    assert!(!memoizer.memo_file().unwrap().exists());
}

#[test]
fn memoizer_preserves_detached_dataset_used_files() {
    let root = temp_dir("memoizer_detached_nrrd");
    let raw_path = root.join("pixels.raw");
    let header_path = root.join("image.nhdr");
    fs::write(&raw_path, [1_u8, 2, 3, 4]).unwrap();
    fs::write(
        &header_path,
        b"NRRD0005\ntype: uint8\ndimension: 2\nsizes: 2 2\nencoding: raw\ndata file: pixels.raw\n\n",
    )
    .unwrap();

    let mut memoizer = Memoizer::with_minimum_elapsed(0);
    memoizer.set_id(&header_path).unwrap();

    assert_eq!(memoizer.used_files(), [header_path, raw_path]);
    assert_eq!(memoizer.open_bytes(0).unwrap(), [1, 2, 3, 4]);
    assert!(!memoizer.is_saved_to_memo());
}

#[test]
fn default_resolution_contract_rejects_nonzero_levels() {
    let metadata = ImageMetadata::default();
    let mut reader = GridReader::new(metadata, vec![vec![7]]);
    assert!(matches!(
        reader.set_resolution(1),
        Err(bioformats_rs::BioFormatsError::ResolutionOutOfRange {
            series: 0,
            resolution: 1
        })
    ));
}

fn find_axis_digit(name: &str, axis: &str) -> Option<u32> {
    let start = name.find(axis)? + axis.len();
    let digits: String = name[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn temp_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("bioformats_rs_{}_{}", label, unique));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_tiny_tiff(path: &Path) {
    let ifd_offset = 8u32;
    let entry_count = 9u16;
    let pixel_offset = ifd_offset as usize + 2 + entry_count as usize * 12 + 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&ifd_offset.to_le_bytes());
    bytes.extend_from_slice(&entry_count.to_le_bytes());
    bytes.extend_from_slice(&ifd_entry(256, 4, 1, 1));
    bytes.extend_from_slice(&ifd_entry(257, 4, 1, 1));
    bytes.extend_from_slice(&ifd_entry(258, 3, 1, 8));
    bytes.extend_from_slice(&ifd_entry(259, 3, 1, 1));
    bytes.extend_from_slice(&ifd_entry(262, 3, 1, 1));
    bytes.extend_from_slice(&ifd_entry(273, 4, 1, pixel_offset as u32));
    bytes.extend_from_slice(&ifd_entry(277, 3, 1, 1));
    bytes.extend_from_slice(&ifd_entry(278, 4, 1, 1));
    bytes.extend_from_slice(&ifd_entry(279, 4, 1, 1));
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.push(7);
    fs::write(path, bytes).unwrap();
}

fn ifd_entry(tag: u16, field_type: u16, count: u32, value: u32) -> [u8; 12] {
    let mut entry = [0u8; 12];
    entry[0..2].copy_from_slice(&tag.to_le_bytes());
    entry[2..4].copy_from_slice(&field_type.to_le_bytes());
    entry[4..8].copy_from_slice(&count.to_le_bytes());
    entry[8..12].copy_from_slice(&value.to_le_bytes());
    entry
}
