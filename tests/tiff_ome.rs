use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats_rs::{open, BioFormatsError, ImageReader, PlaneCoordinates, ReadRequest};

struct TestTiffPage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    image_description: Option<String>,
    subifd: Option<Box<TestTiffPage>>,
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bioformats_rs_{name}_{nanos}"));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_test_tiff(path: &Path, pages: &[TestTiffPage]) {
    assert!(!pages.is_empty());

    let main_ifd_sizes = pages.iter().map(ifd_size).collect::<Vec<_>>();
    let mut child_pages = Vec::new();
    let mut child_lookup = vec![None; pages.len()];
    for (index, page) in pages.iter().enumerate() {
        if let Some(child) = page.subifd.as_deref() {
            child_lookup[index] = Some(child_pages.len());
            child_pages.push(child);
        }
    }
    let child_ifd_sizes = child_pages
        .iter()
        .map(|page| ifd_size(page))
        .collect::<Vec<_>>();

    let mut offset = 8u32;
    let main_ifd_offsets = allocate_offsets(&mut offset, &main_ifd_sizes);
    let child_ifd_offsets = allocate_offsets(&mut offset, &child_ifd_sizes);

    let main_desc_offsets = allocate_optional_blob_offsets(
        &mut offset,
        &pages
            .iter()
            .map(|page| page.image_description.as_ref().map(|value| value.len() + 1))
            .collect::<Vec<_>>(),
    );
    let child_desc_offsets = allocate_optional_blob_offsets(
        &mut offset,
        &child_pages
            .iter()
            .map(|page| page.image_description.as_ref().map(|value| value.len() + 1))
            .collect::<Vec<_>>(),
    );

    let main_pixel_offsets = allocate_blob_offsets(
        &mut offset,
        &pages
            .iter()
            .map(|page| page.pixels.len())
            .collect::<Vec<_>>(),
    );
    let child_pixel_offsets = allocate_blob_offsets(
        &mut offset,
        &child_pages
            .iter()
            .map(|page| page.pixels.len())
            .collect::<Vec<_>>(),
    );

    let mut out = Vec::with_capacity(offset as usize);
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&main_ifd_offsets[0].to_le_bytes());

    for (index, page) in pages.iter().enumerate() {
        let next_ifd = main_ifd_offsets.get(index + 1).copied().unwrap_or(0);
        let subifd_offset = child_lookup[index].map(|child_index| child_ifd_offsets[child_index]);
        write_ifd(
            &mut out,
            page,
            next_ifd,
            subifd_offset,
            main_desc_offsets[index],
            main_pixel_offsets[index],
        );
    }
    for (index, page) in child_pages.iter().enumerate() {
        write_ifd(
            &mut out,
            page,
            0,
            None,
            child_desc_offsets[index],
            child_pixel_offsets[index],
        );
    }

    for page in pages {
        if let Some(description) = &page.image_description {
            out.extend_from_slice(description.as_bytes());
            out.push(0);
        }
    }
    for page in &child_pages {
        if let Some(description) = &page.image_description {
            out.extend_from_slice(description.as_bytes());
            out.push(0);
        }
    }
    for page in pages {
        out.extend_from_slice(&page.pixels);
    }
    for page in child_pages {
        out.extend_from_slice(&page.pixels);
    }

    fs::write(path, out).unwrap();
}

fn allocate_offsets(offset: &mut u32, sizes: &[usize]) -> Vec<u32> {
    sizes
        .iter()
        .map(|size| {
            let current = *offset;
            *offset += *size as u32;
            current
        })
        .collect()
}

fn allocate_blob_offsets(offset: &mut u32, sizes: &[usize]) -> Vec<u32> {
    sizes
        .iter()
        .map(|size| {
            let current = *offset;
            *offset += *size as u32;
            current
        })
        .collect()
}

fn allocate_optional_blob_offsets(offset: &mut u32, sizes: &[Option<usize>]) -> Vec<Option<u32>> {
    sizes
        .iter()
        .map(|size| {
            size.map(|size| {
                let current = *offset;
                *offset += size as u32;
                current
            })
        })
        .collect()
}

fn ifd_size(page: &TestTiffPage) -> usize {
    let tag_count = base_tag_count(page);
    2 + tag_count as usize * 12 + 4
}

fn base_tag_count(page: &TestTiffPage) -> u16 {
    9 + u16::from(page.image_description.is_some()) + u16::from(page.subifd.is_some())
}

fn write_ifd(
    out: &mut Vec<u8>,
    page: &TestTiffPage,
    next_ifd: u32,
    subifd_offset: Option<u32>,
    description_offset: Option<u32>,
    pixel_offset: u32,
) {
    out.extend_from_slice(&base_tag_count(page).to_le_bytes());
    push_tag(out, 256, 4, 1, page.width);
    push_tag(out, 257, 4, 1, page.height);
    push_tag(out, 258, 3, 1, 8);
    push_tag(out, 259, 3, 1, 1);
    push_tag(out, 262, 3, 1, 1);
    if let Some(description) = &page.image_description {
        push_tag(
            out,
            270,
            2,
            (description.len() + 1) as u32,
            description_offset.unwrap(),
        );
    }
    push_tag(out, 273, 4, 1, pixel_offset);
    push_tag(out, 277, 3, 1, 1);
    push_tag(out, 278, 4, 1, page.height);
    push_tag(out, 279, 4, 1, page.pixels.len() as u32);
    if let Some(subifd_offset) = subifd_offset {
        push_tag(out, 330, 13, 1, subifd_offset);
    }
    out.extend_from_slice(&next_ifd.to_le_bytes());
}

fn push_tag(out: &mut Vec<u8>, tag: u16, ty: u16, count: u32, value: u32) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_rgb_ome_tiff(path: &Path, ome_xml: &str, width: u32, height: u32, pixels: &[u8]) {
    const TAG_COUNT: u16 = 11;
    let ifd_size = 2 + usize::from(TAG_COUNT) * 12 + 4;
    let bits_offset = 8 + ifd_size as u32;
    let description_offset = bits_offset + 6;
    let pixel_offset = description_offset + ome_xml.len() as u32 + 1;

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&TAG_COUNT.to_le_bytes());
    push_tag(&mut out, 256, 4, 1, width);
    push_tag(&mut out, 257, 4, 1, height);
    push_tag(&mut out, 258, 3, 3, bits_offset);
    push_tag(&mut out, 259, 3, 1, 1);
    push_tag(&mut out, 262, 3, 1, 2);
    push_tag(
        &mut out,
        270,
        2,
        (ome_xml.len() + 1) as u32,
        description_offset,
    );
    push_tag(&mut out, 273, 4, 1, pixel_offset);
    push_tag(&mut out, 277, 3, 1, 3);
    push_tag(&mut out, 278, 4, 1, height);
    push_tag(&mut out, 279, 4, 1, pixels.len() as u32);
    push_tag(&mut out, 284, 3, 1, 1);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(ome_xml.as_bytes());
    out.push(0);
    out.extend_from_slice(pixels);

    fs::write(path, out).unwrap();
}

fn pyramid_root_pixels() -> Vec<u8> {
    (1u8..=16).collect()
}

fn pyramid_sub_pixels() -> Vec<u8> {
    vec![21, 22, 23, 24]
}

#[test]
fn reads_pyramidal_tiff_flattened_and_unflattened() {
    let dir = TempDir::new("tiff_pyramid");
    let path = dir.path().join("pyramid.tif");
    write_test_tiff(
        &path,
        &[TestTiffPage {
            width: 4,
            height: 4,
            pixels: pyramid_root_pixels(),
            image_description: None,
            subifd: Some(Box::new(TestTiffPage {
                width: 2,
                height: 2,
                pixels: pyramid_sub_pixels(),
                image_description: None,
                subifd: None,
            })),
        }],
    );

    let mut flattened = ImageReader::open(&path).unwrap();
    assert!(flattened.flattened_resolutions());
    assert_eq!(flattened.used_files(), vec![path.clone()]);
    assert_eq!(flattened.series_count(), 2);
    assert_eq!(flattened.resolution_count(), 1);
    assert_eq!(flattened.metadata().size_x, 4);
    assert_eq!(flattened.metadata().size_y, 4);
    assert_eq!(flattened.metadata().resolution_count, 2);
    assert_eq!(flattened.open_bytes(0).unwrap(), pyramid_root_pixels());

    flattened.set_series(1).unwrap();
    assert_eq!(flattened.metadata().size_x, 2);
    assert_eq!(flattened.metadata().size_y, 2);
    assert_eq!(flattened.open_bytes(0).unwrap(), pyramid_sub_pixels());

    let mut hierarchical = ImageReader::open(&path).unwrap();
    hierarchical.set_flattened_resolutions(false).unwrap();
    assert!(!hierarchical.flattened_resolutions());
    assert_eq!(hierarchical.series_count(), 1);
    assert_eq!(hierarchical.resolution_count(), 2);
    assert_eq!(hierarchical.metadata().size_x, 4);
    assert_eq!(hierarchical.metadata().size_y, 4);
    assert_eq!(hierarchical.open_bytes(0).unwrap(), pyramid_root_pixels());

    hierarchical.set_resolution(1).unwrap();
    assert_eq!(hierarchical.resolution(), 1);
    assert_eq!(hierarchical.metadata().size_x, 2);
    assert_eq!(hierarchical.metadata().size_y, 2);
    assert_eq!(hierarchical.open_bytes(0).unwrap(), pyramid_sub_pixels());

    let dataset = open(&path).unwrap();
    assert_eq!(dataset.series().len(), 1);
    assert_eq!(dataset.series()[0].resolutions().len(), 2);
    let sub_resolution = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::default()).with_resolution(1))
        .unwrap();
    assert_eq!(sub_resolution.bytes(), pyramid_sub_pixels());
}

#[test]
fn reads_embedded_ome_tiff_metadata_and_planes() {
    let dir = TempDir::new("embedded_ome");
    let path = dir.path().join("sample.ome.tif");
    let ome_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Instrument ID="Instrument:0">
    <Objective ID="Objective:0" Model="PlanApo" NominalMagnification="60" LensNA="1.4"/>
  </Instrument>
  <Image ID="Image:0" Name="Series 0">
    <AcquisitionDate>2024-01-02T03:04:05</AcquisitionDate>
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SignificantBits="7" SizeX="2" SizeY="2" SizeZ="1" SizeC="2" SizeT="1" PhysicalSizeX="500" PhysicalSizeXUnit="nm" PhysicalSizeY="0.0006" PhysicalSizeYUnit="mm" PhysicalSizeZ="1500" PhysicalSizeZUnit="nm" TimeIncrement="2000" TimeIncrementUnit="ms">
      <Channel ID="Channel:0:0" Name="DAPI" Color="255" EmissionWavelength="0.45" EmissionWavelengthUnit="µm" ExcitationWavelength="405"/>
      <Channel ID="Channel:0:1" Name="FITC" Color="65280" EmissionWavelength="520" ExcitationWavelength="488"/>
      <Plane TheZ="0" TheC="0" TheT="0" DeltaT="0.0" DeltaTUnit="ms" PositionX="0.001" PositionXUnit="mm" PositionY="0.002" PositionYUnit="mm" PositionZ="0.003" PositionZUnit="mm"/>
      <Plane TheZ="0" TheC="1" TheT="0" DeltaT="1500" DeltaTUnit="ms" PositionX="0.0011" PositionXUnit="mm" PositionY="0.0021" PositionYUnit="mm" PositionZ="0.0031" PositionZUnit="mm"/>
      <TiffData IFD="0" PlaneCount="2"/>
    </Pixels>
  </Image>
</OME>"#;

    write_test_tiff(
        &path,
        &[
            TestTiffPage {
                width: 2,
                height: 2,
                pixels: vec![1, 2, 3, 4],
                image_description: Some(ome_xml.to_string()),
                subifd: None,
            },
            TestTiffPage {
                width: 2,
                height: 2,
                pixels: vec![5, 6, 7, 8],
                image_description: None,
                subifd: None,
            },
        ],
    );

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(reader.used_files(), vec![path.clone()]);
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.bits_per_pixel, 7);
    assert_eq!(meta.channel_metadata.len(), 2);
    assert_eq!(meta.channel_metadata[0].name.as_deref(), Some("DAPI"));
    assert_eq!(meta.channel_metadata[1].name.as_deref(), Some("FITC"));
    assert_eq!(meta.channel_metadata[1].color, Some(65280));
    assert_eq!(meta.channel_metadata[0].emission_wavelength_nm, Some(450.0));
    assert_eq!(meta.plane_metadata.len(), 2);
    assert_eq!(meta.plane_metadata[1].c, 1);
    assert_eq!(meta.plane_metadata[1].delta_t_seconds, Some(1.5));
    assert_eq!(meta.plane_metadata[1].position_x_um, Some(1.1));
    assert_eq!(meta.physical_size_x_um, Some(0.5));
    assert_eq!(meta.physical_size_y_um, Some(0.6));
    assert_eq!(meta.physical_size_z_um, Some(1.5));
    assert_eq!(meta.time_increment_seconds, Some(2.0));
    assert_eq!(
        meta.acquisition_timestamp.as_deref(),
        Some("2024-01-02T03:04:05")
    );
    assert_eq!(meta.objective_model.as_deref(), Some("PlanApo"));
    assert_eq!(meta.objective_magnification, Some(60.0));
    assert_eq!(meta.objective_na, Some(1.4));

    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let dataset = open(&path).unwrap();
    let channel_one = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 1, 0)))
        .unwrap();
    assert_eq!(channel_one.bytes(), &[5, 6, 7, 8]);
}

#[test]
fn bare_tiff_data_maps_all_consecutive_ifds() {
    let dir = TempDir::new("implicit_tiff_data");
    let path = dir.path().join("implicit.ome.tif");
    let ome_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="1" SizeZ="1" SizeC="2" SizeT="1">
      <Channel ID="Channel:0:0"/>
      <Channel ID="Channel:0:1"/>
      <TiffData/>
    </Pixels>
  </Image>
</OME>"#;
    write_test_tiff(
        &path,
        &[
            TestTiffPage {
                width: 2,
                height: 1,
                pixels: vec![1, 2],
                image_description: Some(ome_xml.to_string()),
                subifd: None,
            },
            TestTiffPage {
                width: 2,
                height: 1,
                pixels: vec![3, 4],
                image_description: None,
                subifd: None,
            },
        ],
    );

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), [1, 2]);
    assert_eq!(reader.open_bytes(1).unwrap(), [3, 4]);
}

#[test]
fn reads_rgb_ome_tiff_as_one_logical_channel_with_three_samples() {
    let dir = TempDir::new("rgb_ome");
    let path = dir.path().join("rgb.ome.tif");
    let ome_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0" Name="RGB">
    <Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SizeX="2" SizeY="1" SizeZ="1" SizeC="3" SizeT="1" Interleaved="true">
      <Channel ID="Channel:0:0" Name="RGB" SamplesPerPixel="3"/>
      <TiffData IFD="0" PlaneCount="1"/>
    </Pixels>
  </Image>
</OME>"#;
    let pixels = [1, 2, 3, 4, 5, 6];
    write_rgb_ome_tiff(&path, ome_xml, 2, 1, &pixels);

    let mut reader = ImageReader::open(&path).unwrap();
    let metadata = reader.metadata();
    assert_eq!(metadata.size_c, 3);
    assert_eq!(metadata.effective_size_c(), 1);
    assert_eq!(metadata.image_count, 1);
    assert_eq!(metadata.samples_per_pixel, 3);
    assert!(metadata.is_rgb);
    assert!(metadata.is_interleaved);
    assert_eq!(metadata.channel_metadata.len(), 1);
    assert_eq!(metadata.channel_metadata[0].name.as_deref(), Some("RGB"));
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);

    let dataset = open(&path).unwrap();
    let plane = dataset
        .read_plane(ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)))
        .unwrap();
    assert_eq!(plane.info().layout.samples_per_pixel, 3);
    assert_eq!(plane.bytes(), pixels);
    assert!(dataset
        .plane_info(ReadRequest::new(0, PlaneCoordinates::new(0, 1, 0)))
        .is_err());
}

#[test]
fn recognized_ome_layout_errors_do_not_fall_back_to_generic_tiff() {
    let dir = TempDir::new("invalid_rgb_ome");
    let path = dir.path().join("invalid-rgb.ome.tif");
    let ome_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SizeX="2" SizeY="1" SizeZ="1" SizeC="3" SizeT="1">
      <Channel ID="Channel:0:0" SamplesPerPixel="1"/>
      <TiffData IFD="0" PlaneCount="1"/>
    </Pixels>
  </Image>
</OME>"#;
    write_rgb_ome_tiff(&path, ome_xml, 2, 1, &[1, 2, 3, 4, 5, 6]);

    assert!(ImageReader::open(&path).is_err());
    assert!(open(&path).is_err());

    let unknown_type_path = dir.path().join("unknown-type.ome.tif");
    let unknown_type_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="mystery" SizeX="2" SizeY="1" SizeZ="1" SizeC="3" SizeT="1">
      <Channel ID="Channel:0:0" SamplesPerPixel="3"/>
      <TiffData IFD="0" PlaneCount="1"/>
    </Pixels>
  </Image>
</OME>"#;
    write_rgb_ome_tiff(
        &unknown_type_path,
        unknown_type_xml,
        2,
        1,
        &[1, 2, 3, 4, 5, 6],
    );

    assert!(ImageReader::open(&unknown_type_path).is_err());
    assert!(open(&unknown_type_path).is_err());
}

#[test]
fn rejects_malformed_ome_mapping_units_and_significant_bits() {
    let dir = TempDir::new("invalid_ome_attributes");
    let pixels = [1, 2, 3, 4, 5, 6];

    let malformed_mapping = dir.path().join("malformed-mapping.ome.tif");
    write_rgb_ome_tiff(
        &malformed_mapping,
        r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SizeX="2" SizeY="1" SizeZ="1" SizeC="3" SizeT="1">
    <Channel ID="Channel:0:0" SamplesPerPixel="3"/><TiffData IFD="bad" PlaneCount="1"/>
  </Pixels></Image>
</OME>"#,
        2,
        1,
        &pixels,
    );
    assert!(ImageReader::open(&malformed_mapping).is_err());

    let reference_frame_position = dir.path().join("reference-frame.ome.tif");
    write_rgb_ome_tiff(
        &reference_frame_position,
        r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SizeX="2" SizeY="1" SizeZ="1" SizeC="3" SizeT="1">
    <Channel ID="Channel:0:0" SamplesPerPixel="3"/><Plane TheZ="0" TheC="0" TheT="0" PositionX="1"/><TiffData IFD="0" PlaneCount="1"/>
  </Pixels></Image>
</OME>"#,
        2,
        1,
        &pixels,
    );
    assert!(matches!(
        ImageReader::open(&reference_frame_position),
        Err(BioFormatsError::UnsupportedFormat(message)) if message.contains("reference frame")
    ));

    let invalid_significant_bits = dir.path().join("significant-bits.ome.tif");
    write_rgb_ome_tiff(
        &invalid_significant_bits,
        r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SignificantBits="9" SizeX="2" SizeY="1" SizeZ="1" SizeC="3" SizeT="1">
    <Channel ID="Channel:0:0" SamplesPerPixel="3"/><TiffData IFD="0" PlaneCount="1"/>
  </Pixels></Image>
</OME>"#,
        2,
        1,
        &pixels,
    );
    assert!(ImageReader::open(&invalid_significant_bits).is_err());
}

#[test]
fn reads_companion_ome_multifile_mapping_and_used_files() {
    let dir = TempDir::new("companion_ome");
    let plane0 = dir.path().join("plane0.tif");
    let plane1 = dir.path().join("plane1.tif");
    let companion = dir.path().join("sample.ome");

    write_test_tiff(
        &plane0,
        &[TestTiffPage {
            width: 2,
            height: 2,
            pixels: vec![11, 12, 13, 14],
            image_description: None,
            subifd: None,
        }],
    );
    write_test_tiff(
        &plane1,
        &[TestTiffPage {
            width: 2,
            height: 2,
            pixels: vec![21, 22, 23, 24],
            image_description: None,
            subifd: None,
        }],
    );

    let ome_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0" Name="Series 0">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="2" SizeZ="1" SizeC="2" SizeT="1">
      <Channel ID="Channel:0:0" Name="C0"/>
      <Channel ID="Channel:0:1" Name="C1"/>
      <TiffData FileName="plane0.tif" IFD="0" FirstC="0" PlaneCount="1"/>
      <TiffData FileName="plane1.tif" IFD="0" FirstC="1" PlaneCount="1"/>
    </Pixels>
  </Image>
</OME>"#;
    fs::write(&companion, ome_xml).unwrap();

    let mut reader = ImageReader::open(&companion).unwrap();
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(
        reader.used_files(),
        vec![companion.clone(), plane0.clone(), plane1.clone()]
    );
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11, 12, 13, 14]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![21, 22, 23, 24]);
}
