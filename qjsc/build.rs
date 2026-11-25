//! Builds QuickJS-NG from the quickjs/ submodule, pinned to the release
//! Soul Injector runs (components/esp-quickjs-ng/quickjs), so the bytecode
//! version matches the device's.
fn main() {
    let sources = ["dtoa.c", "libregexp.c", "libunicode.c", "quickjs.c"];
    for file in sources.iter().chain(["quickjs.h"].iter()) {
        println!("cargo:rerun-if-changed=quickjs/{file}");
    }
    println!("cargo:rerun-if-changed=shim.c");
    assert!(
        std::path::Path::new("quickjs/quickjs.c").exists(),
        "qjsc/quickjs is empty: run `git submodule update --init`"
    );

    let mut build = cc::Build::new();
    build
        .files(sources.iter().map(|f| format!("quickjs/{f}")))
        .file("shim.c")
        .include("quickjs")
        .define("QUICKJS_NG_BUILD", None)
        .define("_GNU_SOURCE", None)
        .warnings(false);
    if build.get_compiler().is_like_msvc() {
        build
            .flag("/std:c11")
            .flag("/experimental:c11atomics")
            .flag("/J") // unsigned char, as -funsigned-char below
            .define("WIN32_LEAN_AND_MEAN", None)
            .define("_WIN32_WINNT", "0x0601");
    } else {
        build
            .flag("-funsigned-char")
            .flag_if_supported("-std=gnu11");
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            build
                .define("WIN32_LEAN_AND_MEAN", None)
                .define("_WIN32_WINNT", "0x0601");
        }
    }
    build.compile("qjs");
    if std::env::var("CARGO_CFG_UNIX").is_ok() {
        println!("cargo:rustc-link-lib=m");
    }
}
