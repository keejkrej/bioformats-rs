# Vendored JPEG-XR implementation

This directory contains the source and header subset needed to build JXRLib
for `bioformats-rs`. It was copied from the Ruffle-maintained `jpegxr` fork at
commit `5281b4ae42be742779269a9f1a986101f101f32f`:

<https://github.com/ruffle-rs/jpegxr/tree/5281b4ae42be742779269a9f1a986101f101f32f>

Samples, command-line applications, binary documentation, JavaScript glue,
IDE project files, and repository metadata are intentionally excluded. The
corresponding Rust wrapper lives in `src/jpegxr`, and the native build wiring
lives in the repository's root `build.rs`. Vendored text files have also had
trailing whitespace normalized; this does not change the codec sources.

Local hardening changes on top of that revision are intentionally small:

- release a native `PKImageDecode` when initialization fails;
- zero-fill an incomplete native read and return `WMP_errFileIO` instead of
  reporting success with uninitialized destination bytes;
- validate decode rectangles, strides, checked buffer sizes, and destination
  capacity before passing a writable pointer to JXRLib;
- harden the WASM C allocator exports with checked size arithmetic,
  max-aligned metadata, standard zero-size behavior, and failure-safe
  reallocation; and
- remove the upstream sample-dependent test while retaining focused malformed
  initialization, short-read, buffer-validation, and allocator tests.

The complete BSD license is retained in `LICENSE.md` and the repository-wide
`THIRD_PARTY_NOTICES.md`. The WASM-only libc shim's `qsort.c` is derived from
Valentin Ochs' MIT-licensed smoothsort implementation; its complete notice is
also retained in `THIRD_PARTY_NOTICES.md` and at the top of that source file.
