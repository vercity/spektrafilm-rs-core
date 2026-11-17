#[cfg(not(feature = "libraw"))]
fn main() {}

#[cfg(feature = "libraw")]
fn main() {
    use std::{env, path::PathBuf};

    println!("cargo:rerun-if-changed=src/raw/libraw_bridge.cpp");
    for key in [
        "SPEKTRAFILM_LIBRAW_INCLUDE_DIR",
        "SPEKTRAFILM_LIBRAW_LIBRARY_DIR",
        "SPEKTRAFILM_LIBRAW_LIBRARY_NAME",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    let mut bridge = cc::Build::new();
    bridge
        .cpp(true)
        .std("c++17")
        .file("src/raw/libraw_bridge.cpp");
    let include = env::var_os("SPEKTRAFILM_LIBRAW_INCLUDE_DIR");
    let library = env::var_os("SPEKTRAFILM_LIBRAW_LIBRARY_DIR");
    match (include, library) {
        (Some(include), Some(directory)) => {
            let name =
                env::var("SPEKTRAFILM_LIBRAW_LIBRARY_NAME").unwrap_or_else(|_| "raw_r".into());
            bridge.include(include).compile("spektrafilm_libraw_bridge");
            println!(
                "cargo:rustc-link-search=native={}",
                PathBuf::from(directory).display()
            );
            println!("cargo:rustc-link-lib=dylib={name}");
        }
        (None, None) => {
            let discover = |name| {
                pkg_config::Config::new()
                    .atleast_version("0.22")
                    .cargo_metadata(false)
                    .probe(name)
            };
            let lib = discover("libraw_r").or_else(|_| discover("libraw")).expect(
                "LibRaw >= 0.22 development files are required for the libraw feature;                  set PKG_CONFIG_PATH or the SPEKTRAFILM_LIBRAW_INCLUDE_DIR and                  SPEKTRAFILM_LIBRAW_LIBRARY_DIR paths",
            );
            bridge
                .includes(lib.include_paths)
                .compile("spektrafilm_libraw_bridge");
            for path in lib.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
            for name in lib.libs {
                println!("cargo:rustc-link-lib={name}");
            }
        }
        _ => panic!("set both the LibRaw include and library directory overrides"),
    }
}
