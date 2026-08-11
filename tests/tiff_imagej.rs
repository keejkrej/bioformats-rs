use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats_rs::{
    open, DimensionOrder, ImageReader, MetadataValue, PlaneCoordinates, ReadRequest, Rect, Region,
};
use roxmltree::Document;

const WIDTH: u32 = 3;
const HEIGHT: u32 = 2;

struct TempTiff {
    path: PathBuf,
}

impl TempTiff {
    fn new(name: &str, pages: &[Page], calibration: Option<Calibration>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bioformats_{name}_{nanos}.tif"));
        write_stack(&path, pages, calibration);
        Self { path }
    }
}

impl Drop for TempTiff {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct Page {
    width: u32,
    height: u32,
    samples: u16,
    pixels: Vec<u8>,
    description: Option<String>,
    new_subfile_type: Option<u32>,
}

#[derive(Clone, Copy)]
struct Calibration {
    x_resolution: (u32, u32),
    y_resolution: (u32, u32),
    resolution_unit: u16,
}

fn pages(count: usize, samples: u16) -> Vec<Page> {
    (0..count)
        .map(|index| Page {
            width: WIDTH,
            height: HEIGHT,
            samples,
            pixels: plane(index, samples),
            description: None,
            new_subfile_type: None,
        })
        .collect()
}

fn plane(index: usize, samples: u16) -> Vec<u8> {
    let len = WIDTH as usize * HEIGHT as usize * usize::from(samples);
    let base = index * 18;
    (0..len).map(|offset| (base + offset + 1) as u8).collect()
}

fn write_stack(path: &Path, pages: &[Page], calibration: Option<Calibration>) {
    assert!(!pages.is_empty());
    for page in pages {
        assert!(matches!(page.samples, 1 | 3));
        assert_eq!(
            page.pixels.len(),
            page.width as usize * page.height as usize * usize::from(page.samples)
        );
    }

    let mut next_offset = 8_u32;
    let ifd_offsets = pages
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let offset = next_offset;
            let tags = 9_u32
                + u32::from(page.samples == 3)
                + u32::from(page.description.is_some())
                + u32::from(page.new_subfile_type.is_some())
                + if index == 0 && calibration.is_some() {
                    3
                } else {
                    0
                };
            next_offset += 2 + tags * 12 + 4;
            offset
        })
        .collect::<Vec<_>>();

    let bits_offsets = pages
        .iter()
        .map(|page| {
            (page.samples == 3).then(|| {
                let offset = next_offset;
                next_offset += 6;
                offset
            })
        })
        .collect::<Vec<_>>();
    let description_offsets = pages
        .iter()
        .map(|page| {
            page.description.as_ref().map(|description| {
                let offset = next_offset;
                next_offset += description.len() as u32 + 1;
                offset
            })
        })
        .collect::<Vec<_>>();
    let x_resolution_offset = calibration.map(|_| {
        let offset = next_offset;
        next_offset += 8;
        offset
    });
    let y_resolution_offset = calibration.map(|_| {
        let offset = next_offset;
        next_offset += 8;
        offset
    });
    let pixel_offsets = pages
        .iter()
        .map(|page| {
            let offset = next_offset;
            next_offset += page.pixels.len() as u32;
            offset
        })
        .collect::<Vec<_>>();

    let mut bytes = Vec::with_capacity(next_offset as usize);
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&ifd_offsets[0].to_le_bytes());

    for (index, page) in pages.iter().enumerate() {
        assert_eq!(bytes.len(), ifd_offsets[index] as usize);
        let tag_count = 9_u16
            + u16::from(page.samples == 3)
            + u16::from(page.description.is_some())
            + u16::from(page.new_subfile_type.is_some())
            + if index == 0 && calibration.is_some() {
                3
            } else {
                0
            };
        bytes.extend_from_slice(&tag_count.to_le_bytes());
        if let Some(new_subfile_type) = page.new_subfile_type {
            push_tag(&mut bytes, 254, 4, 1, new_subfile_type);
        }
        push_tag(&mut bytes, 256, 4, 1, page.width);
        push_tag(&mut bytes, 257, 4, 1, page.height);
        if page.samples == 3 {
            push_tag(&mut bytes, 258, 3, 3, bits_offsets[index].unwrap());
        } else {
            push_tag(&mut bytes, 258, 3, 1, 8);
        }
        push_tag(&mut bytes, 259, 3, 1, 1);
        push_tag(&mut bytes, 262, 3, 1, if page.samples == 3 { 2 } else { 1 });
        if let Some(description) = &page.description {
            push_tag(
                &mut bytes,
                270,
                2,
                description.len() as u32 + 1,
                description_offsets[index].unwrap(),
            );
        }
        push_tag(&mut bytes, 273, 4, 1, pixel_offsets[index]);
        push_tag(&mut bytes, 277, 3, 1, u32::from(page.samples));
        push_tag(&mut bytes, 278, 4, 1, page.height);
        push_tag(&mut bytes, 279, 4, 1, page.pixels.len() as u32);
        if index == 0 {
            if let Some(calibration) = calibration {
                push_tag(&mut bytes, 282, 5, 1, x_resolution_offset.unwrap());
                push_tag(&mut bytes, 283, 5, 1, y_resolution_offset.unwrap());
                if page.samples == 3 {
                    push_tag(&mut bytes, 284, 3, 1, 1);
                }
                push_tag(
                    &mut bytes,
                    296,
                    3,
                    1,
                    u32::from(calibration.resolution_unit),
                );
            } else if page.samples == 3 {
                push_tag(&mut bytes, 284, 3, 1, 1);
            }
        } else if page.samples == 3 {
            push_tag(&mut bytes, 284, 3, 1, 1);
        }
        let next_ifd = ifd_offsets.get(index + 1).copied().unwrap_or(0);
        bytes.extend_from_slice(&next_ifd.to_le_bytes());
    }

    for offset in bits_offsets.into_iter().flatten() {
        assert_eq!(bytes.len(), offset as usize);
        for _ in 0..3 {
            bytes.extend_from_slice(&8_u16.to_le_bytes());
        }
    }
    for (index, page) in pages.iter().enumerate() {
        if let Some(description) = &page.description {
            assert_eq!(bytes.len(), description_offsets[index].unwrap() as usize);
            bytes.extend_from_slice(description.as_bytes());
            bytes.push(0);
        }
    }
    if let Some(calibration) = calibration {
        assert_eq!(bytes.len(), x_resolution_offset.unwrap() as usize);
        bytes.extend_from_slice(&calibration.x_resolution.0.to_le_bytes());
        bytes.extend_from_slice(&calibration.x_resolution.1.to_le_bytes());
        assert_eq!(bytes.len(), y_resolution_offset.unwrap() as usize);
        bytes.extend_from_slice(&calibration.y_resolution.0.to_le_bytes());
        bytes.extend_from_slice(&calibration.y_resolution.1.to_le_bytes());
    }
    for (index, page) in pages.iter().enumerate() {
        assert_eq!(bytes.len(), pixel_offsets[index] as usize);
        bytes.extend_from_slice(&page.pixels);
    }
    assert_eq!(bytes.len(), next_offset as usize);
    fs::write(path, bytes).expect("write generated TIFF stack");
}

fn push_tag(bytes: &mut Vec<u8>, tag: u16, field_type: u16, count: u32, value: u32) {
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&field_type.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn assert_float_metadata(metadata: &bioformats_rs::ImageMetadata, key: &str, expected: f64) {
    match metadata.series_metadata.get(key) {
        Some(MetadataValue::Float(value)) => {
            assert!(
                (value - expected).abs() < 1e-12,
                "{key}: {value} != {expected}"
            );
        }
        value => panic!("expected floating-point {key} metadata, got {value:?}"),
    }
}

#[test]
fn generic_multipage_tiff_defaults_to_a_time_series() {
    let fixture = TempTiff::new("generic_time_stack", &pages(4, 1), None);
    let mut reader = ImageReader::open(&fixture.path).expect("open generic TIFF stack");
    let metadata = reader.metadata();

    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (1, 1, 4)
    );
    assert_eq!(metadata.image_count, 4);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);
    assert_eq!(reader.open_bytes(2).unwrap(), plane(2, 1));

    let dataset = open(&fixture.path).expect("open generic TIFF dataset");
    let region = dataset
        .read_plane(
            ReadRequest::new(0, PlaneCoordinates::new(0, 0, 2))
                .with_region(Region::Rect(Rect::new(1, 0, 2, 2).expect("valid region"))),
        )
        .expect("read timepoint region");
    assert_eq!(region.bytes(), &[38, 39, 41, 42]);
}

#[test]
fn imagej_hyperstack_maps_czt_and_calibration_like_java() {
    let description = "ImageJ=1.54f\nimages=12\nchannels=2\nslices=3\nframes=2\nhyperstack=true\nmode=grayscale\nunit=nm\nspacing=-1.5\nfinterval=2.5\nxorigin=3\nyorigin=4\ncustom=kept\n";
    let mut stack = pages(12, 1);
    stack[0].description = Some(description.to_string());
    let fixture = TempTiff::new(
        "imagej_hyperstack",
        &stack,
        Some(Calibration {
            x_resolution: (4, 1),
            y_resolution: (2, 1),
            resolution_unit: 1,
        }),
    );

    let mut reader = ImageReader::open(&fixture.path).expect("open ImageJ TIFF");
    let metadata = reader.metadata();
    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (3, 2, 2)
    );
    assert_eq!(metadata.image_count, 12);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);
    assert_eq!(metadata.physical_size_x_um, Some(0.00025));
    assert_eq!(metadata.physical_size_y_um, Some(0.0005));
    assert_eq!(metadata.physical_size_z_um, Some(1.5));
    assert_eq!(metadata.time_increment_seconds, Some(2.5));
    assert!(matches!(
        metadata.series_metadata.get("ImageJ"),
        Some(MetadataValue::String(value)) if value == "1.54f"
    ));
    assert!(matches!(
        metadata.series_metadata.get("Unit"),
        Some(MetadataValue::String(value)) if value == "nm"
    ));
    assert!(matches!(
        metadata.series_metadata.get("Color mode"),
        Some(MetadataValue::String(value)) if value == "grayscale"
    ));
    assert!(matches!(
        metadata.series_metadata.get("custom"),
        Some(MetadataValue::String(value)) if value == "kept"
    ));
    assert_float_metadata(metadata, "Spacing", -1.5);
    assert_float_metadata(metadata, "Frame Interval", 2.5);
    assert!(matches!(
        metadata.series_metadata.get("X Origin"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        metadata.series_metadata.get("Y Origin"),
        Some(MetadataValue::Int(4))
    ));

    assert_eq!(reader.open_bytes(11).unwrap(), plane(11, 1));
    let dataset = open(&fixture.path).expect("open ImageJ dataset");
    let coordinate_plane = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(1, 0, 1)))
        .expect("read C-fastest ImageJ coordinate");
    assert_eq!(coordinate_plane.bytes(), plane(8, 1));
}

#[test]
fn imagej_comment_on_last_ifd_is_used_and_mismatched_axes_fall_back_to_time() {
    let mut stack = pages(4, 1);
    stack[3].description = Some(
        "ImageJ=1.53\nimages=4\nchannels=2\nslices=3\nframes=1\nspacing=2.25\nfinterval=0.75\n"
            .to_string(),
    );
    let fixture = TempTiff::new("imagej_last_comment", &stack, None);
    let mut reader = ImageReader::open(&fixture.path).expect("open ImageJ TIFF");
    let metadata = reader.metadata();

    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (1, 1, 4)
    );
    assert_eq!(metadata.image_count, 4);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);
    assert_eq!(metadata.physical_size_z_um, Some(2.25));
    assert_eq!(metadata.time_increment_seconds, Some(0.75));
    assert_eq!(reader.open_bytes(3).unwrap(), plane(3, 1));
}

#[test]
fn imagej_rgb_stack_keeps_samples_separate_from_logical_channels() {
    let mut stack = pages(2, 3);
    stack[0].description = Some(
        "ImageJ=1.54f\nimages=2\nchannels=3\nslices=1\nframes=2\nhyperstack=true\n".to_string(),
    );
    let fixture = TempTiff::new("imagej_rgb", &stack, None);
    let reader = ImageReader::open(&fixture.path).expect("open RGB ImageJ TIFF");
    let metadata = reader.metadata();

    assert!(metadata.is_rgb);
    assert_eq!(metadata.samples_per_pixel, 3);
    assert_eq!(metadata.size_c, 3);
    assert_eq!(metadata.logical_channel_count(), 1);
    assert_eq!((metadata.size_z, metadata.size_t), (1, 2));
    assert_eq!(metadata.image_count, 2);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);

    let dataset = open(&fixture.path).expect("open RGB ImageJ dataset");
    let second = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 0, 1)))
        .expect("read second RGB timepoint");
    assert_eq!(second.bytes(), plane(1, 3));
}

#[test]
fn imagej_rgb_hyperstack_exposes_declared_logical_channels() {
    let mut stack = pages(12, 3);
    stack[0].description = Some(
        "ImageJ=1.54f\nimages=12\nchannels=2\nslices=3\nframes=2\nhyperstack=true\n".to_string(),
    );
    let fixture = TempTiff::new("imagej_rgb_logical_channels", &stack, None);
    let reader = ImageReader::open(&fixture.path).expect("open RGB ImageJ hyperstack");
    let metadata = reader.metadata();

    assert!(metadata.is_rgb);
    assert_eq!(metadata.samples_per_pixel, 3);
    assert_eq!(metadata.size_c, 6);
    assert_eq!(metadata.logical_channel_count(), 2);
    assert_eq!((metadata.size_z, metadata.size_t), (3, 2));
    assert_eq!(metadata.image_count, 12);
    assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);

    let dataset = open(&fixture.path).expect("open RGB logical-channel dataset");
    let last = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(2, 1, 1)))
        .expect("read C-fastest RGB logical channel");
    assert_eq!(last.bytes(), plane(11, 3));
}

#[test]
fn imagej_first_comment_wins_and_axis_tokens_are_strict() {
    let mut stack = pages(4, 1);
    stack[0].description = Some(
        "ImageJ=first\nchannels= 2\nslices=2\nframes=1\nspacing= 1.25 \nfinterval= 2.5 \n"
            .to_string(),
    );
    stack[3].description =
        Some("ImageJ=last\nchannels=1\nslices=4\nframes=1\nspacing=9\n".to_string());
    let fixture = TempTiff::new("imagej_first_comment", &stack, None);
    let reader = ImageReader::open(&fixture.path).expect("open strict ImageJ TIFF");
    let metadata = reader.metadata();

    assert_eq!(
        (metadata.size_z, metadata.size_c, metadata.size_t),
        (1, 1, 4)
    );
    assert_eq!(metadata.physical_size_z_um, Some(1.25));
    assert_eq!(metadata.time_increment_seconds, Some(2.5));
    assert!(matches!(
        metadata.series_metadata.get("ImageJ"),
        Some(MetadataValue::String(value)) if value == "first"
    ));
}

#[test]
fn heterogeneous_primary_ifds_split_individually_and_reduced_images_are_excluded() {
    let mut stack = pages(4, 1);
    stack[1].description = Some("ImageJ=middle\nslices=1\nframes=1\n".to_string());
    stack[2].width = 2;
    stack[2].height = 2;
    stack[2].pixels = vec![91, 92, 93, 94];
    stack[3].width = 1;
    stack[3].height = 1;
    stack[3].pixels = vec![200];
    stack[3].new_subfile_type = Some(1);
    let fixture = TempTiff::new("heterogeneous_primary_ifds", &stack, None);
    let mut reader = ImageReader::open(&fixture.path).expect("open heterogeneous TIFF");

    assert_eq!(reader.series_count(), 3);
    for series in 0..3 {
        reader.set_series(series).unwrap();
        let metadata = reader.metadata();
        assert_eq!(metadata.image_count, 1);
        assert_eq!(
            (metadata.size_z, metadata.size_c, metadata.size_t),
            (1, 1, 1)
        );
        assert_eq!(metadata.dimension_order, DimensionOrder::XYCZT);
        assert!(!metadata.series_metadata.contains_key("ImageJ"));
    }
    reader.set_series(0).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), plane(0, 1));
    reader.set_series(1).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), plane(1, 1));
    reader.set_series(2).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), [91, 92, 93, 94]);

    let mut lone_reduced = pages(1, 1);
    lone_reduced[0].new_subfile_type = Some(1);
    let lone = TempTiff::new("lone_reduced_ifd", &lone_reduced, None);
    let lone_reader = ImageReader::open(&lone.path).expect("open lone reduced-image IFD");
    assert_eq!(lone_reader.series_count(), 1);
    assert_eq!(lone_reader.metadata().image_count, 1);

    let mut mixed_samples = pages(2, 1);
    mixed_samples[1].samples = 3;
    mixed_samples[1].pixels = plane(1, 3);
    let mixed = TempTiff::new("heterogeneous_sample_layout", &mixed_samples, None);
    let mut mixed_reader = ImageReader::open(&mixed.path).expect("open mixed-sample TIFF");
    assert_eq!(mixed_reader.series_count(), 2);
    mixed_reader.set_series(0).unwrap();
    assert_eq!(mixed_reader.metadata().samples_per_pixel, 1);
    assert_eq!(mixed_reader.open_bytes(0).unwrap(), plane(0, 1));
    mixed_reader.set_series(1).unwrap();
    assert_eq!(mixed_reader.metadata().samples_per_pixel, 3);
    assert_eq!(mixed_reader.open_bytes(0).unwrap(), plane(1, 3));

    let mut all_reduced = pages(2, 1);
    for page in &mut all_reduced {
        page.new_subfile_type = Some(1);
    }
    let reduced = TempTiff::new("all_reduced_ifds", &all_reduced, None);
    assert!(matches!(
        ImageReader::open(&reduced.path),
        Err(bioformats_rs::BioFormatsError::InvalidData(message))
            if message.contains("no full-resolution primary IFDs")
    ));
}

fn java_ome_xml(path: &Path) -> String {
    let root = std::env::var("BIOFORMATS_RS_BIOFORMATS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../bioformats"));
    let mut command = if let Ok(jar) = std::env::var("BIOFORMATS_RS_BIOFORMATS_JAR") {
        let mut command = Command::new("java");
        command.args(["-cp", &jar, "loci.formats.tools.ImageInfo"]);
        command
    } else {
        let showinf = root.join("tools/showinf");
        assert!(
            showinf.exists(),
            "missing Java showinf at {}",
            showinf.display()
        );
        let mut command = Command::new("bash");
        command.arg(showinf).current_dir(&root);
        command
    };
    let output = command
        .args(["-nopix", "-omexml-only"])
        .arg(path)
        .output()
        .expect("launch Java Bio-Formats showinf");
    assert!(
        output.status.success(),
        "showinf failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("showinf returned non-UTF8 XML");
    let start = stdout
        .find("<?xml")
        .or_else(|| stdout.find("<OME"))
        .expect("showinf output contains no OME-XML document");
    let end = stdout[start..]
        .rfind("</OME>")
        .map(|offset| start + offset + "</OME>".len())
        .expect("showinf output contains no complete OME-XML document");
    stdout[start..end].to_owned()
}

fn physical_length_um(pixels: roxmltree::Node<'_, '_>, name: &str) -> Option<f64> {
    let value = pixels.attribute(name)?.parse::<f64>().ok()?;
    let unit_name = format!("{name}Unit");
    let factor = match pixels.attribute(unit_name.as_str()) {
        Some("nm") => 1e-3,
        Some("mm") => 1e3,
        Some("cm") => 1e4,
        Some("m") => 1e6,
        Some("µm" | "μm" | "um") | None => 1.0,
        unit => panic!("unexpected {name} unit {unit:?}"),
    };
    Some(value * factor)
}

#[test]
#[ignore = "requires Java Bio-Formats showinf or BIOFORMATS_RS_BIOFORMATS_JAR"]
fn generated_generic_and_imagej_metadata_match_java_bioformats() {
    let generic = TempTiff::new("java_generic_time_stack", &pages(4, 1), None);
    let generic_xml = java_ome_xml(&generic.path);
    let generic_document = Document::parse(&generic_xml).expect("parse generic Java OME-XML");
    let generic_pixels = generic_document
        .descendants()
        .find(|node| node.has_tag_name("Pixels"))
        .expect("generic Java Pixels");
    assert_eq!(generic_pixels.attribute("SizeZ"), Some("1"));
    assert_eq!(generic_pixels.attribute("SizeC"), Some("1"));
    assert_eq!(generic_pixels.attribute("SizeT"), Some("4"));
    assert_eq!(generic_pixels.attribute("DimensionOrder"), Some("XYCZT"));

    let description = "ImageJ=1.54f\nimages=12\nchannels=2\nslices=3\nframes=2\nhyperstack=true\nmode=grayscale\nunit=nm\nspacing=-1.5\nfinterval=2.5\n";
    let mut stack = pages(12, 1);
    stack[0].description = Some(description.to_string());
    let imagej = TempTiff::new(
        "java_imagej_hyperstack",
        &stack,
        Some(Calibration {
            x_resolution: (4, 1),
            y_resolution: (2, 1),
            resolution_unit: 1,
        }),
    );
    let imagej_xml = java_ome_xml(&imagej.path);
    let imagej_document = Document::parse(&imagej_xml).expect("parse ImageJ Java OME-XML");
    let imagej_pixels = imagej_document
        .descendants()
        .find(|node| node.has_tag_name("Pixels"))
        .expect("ImageJ Java Pixels");
    assert_eq!(imagej_pixels.attribute("SizeZ"), Some("3"));
    assert_eq!(imagej_pixels.attribute("SizeC"), Some("2"));
    assert_eq!(imagej_pixels.attribute("SizeT"), Some("2"));
    assert_eq!(imagej_pixels.attribute("DimensionOrder"), Some("XYCZT"));
    assert_eq!(
        physical_length_um(imagej_pixels, "PhysicalSizeX"),
        Some(0.00025)
    );
    assert_eq!(
        physical_length_um(imagej_pixels, "PhysicalSizeY"),
        Some(0.0005)
    );
    assert_eq!(
        physical_length_um(imagej_pixels, "PhysicalSizeZ"),
        Some(1.5)
    );
    assert_eq!(imagej_pixels.attribute("TimeIncrement"), Some("2.5"));

    let rust = ImageReader::open(&imagej.path).expect("open generated ImageJ TIFF in Rust");
    let metadata = rust.metadata();
    assert_eq!(
        metadata.physical_size_x_um,
        physical_length_um(imagej_pixels, "PhysicalSizeX")
    );
    assert_eq!(
        metadata.physical_size_y_um,
        physical_length_um(imagej_pixels, "PhysicalSizeY")
    );
    assert_eq!(
        metadata.physical_size_z_um,
        physical_length_um(imagej_pixels, "PhysicalSizeZ")
    );
}
