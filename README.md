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
request and reports the required buffer size before I/O. Reader-native scalar
bytes are returned with an explicit `PixelLayout`, including byte order,
significant bits, interleaving, and samples per pixel even when the samples are
not RGB. The layout describes decoded, byte-addressable samples rather than
source compression or bit packing. TIFF samples stored at unsigned 1–7 or
9–15-bit widths are expanded without scaling into `Uint8` or `Uint16`
containers, respectively. Generic TIFF retains the stored width in
`significant_bits`; OME-TIFF reports its declared significant precision, and
OME `Type="bit"` requires a one-bit IFD while using the same `Uint8`
representation. Packed JPEG storage is rejected until its decoder-output
representation is implemented explicitly.
Conversion to an application's tensor or image model belongs in an adapter
owned by that application.

Rows are tightly packed. Interleaved planes use pixel-major samples; planar
planes store complete sample components consecutively. `read_plane_into` accepts
a reusable caller buffer. NRRD (raw or gzip), MRC, and DCIMG decode directly into
it; other readers may still need temporary storage for codecs or whole-plane
transforms.

`ImageReader` and `FormatReader` remain public as the lower-level porting seam for
format implementations, wrappers, and Java parity work. Their mutable methods
follow the initialized-reader contract; embedders should prefer `Dataset`.

`open(path)` and `Dataset::open(path)` remain the convenient filesystem entry
points. Applications that already own their storage can instead use
`open_source(SourceInput)`. A `RandomAccessSource` supplies an immutable
`SourceInfo` (stable identity, logical name, and length) and exact bounded
`read_at` operations. It is `Send + Sync`; the library checks every range before
calling it and retains the source for lazy pixel reads.

```rust,no_run
use std::sync::Arc;
use bioformats_rs::{open_source, RandomAccessSource, SourceInput};

# fn open_owned(source: Arc<dyn RandomAccessSource>) -> bioformats_rs::Result<()> {
let dataset = open_source(SourceInput::new(source))?;
let sources = dataset.used_sources();
assert_eq!(sources.len(), 1);
# Ok(())
# }
```

For detached or split datasets, attach a `CompanionResolver`. `Named` requests
resolve metadata-declared members such as NRRD data files and OME-TIFF planes;
`Siblings` requests provide the complete candidate set for convention-based
datasets such as split CZI and grouped DCIMG Z stacks. DCIMG members are sorted
by logical filename, de-duplicated by stable identity, and must agree on
dimensions, frame count, pixel type, and version. As in Java Bio-Formats'
default grouping mode, every valid `.dcimg` sibling supplied by the resolver is
considered a Z member. The current API does not expose Java Bio-Formats'
`groupFiles=false` opt-out. The logical name is a naming and format hint, not a
filesystem path. The filesystem APIs are adapters over the same source and
resolver boundary—custom sources are never copied to temporary files or silently
materialized as one complete byte buffer. See the compiling
[`application_source` example](examples/application_source.rs) for an adapter to
an application-owned range store.

`Dataset::used_sources` reports every resolved source identity. `used_files`
remains available for backwards compatibility and contains only actual
filesystem paths, so it is empty for purely application-owned datasets.
Persisted reader snapshots and memoizer cache rebinding remain path-oriented;
snapshotting an application-owned source returns a recoverable
`SnapshotUnsupported` error.

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

CZI JPEG-XR decoding uses a revision-pinned Rust wrapper around the bundled
Microsoft JXRLib codec. Building that dependency currently requires a C compiler
and `libclang` for bindgen. Its permissive license notices are reproduced in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).

No CLI, JSON-RPC protocol, or server lifecycle is part of this crate. Those are
deployment adapters and remain outside the library interface.
