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
| TIFF / BigTIFF / OME-TIFF | BSD | Multiple files/series, Java-compatible generic time stacks and heterogeneous-primary splitting, ImageJ scalar/RGB C/Z/T hyperstacks and calibration, OME effective-channel/RGB mapping, unit-normalized metadata and SignificantBits, SubIFD pyramids, chunky or planar strips/tiles, raw/LZW/Deflate/PackBits/JPEG/Zstd for byte-aligned samples, horizontal predictor for 8/16-bit samples, and unsigned packed 1–7/9–15-bit samples expanded without scaling into byte-addressable integer containers | Generated generic and scalar ImageJ stack metadata match Java 8.5 directly for default axes, C/Z/T, and physical/time calibration; native regressions additionally cover both RGB axis branches, strict global first/last comments, mismatch fallback, heterogeneous and incompatible-sample series, reduced-image exclusion, planes, and regions. Generated default, OME RGB/units/bit, pyramid, planar, endian, every packed width, packed strip/tile/layout/lazy application-source, malformed strip/tile metadata, bounded-codec/transform, and overflow tests also pass; legacy missing-IFD raw-tail repair, packed JPEG, signed packed samples, and packed widths above 16 remain unsupported, while FillOrder 2, Predictor 3, WhiteIsZero/CMYK, and non-JPEG YCbCr explicitly error; additional real codec corpus needed |
| Nikon ND2 | GPL | Modern chunked files with Nikon LV/text metadata, series and plane maps, shared components, raw/zlib pixels, row padding, indexed channel colors/LUTs, and root-scoped atomic acquisition reconciliation; legacy JP2-box files with unsigned scalar 8/16-bit JPEG 2000 codestreams, big-endian output, channel planes, and position series | Eight modern public fixtures cover scalar/indexed, shared C3, zlib, padded Z/T, NETime, planned spectral loops, stale/final metadata, and binary LV metadata against Java 8.3/8.5; the public legacy `but3_cont200-1` fixture covers all five series, both channels, metadata, selected full planes, and regions against Java 8.5; generated legacy containers cover 8/16-bit decode, C/series mapping, bounded source reads, snapshots, malformed boxes/SIZ metadata, and codec failures; multi-component/RGB legacy codestreams, JPEG 2000 reference-grid coordinates above 60,000 per axis or 1 GiB decoded, pathological tile grids, and broader legacy metadata variants remain unsupported |
| Zeiss CZI | GPL | Scenes and other series axes, both split-file naming forms, typed XML metadata, first same-part Label/SlidePreview embedded-CZI attachments, and raw/JPEG/JPEG-XR/LZW/Zstd-0/Zstd-1 subblocks; mosaic tiles and integer-scaled pyramid levels are assembled from logical coordinates while stored dimensions govern decoding, including sparse fill, rounded edge clipping, and intersection-only region reads | Public idr0011 metadata, three planes, and region match Java Bio-Formats 8.3; public `Zeiss-5-Cropped` JPEG-XR pyramid dimensions, selected full planes, native-resolution region, and default attachment series match 8.5; generated Java-readable mosaics cover factor-2/factor-3 levels, negative origins, missing tiles/planes, sparse independent M series, RGB fill/BGR conversion, rounded edges, snapshots, and bounded tile selection; LZW/Zstd pyramids still need real-vendor verification, while per-channel LUTs, complex pixels, incompatible heterogeneous layouts, cross-part and other attachment types, and non-singleton R/I/H axes remain unsupported |
| NRRD | BSD | Inline or detached raw/gzip data, scalar/vector dimensions, endian and byte-skip handling | Public `dt-helix` metadata, three planes, and region match Java Bio-Formats 8.3 |
| MRC | GPL | Little/big endian, modes 0/1/2/3/4/6/16, extended headers, IMOD and EMAN conventions | Public `EMD-2225` metadata, three planes, and region match Java Bio-Formats 8.3 |
| Hamamatsu DCIMG | GPL | Version 0 and version 1 mono8/mono16 frames, footer and row-orientation handling, plus sorted multi-file Z grouping across filesystem or application-owned sources | Public `Cell07` single-file metadata, three planes, and region match Java Bio-Formats 8.3; generated V0/V1 path and application-source groups prove Z-before-T mapping, de-duplication, direct regions, per-member footer correction, and incompatible-member rejection; the public `bead_bot4_018` group gate covers first/middle/last Z planes against Java Bio-Formats 8.5; grouping is currently automatic because there is no `groupFiles=false` option |

The public fixture URLs and environment variables are recorded in
`tests/data/README.md`. All fifteen public fixture gates across five format
families are exercised through both the low-level reader and the
application-facing request API. Fixture-gated tests are ignored in the default
test run so the repository remains self-contained.

## Remaining gap to Java Bio-Formats

The current Java checkout registers 181 readers and 15 writers. This crate has
six native reader families and intentionally has no writers. Reader
registrations are not a one-to-one count of file families—Java has alternate,
specialized, and TIFF-derived readers—but they still show that breadth is the
largest gap. Major unimplemented families include Leica LIF/LOF/XLEF,
Olympus/Evident OIR/FV1000/CellSens, Zeiss LSM/ZVI, Hamamatsu NDPI/VMS,
DICOM/NIfTI/Imaris/DeltaVision, and high-content screening formats.

Within the six implemented families, the remaining CZI gaps are non-singleton
R/I/H axes, channel LUTs, incompatible heterogeneous pixel layouts, cross-part
attachment references and types beyond the first Label/SlidePreview images,
and broader real-vendor coverage of non-JPEG-XR pyramids. ND2 legacy JPEG 2000
currently covers unsigned scalar codestreams; multi-component/RGB legacy
codestreams and the full variety of old metadata layouts remain gaps. Generic
TIFF does not implement Java's risky legacy repair that invents missing IFDs
from trailing uncompressed bytes. Reduced-image primary IFDs are excluded from
full-resolution stacks but are not currently exposed through the thumbnail API;
the remaining TIFF codec and transform limitations are listed in the matrix
above. The metadata surface is intentionally focused on pixels,
dimensions, channels, physical sizes,
acquisition timing, and selected annotations rather than Java Bio-Formats'
full OME metadata graph and service/plugin APIs.

The recommended next large milestone is CZI non-singleton R/I/H axis support
plus a real public-fixture parity gate. Those dimensions are currently rejected
before otherwise supported CZI pixel storage can be exposed, and implementing
them would close the largest remaining structural gap in an existing reader.

All six implemented reader families also run through an application-owned
`RandomAccessSource`: TIFF/OME-TIFF, ND2, CZI, NRRD, MRC, and DCIMG. Integration
tests record bounded TIFF ranges, reject a source with a malformed declared
range, share one `Dataset` across threads, resolve detached NRRD data and
multi-file OME-TIFF by name, and resolve/de-duplicate/order split CZI and grouped
DCIMG siblings.
The filesystem path APIs exercise the same reader code through the built-in
source and resolver adapters.

## Library boundary

The stable application-facing direction is `Dataset` plus explicit
`ReadRequest` values. It keeps the Java reader's mutable series/resolution state
inside the crate, exposes hierarchical metadata, distinguishes logical channels
from stored samples per pixel, validates native plane sizes, and serializes
concurrent access to one open file. Caller-buffer reads avoid an intermediate
plane for raw or gzip NRRD, MRC, and DCIMG data; other readers may still require
temporary storage for codecs or whole-plane transforms. `FormatReader`,
wrappers, and snapshots are lower-level implementation machinery.

TIFF bit unpacking is a reader storage transform rather than an application
pixel conversion: unsigned 1–7-bit values are returned unscaled in `Uint8`, and
9–15-bit values in `Uint16`. Generic TIFF retains the stored width as
significant-bit metadata; OME-TIFF reports its declared significant precision,
and OME `Type="bit"` requires a one-bit IFD while resolving to the same
byte-addressable `Uint8` layout. Source row padding and bit order do not leak
through `PixelLayout`, and an interior read expands only its requested scalar
window after bounded packed-byte decompression. Packed JPEG, signed packed
samples, and widths above 16 remain structured unsupported errors because
current decoder/Java behavior does not provide a sound parity representation
for them.

Pixel conversion is otherwise intentionally not part of this crate. An application such
as `image-rs` can decide whether to preserve native integer samples, normalize
to `f32`, lazily page planes, or eagerly materialize an entire tensor. Keeping
that policy in the application avoids forcing every integrator into the same
memory and precision tradeoffs.

The storage boundary is `SourceInput`, containing one primary
`RandomAccessSource` and an optional `CompanionResolver`. Sources expose an exact
bounded read-at operation, length, stable identity, logical name, and thread
safety. Readers retain source handles and issue range reads without creating
temporary files or preloading the complete dataset. Resolver `Named` lookups
cover explicit references (detached NRRD and OME-TIFF); complete `Siblings`
lookups cover implicit split or grouped sets (CZI and DCIMG).
`Dataset::used_sources` is authoritative for this boundary, while `used_files`
intentionally reports only real paths.

No implemented reader requires filesystem storage for opening, metadata, or
pixel reads. Filesystem-specific behavior remains in the convenience path
adapter and its automatic relative/sibling discovery, persisted reader
snapshots and memoizer rebinding, and path-oriented helpers such as file-pattern
stitching. Application-owned sources must be rebound by the application after a
restart; snapshot requests return `SnapshotUnsupported`. Format gaps in the
matrix above remain format gaps regardless of storage—for example cross-part CZI
attachments are still unsupported. DCIMG timestamp extraction is a possible
beyond-Java enhancement: the reference reader does not currently publish those
timestamp records.

`ReaderSnapshot` serialization is version-bound internal state rather than a
stable interchange format. The memoizer treats snapshots from an incompatible
schema as cache misses and rebuilds them from the source dataset.

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
