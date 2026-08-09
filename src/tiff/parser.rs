use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use crate::common::endian::*;
use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;

use super::ifd::{Ifd, IfdValue};

/// Whether the file is standard (32-bit offsets) or BigTIFF (64-bit offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiffVariant {
    Classic,
    Big,
}

fn is_known_ifd_type(type_code: u16) -> bool {
    matches!(
        type_code,
        1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 16 | 17 | 18
    )
}

fn try_collect_exact<T>(count: usize, values: impl IntoIterator<Item = T>) -> Result<Vec<T>> {
    let mut output = Vec::new();
    output.try_reserve_exact(count).map_err(|error| {
        BioFormatsError::InvalidData(format!("cannot allocate TIFF tag values: {error}"))
    })?;
    output.extend(values);
    Ok(output)
}

fn try_copy_bytes(data: &[u8]) -> Result<Vec<u8>> {
    try_collect_exact(data.len(), data.iter().copied())
}

/// Parsed state of the TIFF stream header.
#[derive(Debug)]
pub struct TiffParser<R: Read + Seek> {
    pub reader: R,
    pub little_endian: bool,
    pub variant: TiffVariant,
    /// Offset of the first IFD.
    pub first_ifd_offset: u64,
    file_len: u64,
}

impl<R: Read + Seek> TiffParser<R> {
    /// Parse the TIFF/BigTIFF file header.
    pub fn new(mut reader: R) -> Result<Self> {
        reader.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;

        let little_endian = match &magic[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => {
                return Err(BioFormatsError::Format(
                    "Not a TIFF file: bad byte-order mark".into(),
                ))
            }
        };

        let bigtiff_magic: u16 = if little_endian {
            u16::from_le_bytes([magic[2], magic[3]])
        } else {
            u16::from_be_bytes([magic[2], magic[3]])
        };

        let (variant, first_ifd_offset) = match bigtiff_magic {
            42 => {
                // Classic TIFF
                let mut off_bytes = [0u8; 4];
                reader.read_exact(&mut off_bytes)?;
                let off = if little_endian {
                    u32::from_le_bytes(off_bytes)
                } else {
                    u32::from_be_bytes(off_bytes)
                };
                (TiffVariant::Classic, off as u64)
            }
            43 => {
                // BigTIFF — 2 extra header fields before IFD offset
                let bytesize = read_u16(&mut reader, little_endian)?;
                let always_zero = read_u16(&mut reader, little_endian)?;
                if bytesize != 8 || always_zero != 0 {
                    return Err(BioFormatsError::Format(
                        "invalid BigTIFF offset-size header".into(),
                    ));
                }
                let off = read_u64(&mut reader, little_endian)?;
                (TiffVariant::Big, off)
            }
            other => {
                return Err(BioFormatsError::Format(format!(
                    "Not a TIFF file: unknown magic {:#06x}",
                    other
                )))
            }
        };

        let file_len = reader.seek(SeekFrom::End(0))?;
        Ok(TiffParser {
            reader,
            little_endian,
            variant,
            first_ifd_offset,
            file_len,
        })
    }

    /// Read all IFDs in the main IFD chain.
    pub fn read_ifds(&mut self) -> Result<Vec<Ifd>> {
        let mut ifds = Vec::new();
        let mut offset = self.first_ifd_offset;
        let mut visited = HashSet::new();
        while offset != 0 {
            visited.try_reserve(1).map_err(|error| {
                BioFormatsError::InvalidData(format!(
                    "cannot allocate TIFF IFD cycle tracker: {error}"
                ))
            })?;
            if !visited.insert(offset) {
                return Err(BioFormatsError::InvalidData(format!(
                    "TIFF IFD chain contains a cycle at offset {offset}"
                )));
            }
            let (ifd, next) = self.read_ifd(offset)?;
            ifds.try_reserve(1).map_err(|error| {
                BioFormatsError::InvalidData(format!("cannot allocate TIFF IFD list: {error}"))
            })?;
            ifds.push(ifd);
            offset = next;
        }
        Ok(ifds)
    }

    /// Read one IFD at `offset`; return the IFD and the offset of the next IFD.
    pub fn read_ifd(&mut self, offset: u64) -> Result<(Ifd, u64)> {
        self.reader.seek(SeekFrom::Start(offset))?;

        let entry_count = if self.variant == TiffVariant::Big {
            usize::try_from(read_u64(&mut self.reader, self.little_endian)?).map_err(|_| {
                BioFormatsError::InvalidData("BigTIFF IFD entry count does not fit memory".into())
            })?
        } else {
            read_u16(&mut self.reader, self.little_endian)? as usize
        };

        let entry_size = if self.variant == TiffVariant::Big {
            20_u64
        } else {
            12_u64
        };
        let next_size = if self.variant == TiffVariant::Big {
            8_u64
        } else {
            4_u64
        };
        let entries_start = self.reader.stream_position()?;
        let ifd_end = u64::try_from(entry_count)
            .ok()
            .and_then(|count| count.checked_mul(entry_size))
            .and_then(|bytes| entries_start.checked_add(bytes))
            .and_then(|end| end.checked_add(next_size))
            .ok_or_else(|| BioFormatsError::InvalidData("TIFF IFD size overflows u64".into()))?;
        if ifd_end > self.file_len {
            return Err(BioFormatsError::InvalidData(format!(
                "TIFF IFD at {offset} ends at {ifd_end}, beyond file length {}",
                self.file_len
            )));
        }

        let mut entries = HashMap::new();
        entries.try_reserve(entry_count).map_err(|error| {
            BioFormatsError::InvalidData(format!("cannot allocate TIFF IFD entries: {error}"))
        })?;

        for _ in 0..entry_count {
            let tag = read_u16(&mut self.reader, self.little_endian)?;
            let type_code = read_u16(&mut self.reader, self.little_endian)?;
            let (count, value_or_offset) = if self.variant == TiffVariant::Big {
                let c = read_u64(&mut self.reader, self.little_endian)?;
                let v = read_u64(&mut self.reader, self.little_endian)?;
                (c, v)
            } else {
                let c = read_u32(&mut self.reader, self.little_endian)? as u64;
                let v = read_u32(&mut self.reader, self.little_endian)? as u64;
                (c, v)
            };

            if is_known_ifd_type(type_code) {
                let value = self.read_ifd_value(type_code, count, value_or_offset)?;
                entries.insert(tag, value);
            }
        }

        // Read next-IFD offset
        let next_ifd = if self.variant == TiffVariant::Big {
            read_u64(&mut self.reader, self.little_endian)?
        } else {
            read_u32(&mut self.reader, self.little_endian)? as u64
        };

        Ok((Ifd { entries }, next_ifd))
    }

    fn read_ifd_value(
        &mut self,
        type_code: u16,
        count: u64,
        value_or_offset: u64,
    ) -> Result<IfdValue> {
        let type_size: u64 = match type_code {
            1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
            3 | 8 => 2,         // SHORT, SSHORT
            4 | 9 | 13 => 4,    // LONG, SLONG, IFD
            5 | 10 => 8,        // RATIONAL, SRATIONAL
            11 => 4,            // FLOAT
            12 => 8,            // DOUBLE
            16 | 18 => 8,       // LONG8, IFD8 (BigTIFF)
            17 => 8,            // SLONG8 (BigTIFF)
            _ => {
                return Err(BioFormatsError::Format(format!(
                    "Unknown IFD type {}",
                    type_code
                )))
            }
        };

        let total_bytes = count.checked_mul(type_size).ok_or_else(|| {
            BioFormatsError::InvalidData("TIFF IFD value byte count overflows u64".into())
        })?;
        let total_bytes_usize = usize::try_from(total_bytes).map_err(|_| {
            BioFormatsError::InvalidData("TIFF IFD value does not fit in memory".into())
        })?;
        if total_bytes_usize > isize::MAX as usize {
            return Err(BioFormatsError::InvalidData(
                "TIFF IFD value does not fit in memory".into(),
            ));
        }

        // Determine if value fits inline or must be read from an offset.
        let inline_limit: u64 = if self.variant == TiffVariant::Big {
            8
        } else {
            4
        };

        let data = if total_bytes <= inline_limit {
            // Reconstruct the original file-order bytes from the numeric field value.
            if self.little_endian {
                value_or_offset.to_le_bytes()[..total_bytes_usize].to_vec()
            } else {
                value_or_offset.to_be_bytes()[8 - inline_limit as usize..][..total_bytes_usize]
                    .to_vec()
            }
        } else {
            let pos_after_entry = self.reader.stream_position()?;
            let value_end = value_or_offset.checked_add(total_bytes).ok_or_else(|| {
                BioFormatsError::InvalidData("TIFF IFD value range overflows u64".into())
            })?;
            if value_end > self.file_len {
                return Err(BioFormatsError::InvalidData(format!(
                    "TIFF IFD value range {value_or_offset}..{value_end} exceeds file length {}",
                    self.file_len
                )));
            }
            let buf = read_bytes_at(&mut self.reader, value_or_offset, total_bytes_usize)?;
            self.reader.seek(SeekFrom::Start(pos_after_entry))?;
            buf
        };

        let count = usize::try_from(count).map_err(|_| {
            BioFormatsError::InvalidData("TIFF IFD element count does not fit memory".into())
        })?;
        self.decode_ifd_value(type_code, count, &data)
    }

    fn decode_ifd_value(&self, type_code: u16, count: usize, data: &[u8]) -> Result<IfdValue> {
        let le = self.little_endian;
        Ok(match type_code {
            1 => IfdValue::Byte(try_copy_bytes(data)?),
            2 => {
                // ASCII: null-separated strings; take first
                let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                let decoded = String::from_utf8_lossy(&data[..end]);
                let mut value = String::new();
                value.try_reserve_exact(decoded.len()).map_err(|error| {
                    BioFormatsError::InvalidData(format!(
                        "cannot allocate TIFF ASCII value: {error}"
                    ))
                })?;
                value.push_str(&decoded);
                IfdValue::Ascii(value)
            }
            3 => IfdValue::Short(try_collect_exact(
                count,
                data.chunks_exact(2).map(|c| {
                    if le {
                        u16::from_le_bytes([c[0], c[1]])
                    } else {
                        u16::from_be_bytes([c[0], c[1]])
                    }
                }),
            )?),
            4 => IfdValue::Long(try_collect_exact(
                count,
                data.chunks_exact(4).map(|c| {
                    if le {
                        u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                    } else {
                        u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                    }
                }),
            )?),
            13 => IfdValue::IFD(try_collect_exact(
                count,
                data.chunks_exact(4).map(|c| {
                    if le {
                        u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                    } else {
                        u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                    }
                }),
            )?),
            5 => IfdValue::Rational(try_collect_exact(
                count,
                data.chunks_exact(8).map(|c| {
                    let n = if le {
                        u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                    } else {
                        u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                    };
                    let d = if le {
                        u32::from_le_bytes([c[4], c[5], c[6], c[7]])
                    } else {
                        u32::from_be_bytes([c[4], c[5], c[6], c[7]])
                    };
                    (n, d)
                }),
            )?),
            6 => IfdValue::SByte(try_collect_exact(
                count,
                data.iter().map(|&byte| byte as i8),
            )?),
            7 => IfdValue::Undefined(try_copy_bytes(data)?),
            8 => IfdValue::SShort(try_collect_exact(
                count,
                data.chunks_exact(2).map(|c| {
                    if le {
                        i16::from_le_bytes([c[0], c[1]])
                    } else {
                        i16::from_be_bytes([c[0], c[1]])
                    }
                }),
            )?),
            9 => IfdValue::SLong(try_collect_exact(
                count,
                data.chunks_exact(4).map(|c| {
                    if le {
                        i32::from_le_bytes([c[0], c[1], c[2], c[3]])
                    } else {
                        i32::from_be_bytes([c[0], c[1], c[2], c[3]])
                    }
                }),
            )?),
            10 => IfdValue::SRational(try_collect_exact(
                count,
                data.chunks_exact(8).map(|c| {
                    let n = if le {
                        i32::from_le_bytes([c[0], c[1], c[2], c[3]])
                    } else {
                        i32::from_be_bytes([c[0], c[1], c[2], c[3]])
                    };
                    let d = if le {
                        i32::from_le_bytes([c[4], c[5], c[6], c[7]])
                    } else {
                        i32::from_be_bytes([c[4], c[5], c[6], c[7]])
                    };
                    (n, d)
                }),
            )?),
            11 => IfdValue::Float(try_collect_exact(
                count,
                data.chunks_exact(4).map(|c| {
                    f32::from_bits(if le {
                        u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                    } else {
                        u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                    })
                }),
            )?),
            12 => IfdValue::Double(try_collect_exact(
                count,
                data.chunks_exact(8).map(|c| {
                    f64::from_bits(if le {
                        u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    } else {
                        u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    })
                }),
            )?),
            16 => IfdValue::Long8(try_collect_exact(
                count,
                data.chunks_exact(8).map(|c| {
                    if le {
                        u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    } else {
                        u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    }
                }),
            )?),
            17 => IfdValue::SLong8(try_collect_exact(
                count,
                data.chunks_exact(8).map(|c| {
                    if le {
                        i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    } else {
                        i64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    }
                }),
            )?),
            18 => IfdValue::IFD8(try_collect_exact(
                count,
                data.chunks_exact(8).map(|c| {
                    if le {
                        u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    } else {
                        u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                    }
                }),
            )?),
            _ => {
                let _ = count;
                IfdValue::Undefined(try_copy_bytes(data)?)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn rejects_oversized_bigtiff_ifd_count_before_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&43_u16.to_le_bytes());
        bytes.extend_from_slice(&8_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u64.to_le_bytes());
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());

        let mut parser = TiffParser::new(Cursor::new(bytes)).unwrap();
        assert!(matches!(
            parser.read_ifds(),
            Err(BioFormatsError::InvalidData(_))
        ));
    }

    #[test]
    fn rejects_external_tag_range_and_ifd_cycles() {
        let mut oversized = Vec::new();
        oversized.extend_from_slice(b"II");
        oversized.extend_from_slice(&42_u16.to_le_bytes());
        oversized.extend_from_slice(&8_u32.to_le_bytes());
        oversized.extend_from_slice(&1_u16.to_le_bytes());
        oversized.extend_from_slice(&256_u16.to_le_bytes());
        oversized.extend_from_slice(&12_u16.to_le_bytes());
        oversized.extend_from_slice(&u32::MAX.to_le_bytes());
        oversized.extend_from_slice(&26_u32.to_le_bytes());
        oversized.extend_from_slice(&0_u32.to_le_bytes());
        let mut parser = TiffParser::new(Cursor::new(oversized)).unwrap();
        assert!(matches!(
            parser.read_ifds(),
            Err(BioFormatsError::InvalidData(_))
        ));

        let mut cyclic = Vec::new();
        cyclic.extend_from_slice(b"II");
        cyclic.extend_from_slice(&42_u16.to_le_bytes());
        cyclic.extend_from_slice(&8_u32.to_le_bytes());
        cyclic.extend_from_slice(&0_u16.to_le_bytes());
        cyclic.extend_from_slice(&8_u32.to_le_bytes());
        let mut parser = TiffParser::new(Cursor::new(cyclic)).unwrap();
        assert!(matches!(
            parser.read_ifds(),
            Err(BioFormatsError::InvalidData(_))
        ));
    }

    #[test]
    fn preserves_ifd_and_signed_long8_value_types() {
        let mut header = Vec::new();
        header.extend_from_slice(b"II");
        header.extend_from_slice(&42_u16.to_le_bytes());
        header.extend_from_slice(&0_u32.to_le_bytes());
        let parser = TiffParser::new(Cursor::new(header)).unwrap();

        assert!(matches!(
            parser.decode_ifd_value(13, 1, &7_u32.to_le_bytes()),
            Ok(IfdValue::IFD(values)) if values == [7]
        ));
        assert!(matches!(
            parser.decode_ifd_value(17, 1, &(-9_i64).to_le_bytes()),
            Ok(IfdValue::SLong8(values)) if values == [-9]
        ));
        assert!(matches!(
            parser.decode_ifd_value(18, 1, &11_u64.to_le_bytes()),
            Ok(IfdValue::IFD8(values)) if values == [11]
        ));
    }
}
