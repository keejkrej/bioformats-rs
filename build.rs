use std::env;
use std::path::PathBuf;

const JPEGXR_ROOT: &str = "vendor/jpegxr";

fn jpegxr_path(path: &str) -> String {
    format!("{JPEGXR_ROOT}/{path}")
}

fn main() {
    println!("cargo:rerun-if-changed={JPEGXR_ROOT}/jxrlib/");
    println!("cargo:rerun-if-changed={JPEGXR_ROOT}/src/fakelibc/");

    let sources = [
        // Core codec support.
        "jxrlib/image/sys/adapthuff.c",
        "jxrlib/image/sys/image.c",
        "jxrlib/image/sys/strcodec.c",
        "jxrlib/image/sys/strPredQuant.c",
        "jxrlib/image/sys/strTransform.c",
        "jxrlib/image/sys/perfTimerANSI.c",
        // Decoder.
        "jxrlib/image/decode/decode.c",
        "jxrlib/image/decode/postprocess.c",
        "jxrlib/image/decode/segdec.c",
        "jxrlib/image/decode/strdec.c",
        "jxrlib/image/decode/strInvTransform.c",
        "jxrlib/image/decode/strPredQuantDec.c",
        "jxrlib/image/decode/JXRTranscode.c",
        // Encoder symbols referenced by the glue library.
        "jxrlib/image/encode/encode.c",
        "jxrlib/image/encode/segenc.c",
        "jxrlib/image/encode/strenc.c",
        "jxrlib/image/encode/strFwdTransform.c",
        "jxrlib/image/encode/strPredQuantEnc.c",
        // Container and pixel-format glue.
        "jxrlib/jxrgluelib/JXRGlue.c",
        "jxrlib/jxrgluelib/JXRGlueJxr.c",
        "jxrlib/jxrgluelib/JXRGluePFC.c",
        "jxrlib/jxrgluelib/JXRMeta.c",
        // TIFF symbols referenced by the wrapper. All of these are required
        // for Windows linkers even though bioformats-rs only decodes JPEG-XR.
        "jxrlib/jxrtestlib/JXRTest.c",
        "jxrlib/jxrtestlib/JXRTestBmp.c",
        "jxrlib/jxrtestlib/JXRTestHdr.c",
        "jxrlib/jxrtestlib/JXRTestPnm.c",
        "jxrlib/jxrtestlib/JXRTestTif.c",
        "jxrlib/jxrtestlib/JXRTestYUV.c",
    ];
    let sources = sources.map(jpegxr_path);
    let target = env::var("TARGET").expect("Cargo always defines TARGET for build scripts");

    let mut build = cc::Build::new();
    build
        .files(&sources)
        .include(jpegxr_path("jxrlib"))
        .include(jpegxr_path("jxrlib/common/include"))
        .include(jpegxr_path("jxrlib/image/sys"))
        .include(jpegxr_path("jxrlib/jxrgluelib"))
        .include(jpegxr_path("jxrlib/jxrtestlib"))
        .define("__ANSI__", None)
        .define("DISABLE_PERF_MEASUREMENT", None)
        .flag_if_supported("-Wno-constant-conversion")
        .flag_if_supported("-Wno-unused-const-variable")
        .flag_if_supported("-Wno-deprecated-declarations")
        .flag_if_supported("-Wno-comment")
        .flag_if_supported("-Wno-unused-value")
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-unknown-pragmas")
        .flag_if_supported("-Wno-extra-tokens")
        .flag_if_supported("-Wno-missing-field-initializers")
        .flag_if_supported("-Wno-shift-negative-value")
        .flag_if_supported("-Wno-dangling-else")
        .flag_if_supported("-Wno-sign-compare")
        .flag_if_supported("-Wno-strict-aliasing")
        .flag_if_supported("-Wno-implicit-fallthrough")
        .flag_if_supported("-Wno-old-style-declaration")
        .flag_if_supported("-Wno-endif-labels")
        .flag_if_supported("-Wno-parentheses")
        .flag_if_supported("-Wno-misleading-indentation")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-incompatible-pointer-types")
        .opt_level(2);

    if target == "wasm32-unknown-unknown" {
        build
            .flag("-isystem")
            .flag(jpegxr_path("src/fakelibc"))
            .file(jpegxr_path("src/fakelibc/impl.c"))
            .file(jpegxr_path("src/fakelibc/qsort.c"));
    }

    // JXRLib is an older C codebase with diagnostics that vary across
    // supported compilers. The targeted flags above document known warnings;
    // this final flag also keeps unknown compiler-specific diagnostics quiet.
    build.flag("-w").compile("bioformats_jpegxr");

    let out_path = PathBuf::from(env::var("OUT_DIR").expect("Cargo always defines OUT_DIR"));
    let mut clang_args = vec![
        "-D__ANSI__".to_owned(),
        "-DDISABLE_PERF_MEASUREMENT".to_owned(),
        format!("-I{}", jpegxr_path("jxrlib/jxrgluelib")),
        format!("-I{}", jpegxr_path("jxrlib/common/include")),
        format!("-I{}", jpegxr_path("jxrlib/image/sys")),
    ];

    if target == "wasm32-unknown-unknown" {
        clang_args.push("-isystem".to_owned());
        clang_args.push(jpegxr_path("src/fakelibc"));
    }

    bindgen::Builder::default()
        .header(jpegxr_path("jxrlib/jxrgluelib/JXRGlue.h"))
        .header(jpegxr_path("jxrlib/jxrtestlib/JXRTest.h"))
        .allowlist_function(
            "^(WMP|PK|PixelFormatLookup|GetPixelFormatFromHash|GetImageEncodeIID|GetImageDecodeIID|FreeDescMetadata|Ruffle).*",
        )
        .allowlist_var("^(WMP|PK|LOOKUP|GUID_PK|IID).*")
        .allowlist_type("^(WMP|PK|ERR|BITDEPTH|BD_|BITDEPTH_BITS|COLORFORMAT).*")
        .clang_args(clang_args)
        .derive_eq(true)
        .size_t_is_usize(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("error building vendored JXRLib bindings")
        .write_to_file(out_path.join("jpegxr_bindings.rs"))
        .expect("could not write vendored JXRLib bindings");
}
