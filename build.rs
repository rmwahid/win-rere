//! Links the prebuilt Windows resource file into the executable.
//!
//! The icon and version block are compiled once from `win-rere.rc` into
//! `assets/win-rere.res` with the Windows SDK's `rc.exe`. The build script does
//! not execute a compiler itself: the build stays independent of the SDK and
//! never runs an environment-supplied binary.
//!
//! Regenerate the resource after changing `win-rere.rc` or the icon:
//! `rc /nologo /fo assets/win-rere.res win-rere.rc`.

use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=assets/win-rere.res");
    println!("cargo:rerun-if-changed=assets/win-rere.ico");
    println!("cargo:rerun-if-changed=win-rere.rc");

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let resource = Path::new(&manifest).join("assets").join("win-rere.res");

    if !resource.is_file() {
        println!("cargo:warning=assets/win-rere.res is missing, so the executable has no icon");
        return;
    }

    // The binary resource is committed. Warn when its sources are newer so a
    // stale icon or version block is visible instead of being silently linked.
    let resource_time = fs::metadata(&resource).and_then(|metadata| metadata.modified()).ok();
    for source in ["win-rere.rc", "assets/win-rere.ico"] {
        let source_time = fs::metadata(source).and_then(|metadata| metadata.modified()).ok();
        if let (Some(source_time), Some(resource_time)) = (source_time, resource_time) {
            if source_time > resource_time {
                println!(
                    "cargo:warning={source} is newer than assets/win-rere.res; regenerate the resource"
                );
            }
        }
    }

    println!("cargo:rustc-link-arg-bins={}", resource.display());
}
