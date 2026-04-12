use std::path::{Path, PathBuf};
use std::process::Command;

use bioformats_rs::ImageReader;
use roxmltree::{Document, Node};

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

fn bioformats_root() -> PathBuf {
    std::env::var("BIOFORMATS_RS_BIOFORMATS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("bioformats")
        })
}

fn java_ome_xml(path: &Path) -> String {
    let root = bioformats_root();
    let showinf = root.join("tools").join("showinf");
    assert!(
        showinf.exists(),
        "showinf script not found at {}",
        showinf.display()
    );

    let output = Command::new("bash")
        .arg(showinf)
        .arg("-nopix")
        .arg("-omexml-only")
        .arg(path)
        .current_dir(root)
        .output()
        .expect("failed to launch Bio-Formats showinf");

    assert!(
        output.status.success(),
        "showinf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("showinf returned non-UTF8 output")
}

#[derive(Debug)]
struct ExpectedSeries {
    size_z: u32,
    size_c: u32,
    size_t: u32,
    physical_size_x_um: Option<f64>,
    physical_size_y_um: Option<f64>,
    physical_size_z_um: Option<f64>,
    channel_names: Vec<String>,
    emission_wavelengths: Vec<f64>,
}

fn child_text(node: Node<'_, '_>, child_name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == child_name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn parse_expected_ome(xml: &str) -> Vec<ExpectedSeries> {
    let document = Document::parse(xml).expect("invalid OME-XML from Java Bio-Formats");
    document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "Image")
        .filter_map(|image| {
            let pixels = image
                .children()
                .find(|node| node.is_element() && node.tag_name().name() == "Pixels")?;
            let size_z = pixels.attribute("SizeZ")?.parse().ok()?;
            let size_c = pixels.attribute("SizeC")?.parse().ok()?;
            let size_t = pixels.attribute("SizeT")?.parse().ok()?;
            let physical_size_x_um = pixels
                .attribute("PhysicalSizeX")
                .and_then(|value| value.parse::<f64>().ok());
            let physical_size_y_um = pixels
                .attribute("PhysicalSizeY")
                .and_then(|value| value.parse::<f64>().ok());
            let physical_size_z_um = pixels
                .attribute("PhysicalSizeZ")
                .and_then(|value| value.parse::<f64>().ok());
            let channel_names = pixels
                .children()
                .filter(|node| node.is_element() && node.tag_name().name() == "Channel")
                .filter_map(|channel| channel.attribute("Name").map(str::to_owned))
                .collect::<Vec<_>>();
            let emission_wavelengths = pixels
                .children()
                .filter(|node| node.is_element() && node.tag_name().name() == "Channel")
                .filter_map(|channel| {
                    channel
                        .attribute("EmissionWavelength")
                        .and_then(|value| value.parse::<f64>().ok())
                        .or_else(|| {
                            child_text(channel, "EmissionWavelength")
                                .and_then(|value| value.parse::<f64>().ok())
                        })
                })
                .collect::<Vec<_>>();
            Some(ExpectedSeries {
                size_z,
                size_c,
                size_t,
                physical_size_x_um,
                physical_size_y_um,
                physical_size_z_um,
                channel_names,
                emission_wavelengths,
            })
        })
        .collect()
}

fn assert_close(left: Option<f64>, right: Option<f64>, field: &str) {
    match (left, right) {
        (Some(left), Some(right)) => {
            let delta = (left - right).abs();
            assert!(delta < 1e-6, "{field} mismatch: {left} vs {right}");
        }
        (None, None) => {}
        (left, right) => panic!("{field} mismatch: {:?} vs {:?}", left, right),
    }
}

fn compare_reader_to_java(path: &Path) {
    let expected = parse_expected_ome(&java_ome_xml(path));
    let mut reader = ImageReader::open(path).unwrap();
    assert_eq!(reader.series_count(), expected.len());

    for (series_index, expected_series) in expected.iter().enumerate() {
        reader.set_series(series_index).unwrap();
        let metadata = reader.metadata();
        assert_eq!(metadata.size_z, expected_series.size_z);
        assert_eq!(metadata.logical_channel_count(), expected_series.size_c);
        assert_eq!(metadata.size_t, expected_series.size_t);
        assert_close(
            metadata.physical_size_x_um,
            expected_series.physical_size_x_um,
            "PhysicalSizeX",
        );
        assert_close(
            metadata.physical_size_y_um,
            expected_series.physical_size_y_um,
            "PhysicalSizeY",
        );
        assert_close(
            metadata.physical_size_z_um,
            expected_series.physical_size_z_um,
            "PhysicalSizeZ",
        );

        if !expected_series.channel_names.is_empty() {
            let actual = metadata
                .channel_metadata
                .iter()
                .filter_map(|channel| channel.name.clone())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected_series.channel_names);
        }

        if !expected_series.emission_wavelengths.is_empty() {
            let actual = metadata
                .channel_metadata
                .iter()
                .filter_map(|channel| channel.emission_wavelength_nm)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected_series.emission_wavelengths);
        }
    }
}

#[test]
#[ignore = "requires local ND2 fixture plus Java Bio-Formats checkout with showinf"]
fn nd2_metadata_matches_java_ome() {
    let path = fixture_path("nd2", "BIOFORMATS_RS_ND2_FIXTURE", "sample.nd2").unwrap();
    compare_reader_to_java(&path);
}

#[test]
#[ignore = "requires local CZI fixture plus Java Bio-Formats checkout with showinf"]
fn czi_metadata_matches_java_ome() {
    let path = fixture_path("czi", "BIOFORMATS_RS_CZI_FIXTURE", "sample.czi").unwrap();
    compare_reader_to_java(&path);
}
