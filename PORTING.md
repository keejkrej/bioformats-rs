# Bio-Formats port status

This is a native reader library, not a line-for-line translation of the Java
class hierarchy. Java Bio-Formats is the behavioral authority; the Zig port is
useful for locating fields and offsets, but does not establish correctness.

## Definition of done for a reader

A reader is only considered verified for a file family after tests cover:

1. positive and negative format detection;
2. series, resolution, Z/C/T, RGB, pixel type, byte order, and used-file metadata;
3. first, middle, and last full-plane bytes against Java Bio-Formats;
4. an interior rectangular read against Java Bio-Formats;
5. malformed headers, invalid plane coordinates, and invalid regions;
6. every advertised codec and storage variant with a real or generated fixture.

Unsupported variants must return an error. A reader must not guess metadata or
return partially decoded pixels after recognizing a file.

## Current matrix

| Family | Java source module | Implemented scope | Verification |
| --- | --- | --- | --- |
| TIFF / BigTIFF / OME-TIFF | BSD | Multiple files/series, OME effective-channel/RGB mapping, unit-normalized metadata and SignificantBits, SubIFD pyramids, chunky or planar strips/tiles, raw/LZW/Deflate/PackBits/JPEG/Zstd, horizontal predictor for 8/16-bit samples | Generated default, OME RGB/units, pyramid, planar, endian, malformed-metadata, and overflow tests; packed samples, FillOrder 2, Predictor 3, WhiteIsZero/CMYK, and non-JPEG YCbCr explicitly error; additional real codec corpus needed |
| Nikon ND2 | GPL | Chunked files, Nikon LV/text metadata, series and plane maps, shared components, raw/zlib pixels, row padding, indexed channel colors/LUTs, and root-scoped atomic acquisition reconciliation | Eight public fixtures cover scalar/indexed, shared C3, zlib, padded Z/T, NETime, planned spectral loops, stale/final metadata, and binary LV metadata against Java 8.3/8.5; JPEG 2000 explicitly unsupported |
| Zeiss CZI | GPL | Scenes and other series axes, both split-file naming forms, full-resolution subblocks, typed XML metadata; raw/JPEG/LZW/Zstd-0/Zstd-1 decode paths are implemented, with compressed paths not yet real-fixture verified | Public idr0011 metadata, three planes, and region match Java Bio-Formats 8.3; pyramid blocks are skipped, heterogeneous selected pixel types and non-singleton R/I/H axes fail rather than being collapsed, and per-channel LUTs, JPEG-XR, and complex pixels remain unsupported |
| NRRD | BSD | Inline or detached raw/gzip data, scalar/vector dimensions, endian and byte-skip handling | Public `dt-helix` metadata, three planes, and region match Java Bio-Formats 8.3 |
| MRC | GPL | Little/big endian, modes 0/1/2/3/4/6/16, extended headers, IMOD and EMAN conventions | Public `EMD-2225` metadata, three planes, and region match Java Bio-Formats 8.3 |
| Hamamatsu DCIMG | GPL | Version 0 and version 1 mono8/mono16 frames, footer and row-orientation handling | Public `Cell07` metadata, three planes, and region match Java Bio-Formats 8.3; multi-file Z grouping and timestamps remain unsupported |

The public fixture URLs and environment variables are recorded in
`tests/data/README.md`. All twelve public fixture gates across five format
families are exercised through both the low-level reader and the
application-facing request API. Fixture-gated tests are ignored in the default
test run so the repository remains self-contained.

## Library boundary

The stable application-facing direction is `Dataset` plus explicit
`ReadRequest` values. It keeps the Java reader's mutable series/resolution state
inside the crate, exposes hierarchical metadata, distinguishes logical channels
from stored samples per pixel, validates native plane sizes, and serializes
concurrent access to one open file. Caller-buffer reads avoid an intermediate
plane for raw or gzip NRRD, MRC, and DCIMG data; other readers may still require
temporary storage for codecs or whole-plane transforms. `FormatReader`,
wrappers, and snapshots are lower-level implementation machinery.

Pixel conversion is intentionally not part of this crate. An application such
as `image-rs` can decide whether to preserve native integer samples, normalize
to `f32`, lazily page planes, or eagerly materialize an entire tensor. Keeping
that policy in the application avoids forcing every integrator into the same
memory and precision tradeoffs.

The current integration boundary is path-based. A future byte/custom-storage
source needs both random read-at access and a companion-file resolver; it should
not be faked by loading every dataset into one in-memory blob.

## Deliberate non-goals

- command-line, RPC, or daemon lifecycle;
- image writing;
- Java plugin discovery or reflection-compatible class APIs;
- silently falling back to Java when a native reader is incomplete;
- claiming coverage for a vendor family from synthetic fixtures alone.

## Licensing boundary

TIFF/OME-TIFF and NRRD are ported from Bio-Formats' BSD reader module. ND2, CZI,
MRC, and DCIMG originate in its GPL reader module. This crate therefore remains
`GPL-2.0-or-later`. A future BSD-only package must contain only BSD-derived code
and depend exclusively on permissively licensed components; a Cargo feature on
this mixed crate would not create that separation.
