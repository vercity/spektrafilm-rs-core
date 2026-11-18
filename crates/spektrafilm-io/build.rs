fn main() {
    #[cfg(feature = "libraw")]
    build_libraw();
    #[cfg(feature = "exiv2")]
    build_exiv2();
}

#[cfg(feature = "exiv2")]
fn build_exiv2() {
    use std::{env, path::PathBuf};

    const INCLUDE: &str = "SPEKTRAFILM_EXIV2_INCLUDE_DIR";
    const LIBRARY: &str = "SPEKTRAFILM_EXIV2_LIBRARY_DIR";
    const BRIDGE: &str = "src/raster/maker_note.cpp";
    println!("cargo:rerun-if-changed={BRIDGE}");
    println!("cargo:rerun-if-env-changed={INCLUDE}");
    println!("cargo:rerun-if-env-changed={LIBRARY}");

    let mut bridge = cc::Build::new();
    bridge.cpp(true).std("c++17").file(BRIDGE);
    match (env::var_os(INCLUDE), env::var_os(LIBRARY)) {
        (Some(include), Some(library)) => {
            bridge.include(include).compile("spektrafilm_exiv2_bridge");
            println!(
                "cargo:rustc-link-search=native={}",
                PathBuf::from(library).display()
            );
            println!("cargo:rustc-link-lib=dylib=exiv2");
        }
        (None, None) => {
            let library = pkg_config::Config::new()
                .atleast_version("0.28")
                .cargo_metadata(false)
                .probe("exiv2")
                .expect("Exiv2 >= 0.28 development files are required for the exiv2 feature");
            bridge
                .includes(library.include_paths)
                .compile("spektrafilm_exiv2_bridge");
            for path in library.link_paths {
                println!("cargo:rustc-link-search=native={}", path.display());
            }
            for name in library.libs {
                println!("cargo:rustc-link-lib=dylib={name}");
            }
        }
        _ => panic!("set both Exiv2 include and library directory overrides"),
    }
}

#[cfg(feature = "libraw")]
fn build_libraw() {
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
