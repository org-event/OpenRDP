//! Builds the OpenRDP Microphone audio driver (`driver/openrdp_audio.c`), a
//! Core Audio server plug-in. `openrdp install-system` writes it into
//! `/Library/Audio/Plug-Ins/HAL`; the binary carries it with `include_bytes!`.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let source = "driver/openrdp_audio.c";
    println!("cargo:rerun-if-changed={source}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("OpenRDPAudio");
    let target = std::env::var("TARGET").expect("TARGET");
    let arch = if target.starts_with("x86_64") {
        "x86_64"
    } else {
        "arm64"
    };
    let status = Command::new("xcrun")
        .args([
            "clang",
            "-arch",
            arch,
            "-bundle",
            "-O2",
            "-Wall",
            "-Wextra",
            "-mmacosx-version-min=11.0",
            "-fvisibility=hidden",
            "-framework",
            "CoreFoundation",
            "-o",
        ])
        .arg(&out)
        .arg(source)
        .status()
        .expect("run xcrun clang");
    assert!(status.success(), "building {source} failed");
}
