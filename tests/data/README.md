Local binary fixtures for integration testing.

Supported locations:
- `tests/data/tiff/sample.tif`
- `tests/data/nd2/sample.nd2`
- `tests/data/czi/sample.czi`

Equivalent environment variables:
- `BIOFORMATS_RS_TIFF_FIXTURE`
- `BIOFORMATS_RS_ND2_FIXTURE`
- `BIOFORMATS_RS_CZI_FIXTURE`

The tests in `tests/fixture_gated.rs` are ignored by default and can be run with:

```bash
cargo test -- --ignored
```
