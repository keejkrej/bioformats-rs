Local binary fixtures for integration testing.

Supported locations:
- `tests/data/tiff/sample.tif`
- `tests/data/nd2/sample.nd2`
- `tests/data/czi/sample.czi`

Equivalent environment variables:
- `BIOFORMATS_RS_TIFF_FIXTURE`
- `BIOFORMATS_RS_ND2_FIXTURE`
- `BIOFORMATS_RS_CZI_FIXTURE`

Exact public parity fixture variables:
- `BIOFORMATS_RS_ND2_PUBLIC_FIXTURE`
- `BIOFORMATS_RS_ND2_MULTICHANNEL_FIXTURE`
- `BIOFORMATS_RS_ND2_ZLIB_FIXTURE`
- `BIOFORMATS_RS_ND2_PADDED_ZT_FIXTURE`
- `BIOFORMATS_RS_ND2_PLANNED_LOOP_FIXTURE`
- `BIOFORMATS_RS_ND2_NETIME_FIXTURE`
- `BIOFORMATS_RS_ND2_FINAL_METADATA_FIXTURE`
- `BIOFORMATS_RS_ND2_BINARY_LV_FIXTURE`
- `BIOFORMATS_RS_CZI_PUBLIC_FIXTURE`
- `BIOFORMATS_RS_CZI_JPEGXR_PYRAMID_FIXTURE`
- `BIOFORMATS_RS_NRRD_FIXTURE`
- `BIOFORMATS_RS_MRC_FIXTURE`
- `BIOFORMATS_RS_DCIMG_FIXTURE`
- `BIOFORMATS_RS_DCIMG_GROUP_FIXTURE`

Public parity fixtures used by `tests/public_parity_gated.rs`:

- ND2: <https://downloads.openmicroscopy.org/images/ND2/maxime/BF007.nd2>
- ND2 shared multichannel chunk:
  <https://downloads.openmicroscopy.org/images/ND2/zenodo-10277961/MRAP1%20KO%20DN_10X03.nd2>
- ND2 zlib:
  <https://downloads.openmicroscopy.org/images/ND2/jonas/jonas_nd2Test/Exception_2.nd2>
- ND2 padded Z/T:
  <https://downloads.openmicroscopy.org/images/ND2/jonas/control002.nd2>
- ND2 stored time loop plus non-stored planned spectral loop:
  <https://downloads.openmicroscopy.org/images/ND2/aryeh/MeOh_high_fluo_003.nd2>
- ND2 NETime period selection:
  <https://downloads.openmicroscopy.org/images/ND2/jonas/header_test2.nd2>
- ND2 preliminary/final metadata precedence and zlib:
  <https://downloads.openmicroscopy.org/images/ND2/jonas/jonas_nd2Test/Exception61.nd2>
- ND2 binary LV acquisition metadata and four-byte scanline padding:
  <https://downloads.openmicroscopy.org/images/ND2/zenodo-17186598/Experiment_0001.nd2>
- CZI: <https://downloads.openmicroscopy.org/images/Zeiss-CZI/idr0011/Plate1-Blue-A_TS-Stinger/Plate1-Blue-A-02-Scene-1-P2-E1-01.czi>
- CZI JPEG-XR pyramids, label, and slide preview:
  <https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Cropped.czi>
  (80,002,752 bytes; SHA-256
  `6defce5e5507f07d91ecdaa8f2c156026495c8cef9c4ab181fc3f6564629829f`;
  CC0; credit: Venklab, Pathology, UTHSCSA)
- NRRD header: <https://downloads.openmicroscopy.org/images/NRRD/gordon/dt-helix.nhdr>
  with sibling `dt-helix.raw`
- MRC: <https://downloads.openmicroscopy.org/images/MRC/EMDB/EMD-2225/EMD-2225.map>
- DCIMG: <https://downloads.openmicroscopy.org/images/DCIMG/zenodo-14281237/Cell07_642_000_000.dcimg>
- DCIMG multi-file Z stack: download all eleven `.dcimg` members from
  <https://downloads.openmicroscopy.org/images/DCIMG/zenodo-14268554/bead_bot4_018/>
  into one directory and point `BIOFORMATS_RS_DCIMG_GROUP_FIXTURE` at any one
  member.

The original committed hashes were captured from Java Bio-Formats 8.3.0. The
additional ND2 acquisition-loop fixtures were checked against 8.5.0, including
first, middle, and last full planes plus the same bounded region. The
`Experiment_0001` hashes intentionally use 8.5.0: Bio-Formats 8.4 fixed the
four-byte scanline-padding case exposed by that file. The grouped DCIMG metadata
and hashes were also captured from Java Bio-Formats 8.5.0 with file grouping
enabled. The single-plane ND2 fixture necessarily checks its only plane plus a
region. The JPEG-XR CZI hierarchy, attachment-series metadata, selected full
planes, and native-resolution region were captured from Java Bio-Formats 8.5.0.

The three optional local-fixture tests are ignored by default and can be run
without also selecting the public corpus gates:

```bash
cargo test --test fixture_gated -- --ignored
```

Run a public gate by setting its listed environment variable and selecting its
test name in `public_parity_gated`.
