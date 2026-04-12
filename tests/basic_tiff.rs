use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats_rs::{ImageReader, PixelType};

fn temp_file(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("bioformats_rs_{name}_{nanos}.tif"))
}

fn write_minimal_tiff(path: &PathBuf) {
    // 2x2 grayscale, uint8, uncompressed, one strip.
    let width = 2u32;
    let height = 2u32;
    let pixels = [1u8, 2, 3, 4];
    let ifd_offset = 8u32;
    let tag_count = 9u16;
    let ifd_size = 2 + (tag_count as usize * 12) + 4;
    let bits_offset = ifd_offset as usize + ifd_size;
    let pixel_offset = bits_offset + 2;

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_offset.to_le_bytes());

    out.extend_from_slice(&tag_count.to_le_bytes());
    push_tag(&mut out, 256, 4, 1, width); // ImageWidth LONG
    push_tag(&mut out, 257, 4, 1, height); // ImageLength LONG
    push_tag(&mut out, 258, 3, 1, 8); // BitsPerSample SHORT stored inline
    push_tag(&mut out, 259, 3, 1, 1); // Compression = none
    push_tag(&mut out, 262, 3, 1, 1); // Photometric = BlackIsZero
    push_tag(&mut out, 273, 4, 1, pixel_offset as u32); // StripOffsets
    push_tag(&mut out, 277, 3, 1, 1); // SamplesPerPixel
    push_tag(&mut out, 278, 4, 1, height); // RowsPerStrip
    push_tag(&mut out, 279, 4, 1, pixels.len() as u32); // StripByteCounts
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD

    out.extend_from_slice(&8u16.to_le_bytes()); // dummy BitsPerSample payload
    out.extend_from_slice(&pixels);

    fs::write(path, out).unwrap();
}

fn push_tag(out: &mut Vec<u8>, tag: u16, ty: u16, count: u32, value: u32) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

#[test]
fn reads_minimal_tiff_and_region() {
    let path = temp_file("basic");
    write_minimal_tiff(&path);

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, PixelType::Uint8);

    let full = reader.open_bytes(0).unwrap();
    assert_eq!(full, vec![1, 2, 3, 4]);

    let region = reader.open_bytes_region(0, 1, 0, 1, 2).unwrap();
    assert_eq!(region, vec![2, 4]);

    let _ = fs::remove_file(path);
}
