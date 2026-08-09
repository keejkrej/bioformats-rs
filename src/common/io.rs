use crate::common::error::{BioFormatsError, Result};
use std::io::{Read, Seek, SeekFrom};

/// Read exactly `n` bytes at a given file offset.
pub fn read_bytes_at<R: Read + Seek>(r: &mut R, offset: u64, n: usize) -> Result<Vec<u8>> {
    if n > isize::MAX as usize {
        return Err(BioFormatsError::PlaneByteCountOverflow);
    }
    let length = u64::try_from(n).map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
    let end = offset
        .checked_add(length)
        .ok_or_else(|| BioFormatsError::InvalidData("file byte range overflows u64".into()))?;
    let file_len = r.seek(SeekFrom::End(0))?;
    if end > file_len {
        return Err(BioFormatsError::InvalidData(format!(
            "file byte range {offset}..{end} exceeds file length {file_len}"
        )));
    }
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(n).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate {n}-byte file buffer: {error}"))
    })?;
    buf.resize(n, 0);
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read a null-terminated ASCII string, up to `max_len` bytes.
pub fn read_cstring(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).into_owned()
}

/// Peek at the first N bytes of a file without consuming a reader.
pub fn peek_header(path: &std::path::Path, n: usize) -> Result<Vec<u8>> {
    use std::fs::File;
    let mut f = File::open(path).map_err(BioFormatsError::from)?;
    if n > isize::MAX as usize {
        return Err(BioFormatsError::PlaneByteCountOverflow);
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(n).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate {n}-byte header buffer: {error}"))
    })?;
    buf.resize(n, 0);
    let read = f.read(&mut buf).map_err(BioFormatsError::from)?;
    buf.truncate(read);
    Ok(buf)
}
