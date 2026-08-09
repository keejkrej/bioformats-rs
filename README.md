# bioformats-rs

Native Rust readers for multidimensional biological imaging data, ported against
the Java Bio-Formats implementation and designed to be embedded in applications.

This project is under active development. It is not yet a drop-in replacement
for Java Bio-Formats. The implemented baseline includes TIFF/BigTIFF, OME-TIFF,
Nikon ND2, Zeiss CZI, NRRD, MRC, and Hamamatsu DCIMG, with support varying by
codec and vendor variant. See [PORTING.md](PORTING.md) for the exact behavior
that is currently proven.

## Application interface

Applications should use the request-based dataset interface. Opening parses
metadata and indexing structures while plane pixels remain lazy:

```rust,no_run
use bioformats_rs::{open, PlaneCoordinates, ReadRequest, Rect, Region};

# fn run() -> bioformats_rs::Result<()> {
let dataset = open("sample.ome.tif")?;
let request = ReadRequest::new(0, PlaneCoordinates::new(0, 0, 0)).with_region(
    Region::Rect(Rect::new(128, 128, 512, 512)?),
);
let required_bytes = dataset.plane_info(request)?.byte_len;
let plane = dataset.read_plane(request)?;

println!("format: {}", dataset.format().name());
assert_eq!(plane.bytes().len(), required_bytes);
# Ok(())
# }
```

Every read names its series, resolution, Z/C/T coordinates, and region. This
avoids the mutable selection ordering inherited from Java Bio-Formats and makes
the same opened dataset safe to share between threads. `plane_info` validates a
request and reports the required buffer size before I/O. Native bytes are
returned with an explicit `PixelLayout`, including byte order, significant bits,
interleaving, and samples per pixel even when the samples are not RGB. Conversion
to an application's tensor or image model belongs in an adapter owned by that
application.

Rows are tightly packed. Interleaved planes use pixel-major samples; planar
planes store complete sample components consecutively. `read_plane_into` accepts
a reusable caller buffer. NRRD (raw or gzip), MRC, and DCIMG decode directly into
it; other readers may still need temporary storage for codecs or whole-plane
transforms.

`ImageReader` and `FormatReader` remain public as the lower-level porting seam for
format implementations, wrappers, and Java parity work. Their mutable methods
follow the initialized-reader contract; embedders should prefer `Dataset`.

Opening is currently filesystem/path based, including companion-file discovery.
Byte-backed integration will require a random-access source plus a companion
resolver rather than only a `Read` stream.

## Porting rules

For each reader:

1. Treat the corresponding Java reader under `../bioformats/components` as the
   behavioral authority.
2. Use `../bioformats-zig` as an implementation hint, not as parity proof.
3. Verify positive and negative detection, dimensions, pixel type and layout,
   series/resolution/ZCT mapping, first/middle/last plane bytes, and direct region
   reads.
4. Use checked offset arithmetic and path/range reads for large files.
5. Record unsupported codecs or variants as recoverable errors rather than
   silently producing partial pixels.

Tests with generated fixtures run by default. Tests requiring real vendor files
are fixture-gated under `tests/data`; their expected values should be captured
from Java Bio-Formats.

## Integration and licensing

The crate currently declares `GPL-2.0-or-later` because it contains ports of
readers from Bio-Formats' GPL reader module. Linking it into another distributed
application can therefore affect that application's licensing. Bio-Formats also
contains a BSD reader module, but selecting only those readers would require a
separately licensed crate with a clean dependency graph; a Cargo feature alone
cannot change the license of this crate.

No CLI, JSON-RPC protocol, or server lifecycle is part of this crate. Those are
deployment adapters and remain outside the library interface.
