use super::error::{BioFormatsError, Result};

fn limited_buffer(maximum_length: usize, context: &str) -> Result<Vec<u8>> {
    if maximum_length > isize::MAX as usize {
        return Err(BioFormatsError::PlaneByteCountOverflow);
    }
    let mut output = Vec::new();
    output.try_reserve_exact(maximum_length).map_err(|error| {
        BioFormatsError::Codec(format!(
            "cannot allocate {maximum_length}-byte {context} buffer: {error}"
        ))
    })?;
    Ok(output)
}

fn read_decoded_limited(
    reader: impl std::io::Read,
    maximum_length: usize,
    context: &str,
) -> Result<Vec<u8>> {
    use std::io::Read;

    let probe_length = maximum_length
        .checked_add(1)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let mut output = limited_buffer(probe_length, context)?;
    let limit = u64::try_from(probe_length).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    reader
        .take(limit)
        .read_to_end(&mut output)
        .map_err(BioFormatsError::from)?;
    if output.len() > maximum_length {
        return Err(BioFormatsError::Codec(format!(
            "{context} output exceeds the expected {maximum_length} bytes"
        )));
    }
    Ok(output)
}

/// Decompress LZW-encoded data (TIFF variant — horizontal differencing applied separately).
pub fn decompress_lzw(data: &[u8]) -> Result<Vec<u8>> {
    use weezl::{decode::Decoder, BitOrder};
    let mut decoder = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
    decoder
        .decode(data)
        .map_err(|e| BioFormatsError::Codec(e.to_string()))
}

/// Decompress TIFF-flavoured LZW without allowing output past `maximum_length`.
pub fn decompress_lzw_limited(data: &[u8], maximum_length: usize) -> Result<Vec<u8>> {
    use weezl::{decode::Configuration, BitOrder, LzwStatus};

    let probe_length = maximum_length
        .checked_add(1)
        .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
    let mut output = limited_buffer(probe_length, "LZW output")?;
    output.resize(probe_length, 0);
    let mut decoder = Configuration::with_tiff_size_switch(BitOrder::Msb, 8)
        .with_yield_on_full_buffer(true)
        .build();
    let mut input_offset = 0usize;
    let mut output_offset = 0usize;
    loop {
        let result = decoder.decode_bytes(&data[input_offset..], &mut output[output_offset..]);
        input_offset = input_offset
            .checked_add(result.consumed_in)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        output_offset = output_offset
            .checked_add(result.consumed_out)
            .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
        if output_offset > maximum_length {
            return Err(BioFormatsError::Codec(format!(
                "LZW output exceeds the expected {maximum_length} bytes"
            )));
        }
        match result
            .status
            .map_err(|error| BioFormatsError::Codec(error.to_string()))?
        {
            LzwStatus::Done => break,
            LzwStatus::NoProgress => {
                return Err(BioFormatsError::Codec(
                    "LZW decoder made no progress before its end marker".into(),
                ))
            }
            LzwStatus::Ok if result.consumed_in == 0 && result.consumed_out == 0 => {
                return Err(BioFormatsError::Codec(
                    "LZW decoder made no progress".into(),
                ))
            }
            LzwStatus::Ok => {}
        }
    }
    output.truncate(output_offset);
    Ok(output)
}

/// Decompress zlib-wrapped Deflate data (TIFF compression codes 8 and 32946).
pub fn decompress_deflate(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(BioFormatsError::from)?;
    Ok(out)
}

pub fn decompress_deflate_limited(data: &[u8], maximum_length: usize) -> Result<Vec<u8>> {
    read_decoded_limited(
        flate2::read::ZlibDecoder::new(data),
        maximum_length,
        "Deflate",
    )
}

/// Decompress raw Deflate (no zlib header).
pub fn decompress_deflate_raw(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use std::io::Read;
    let mut decoder = DeflateDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(BioFormatsError::from)?;
    Ok(out)
}

pub fn decompress_deflate_raw_limited(data: &[u8], maximum_length: usize) -> Result<Vec<u8>> {
    read_decoded_limited(
        flate2::read::DeflateDecoder::new(data),
        maximum_length,
        "raw Deflate",
    )
}

/// Decompress PackBits run-length encoding (TIFF compression 32773).
pub fn decompress_packbits(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let header = data[i] as i8;
        i += 1;
        if header >= 0 {
            // Copy (header+1) literal bytes
            let count = (header as usize) + 1;
            if i + count > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "PackBits: literal run overruns input".into(),
                ));
            }
            out.extend_from_slice(&data[i..i + count]);
            i += count;
        } else if header != -128 {
            // Repeat next byte (-header+1) times
            let count = (-header as usize) + 1;
            if i >= data.len() {
                return Err(BioFormatsError::InvalidData(
                    "PackBits: repeat run missing byte".into(),
                ));
            }
            let byte = data[i];
            i += 1;
            for _ in 0..count {
                out.push(byte);
            }
        }
        // header == -128: NOP
    }
    Ok(out)
}

pub fn decompress_packbits_limited(data: &[u8], maximum_length: usize) -> Result<Vec<u8>> {
    let mut out = limited_buffer(maximum_length, "PackBits output")?;
    let mut i = 0;
    while i < data.len() {
        let header = data[i] as i8;
        i += 1;
        if header >= 0 {
            let count = header as usize + 1;
            let input_end = i
                .checked_add(count)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            if input_end > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "PackBits: literal run overruns input".into(),
                ));
            }
            let output_end = out
                .len()
                .checked_add(count)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            if output_end > maximum_length {
                return Err(BioFormatsError::Codec(format!(
                    "PackBits output exceeds the expected {maximum_length} bytes"
                )));
            }
            out.extend_from_slice(&data[i..input_end]);
            i = input_end;
        } else if header != -128 {
            let count = (-header as usize) + 1;
            if i >= data.len() {
                return Err(BioFormatsError::InvalidData(
                    "PackBits: repeat run missing byte".into(),
                ));
            }
            let output_end = out
                .len()
                .checked_add(count)
                .ok_or(BioFormatsError::PlaneByteCountOverflow)?;
            if output_end > maximum_length {
                return Err(BioFormatsError::Codec(format!(
                    "PackBits output exceeds the expected {maximum_length} bytes"
                )));
            }
            out.resize(output_end, data[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// Decompress JPEG data.
pub fn decompress_jpeg(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder
        .decode()
        .map_err(|e| BioFormatsError::Codec(e.to_string()))
}

pub fn decompress_jpeg_limited(data: &[u8], maximum_length: usize) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder.set_max_decoding_buffer_size(maximum_length);
    let output = decoder
        .decode()
        .map_err(|error| BioFormatsError::Codec(error.to_string()))?;
    if output.len() > maximum_length {
        return Err(BioFormatsError::Codec(format!(
            "JPEG output exceeds the expected {maximum_length} bytes"
        )));
    }
    Ok(output)
}

/// Decompress Zstd data.
pub fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(data).map_err(BioFormatsError::from)
}

pub fn decompress_zstd_limited(data: &[u8], maximum_length: usize) -> Result<Vec<u8>> {
    let decoder = zstd::stream::read::Decoder::new(data).map_err(BioFormatsError::from)?;
    read_decoded_limited(decoder, maximum_length, "Zstd")
}

/// Apply TIFF horizontal differencing predictor (predictor = 2).
/// Modifies `data` in-place. `samples_per_pixel` is the number of components.
pub fn undo_horizontal_differencing(data: &mut [u8], samples_per_pixel: usize) {
    if samples_per_pixel == 0 || data.len() < samples_per_pixel * 2 {
        return;
    }
    for i in samples_per_pixel..data.len() {
        data[i] = data[i].wrapping_add(data[i - samples_per_pixel]);
    }
}

/// Apply TIFF horizontal differencing predictor for 16-bit samples.
pub fn undo_horizontal_differencing_u16(data: &mut [u16], samples_per_pixel: usize) {
    if samples_per_pixel == 0 || data.len() < samples_per_pixel * 2 {
        return;
    }
    for i in samples_per_pixel..data.len() {
        data[i] = data[i].wrapping_add(data[i - samples_per_pixel]);
    }
}
