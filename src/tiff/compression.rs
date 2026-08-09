use super::ifd::Compression;
use crate::common::codec::*;
use crate::common::error::{BioFormatsError, Result};

/// Decompress one strip or tile using the specified TIFF compression scheme.
/// `jpeg_tables` may contain JFIF tables from tag 347 for old-style JPEG tiles.
#[derive(Debug, Clone, Copy)]
pub(super) struct DecompressionOptions<'a> {
    pub expected_len: usize,
    pub predictor: u16,
    pub samples_per_pixel: u16,
    pub bits_per_sample: u16,
    pub row_width: u32,
    pub little_endian: bool,
    pub jpeg_tables: Option<&'a [u8]>,
}

pub(super) fn decompress(
    data: &[u8],
    compression: Compression,
    options: DecompressionOptions<'_>,
) -> Result<Vec<u8>> {
    let DecompressionOptions {
        expected_len,
        predictor,
        samples_per_pixel,
        bits_per_sample,
        row_width,
        little_endian,
        jpeg_tables,
    } = options;
    let mut out = match compression {
        Compression::None => {
            let length = data.len().min(expected_len);
            let mut output = Vec::new();
            output.try_reserve_exact(length).map_err(|error| {
                BioFormatsError::Codec(format!(
                    "cannot allocate {length}-byte TIFF raw buffer: {error}"
                ))
            })?;
            output.extend_from_slice(&data[..length]);
            output
        }
        Compression::Lzw => decompress_lzw_limited(data, expected_len)?,
        Compression::Deflate => decompress_deflate_limited(data, expected_len)?,
        // TIFF code 32946 is the legacy/proprietary spelling for the same
        // zlib-wrapped stream used by code 8 (matching Bio-Formats).
        Compression::DeflateOld => decompress_deflate_limited(data, expected_len)?,
        Compression::PackBits => decompress_packbits_limited(data, expected_len)?,
        Compression::JpegNew | Compression::Jpeg => {
            // JPEGTables may be shared by either JPEG compression spelling.
            if let Some(tables) = jpeg_tables {
                let combined = merge_old_style_jpeg(tables, data)?;
                decompress_jpeg_limited(&combined, expected_len)?
            } else {
                decompress_jpeg_limited(data, expected_len)?
            }
        }
        Compression::Zstd => decompress_zstd_limited(data, expected_len)?,
        Compression::Ccitt => {
            return Err(BioFormatsError::UnsupportedFormat(
                "CCITT compression not yet supported".into(),
            ))
        }
        Compression::Unknown(c) => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Unknown TIFF compression code {}",
                c
            )))
        }
    };

    // Apply predictor (horizontal differencing), restarting at each scanline.
    if predictor == 2 {
        let predictor_len = out.len().min(expected_len);
        undo_horizontal_predictor(
            &mut out[..predictor_len],
            row_width,
            samples_per_pixel,
            bits_per_sample,
            little_endian,
        )?;
    }

    Ok(out)
}

fn merge_old_style_jpeg(tables: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if tables.len() < 4 || !tables.starts_with(&[0xff, 0xd8]) || !tables.ends_with(&[0xff, 0xd9]) {
        return Err(BioFormatsError::Codec(
            "TIFF JPEGTables must be an SOI/EOI-delimited JPEG stream".into(),
        ));
    }
    if data.len() < 2 || !data.starts_with(&[0xff, 0xd8]) {
        return Err(BioFormatsError::Codec(
            "old-style TIFF JPEG strip must start with an SOI marker".into(),
        ));
    }

    let tables_without_eoi = &tables[..tables.len() - 2];
    let data_without_soi = &data[2..];
    let combined_len = tables_without_eoi
        .len()
        .checked_add(data_without_soi.len())
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let mut combined = Vec::new();
    combined.try_reserve_exact(combined_len).map_err(|error| {
        BioFormatsError::Codec(format!("cannot allocate old-style JPEG stream: {error}"))
    })?;
    combined.extend_from_slice(tables_without_eoi);
    combined.extend_from_slice(data_without_soi);
    Ok(combined)
}

fn undo_horizontal_predictor(
    data: &mut [u8],
    row_width: u32,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    little_endian: bool,
) -> Result<()> {
    let samples_per_pixel = usize::from(samples_per_pixel);
    let row_samples = usize::try_from(row_width)
        .ok()
        .and_then(|width| width.checked_mul(samples_per_pixel))
        .ok_or_else(|| BioFormatsError::Codec("TIFF predictor row size overflows memory".into()))?;
    if row_samples == 0 {
        return Err(BioFormatsError::Codec(
            "TIFF predictor row width and SamplesPerPixel must be non-zero".into(),
        ));
    }

    match bits_per_sample {
        8 => {
            for row in data.chunks_mut(row_samples) {
                undo_horizontal_differencing(row, samples_per_pixel);
            }
        }
        16 => {
            let row_bytes = row_samples.checked_mul(2).ok_or_else(|| {
                BioFormatsError::Codec("TIFF predictor row byte count overflows memory".into())
            })?;
            if !data.len().is_multiple_of(2) {
                return Err(BioFormatsError::Codec(
                    "TIFF 16-bit predictor data has an odd byte count".into(),
                ));
            }
            for row in data.chunks_mut(row_bytes) {
                let samples = row.len() / 2;
                for sample in samples_per_pixel..samples {
                    let current_offset = sample * 2;
                    let previous_offset = (sample - samples_per_pixel) * 2;
                    let current = read_u16(&row[current_offset..current_offset + 2], little_endian);
                    let previous =
                        read_u16(&row[previous_offset..previous_offset + 2], little_endian);
                    write_u16(
                        &mut row[current_offset..current_offset + 2],
                        current.wrapping_add(previous),
                        little_endian,
                    );
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn read_u16(bytes: &[u8], little_endian: bool) -> u16 {
    let bytes = [bytes[0], bytes[1]];
    if little_endian {
        u16::from_le_bytes(bytes)
    } else {
        u16::from_be_bytes(bytes)
    }
}

fn write_u16(bytes: &mut [u8], value: u16, little_endian: bool) {
    let encoded = if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes.copy_from_slice(&encoded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn predictor_restarts_for_each_eight_bit_row() {
        let encoded = [1, 1, 1, 10, 10, 10];
        let decoded = decompress(
            &encoded,
            Compression::None,
            DecompressionOptions {
                expected_len: encoded.len(),
                predictor: 2,
                samples_per_pixel: 1,
                bits_per_sample: 8,
                row_width: 3,
                little_endian: true,
                jpeg_tables: None,
            },
        )
        .unwrap();

        assert_eq!(decoded, [1, 2, 3, 10, 20, 30]);
    }

    #[test]
    fn predictor_preserves_big_endian_sixteen_bit_samples() {
        let encoded_samples = [0x0100_u16, 0x0002, 0x00fe, 0x1000, 0x0001, 0x0002];
        let encoded = encoded_samples
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let decoded = decompress(
            &encoded,
            Compression::None,
            DecompressionOptions {
                expected_len: encoded.len(),
                predictor: 2,
                samples_per_pixel: 1,
                bits_per_sample: 16,
                row_width: 3,
                little_endian: false,
                jpeg_tables: None,
            },
        )
        .unwrap();
        let expected = [0x0100_u16, 0x0102, 0x0200, 0x1000, 0x1001, 0x1003]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn deflate_output_is_bounded_before_materialization() {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&[7; 1_024]).unwrap();
        let encoded = encoder.finish().unwrap();

        let result = decompress(
            &encoded,
            Compression::Deflate,
            DecompressionOptions {
                expected_len: 8,
                predictor: 1,
                samples_per_pixel: 1,
                bits_per_sample: 8,
                row_width: 8,
                little_endian: true,
                jpeg_tables: None,
            },
        );

        assert!(matches!(result, Err(BioFormatsError::Codec(_))));
    }

    #[test]
    fn bounded_lzw_and_legacy_zlib_deflate_round_trip() {
        let pixels = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        let lzw = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .encode(&pixels)
            .unwrap();
        let options = DecompressionOptions {
            expected_len: pixels.len(),
            predictor: 1,
            samples_per_pixel: 1,
            bits_per_sample: 8,
            row_width: pixels.len() as u32,
            little_endian: true,
            jpeg_tables: None,
        };
        assert_eq!(decompress(&lzw, Compression::Lzw, options).unwrap(), pixels);

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&pixels).unwrap();
        let zlib_deflate = encoder.finish().unwrap();
        assert_eq!(
            decompress(&zlib_deflate, Compression::DeflateOld, options).unwrap(),
            pixels
        );

        assert!(matches!(
            decompress(
                &lzw,
                Compression::Lzw,
                DecompressionOptions {
                    expected_len: pixels.len() - 1,
                    ..options
                }
            ),
            Err(BioFormatsError::Codec(_))
        ));
    }

    #[test]
    fn old_style_jpeg_merge_removes_internal_eoi_and_soi() {
        let tables = [0xff, 0xd8, 0xff, 0xdb, 1, 2, 0xff, 0xd9];
        let strip = [0xff, 0xd8, 0xff, 0xda, 3, 4, 0xff, 0xd9];
        assert_eq!(
            merge_old_style_jpeg(&tables, &strip).unwrap(),
            [0xff, 0xd8, 0xff, 0xdb, 1, 2, 0xff, 0xda, 3, 4, 0xff, 0xd9]
        );

        assert!(merge_old_style_jpeg(&tables[..tables.len() - 2], &strip).is_err());
        assert!(merge_old_style_jpeg(&tables, &strip[2..]).is_err());
    }
}
