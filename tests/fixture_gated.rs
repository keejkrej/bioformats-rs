use std::path::{Path, PathBuf};

use bioformats_rs::ImageReader;

fn fixture_path(format: &str, env_key: &str, default_name: &str) -> Option<PathBuf> {
    if let Ok(value) = std::env::var(env_key) {
        let path = PathBuf::from(value);
        if path.exists() {
            return Some(path);
        }
    }

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(format)
        .join(default_name);

    path.exists().then_some(path)
}

#[test]
#[ignore = "requires local TIFF fixture at tests/data/tiff/sample.tif or BIOFORMATS_RS_TIFF_FIXTURE"]
fn opens_local_tiff_fixture() {
    let path = fixture_path("tiff", "BIOFORMATS_RS_TIFF_FIXTURE", "sample.tif").unwrap();
    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(meta.size_x > 0);
    assert!(meta.size_y > 0);
    assert!(meta.image_count > 0);
    assert!(!reader.open_bytes(0).unwrap().is_empty());
}

#[test]
#[ignore = "requires local ND2 fixture at tests/data/nd2/sample.nd2 or BIOFORMATS_RS_ND2_FIXTURE"]
fn opens_local_nd2_fixture() {
    let path = fixture_path("nd2", "BIOFORMATS_RS_ND2_FIXTURE", "sample.nd2").unwrap();
    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(meta.size_x > 0);
    assert!(meta.size_y > 0);
    assert!(meta.image_count > 0);
    assert!(!reader.open_bytes(0).unwrap().is_empty());
}

#[test]
#[ignore = "requires local CZI fixture at tests/data/czi/sample.czi or BIOFORMATS_RS_CZI_FIXTURE"]
fn opens_local_czi_fixture() {
    let path = fixture_path("czi", "BIOFORMATS_RS_CZI_FIXTURE", "sample.czi").unwrap();
    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(meta.size_x > 0);
    assert!(meta.size_y > 0);
    assert!(meta.image_count > 0);
    assert!(!reader.open_bytes(0).unwrap().is_empty());
}
