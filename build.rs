//! Executable resources on Windows: the icon and the version block.
//!
//! The usual path would be to write a `.rc` and compile it with the Windows
//! SDK's `rc.exe`, but that does not ship with Rust and is rarely on the
//! PATH — the build would break on a clean machine. The `.res` format is a
//! sequence of simple records, so it is assembled here, and `link.exe`
//! accepts the file directly.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;

/// Windows uses the lowest-id `RT_GROUP_ICON` as the file icon.
const GROUP_ID: u16 = 1;
const LANGUAGE: u16 = 0x0409;

fn main() {
    println!("cargo:rerun-if-changed=assets/torrra.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // MSVC only: mingw's `ld` does not accept `.res` without `windres`.
    let target_windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let target_msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !target_windows || !target_msvc {
        return;
    }

    let icon = Path::new("assets/torrra.ico");
    if !icon.exists() {
        println!(
            "cargo:warning=assets/torrra.ico does not exist; the executable will have no icon"
        );
        return;
    }

    let ico = match fs::read(icon) {
        Ok(bytes) => bytes,
        Err(e) => {
            println!("cargo:warning=could not read assets/torrra.ico ({e}); building without icon");
            return;
        }
    };
    let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let Some(res) = build_res(&ico, &version) else {
        println!("cargo:warning=assets/torrra.ico is malformed; building without icon");
        return;
    };

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("torrra.res");
    if let Err(e) = fs::write(&out, res) {
        println!("cargo:warning=could not write the .res file ({e}); building without icon");
        return;
    }
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

// -- .res assembly ----------------------------------------------------------

fn build_res(ico: &[u8], version: &str) -> Option<Vec<u8>> {
    let images = read_ico(ico)?;
    let mut res = Vec::new();

    // Every `.res` starts with an empty record, which marks the format.
    record(&mut res, 0, 0, 0, 0, &[]);

    // Each image becomes an RT_ICON; the RT_GROUP_ICON is the index tying them together.
    for (i, img) in images.iter().enumerate() {
        record(&mut res, RT_ICON, i as u16 + 1, 0x1010, LANGUAGE, &img.data);
    }
    record(
        &mut res,
        RT_GROUP_ICON,
        GROUP_ID,
        0x1030,
        LANGUAGE,
        &icon_group(&images),
    );
    record(
        &mut res,
        RT_VERSION,
        1,
        0x0030,
        LANGUAGE,
        &version_info(version),
    );
    Some(res)
}

/// One `.res` record: a fixed-size header (type and name as ordinals)
/// followed by the data, everything aligned to 4 bytes.
fn record(res: &mut Vec<u8>, kind: u16, name: u16, flags: u16, language: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&32u32.to_le_bytes()); // size of this header
    res.extend_from_slice(&0xFFFFu16.to_le_bytes()); // type comes as an ordinal...
    res.extend_from_slice(&kind.to_le_bytes());
    res.extend_from_slice(&0xFFFFu16.to_le_bytes()); // ...and so does the name
    res.extend_from_slice(&name.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    res.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    res.extend_from_slice(&language.to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(data);
    align(res);
}

struct Image {
    width: u8,
    height: u8,
    colors: u8,
    planes: u16,
    bits: u16,
    data: Vec<u8>,
}

/// Reads the `.ico` directory. Width and height are bytes, with 0 meaning 256.
/// Returns `None` on a truncated or malformed file instead of panicking.
fn read_ico(ico: &[u8]) -> Option<Vec<Image>> {
    if ico.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;

    let mut images = Vec::with_capacity(count);
    for i in 0..count {
        let e = 6 + i * 16;
        let entry = ico.get(e..e + 16)?;
        let size = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let start = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        let data = ico.get(start..start.checked_add(size)?)?.to_vec();
        images.push(Image {
            width: entry[0],
            height: entry[1],
            colors: entry[2],
            planes: u16::from_le_bytes([entry[4], entry[5]]),
            bits: u16::from_le_bytes([entry[6], entry[7]]),
            data,
        });
    }
    Some(images)
}

/// The GRPICONDIR: the same directory as the `.ico`, but each entry points
/// to a resource id instead of a file offset.
fn icon_group(images: &[Image]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&0u16.to_le_bytes()); // reserved
    v.extend_from_slice(&1u16.to_le_bytes()); // 1 = icon
    v.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        v.push(img.width);
        v.push(img.height);
        v.push(img.colors);
        v.push(0); // reserved
        v.extend_from_slice(&img.planes.to_le_bytes());
        v.extend_from_slice(&img.bits.to_le_bytes());
        v.extend_from_slice(&(img.data.len() as u32).to_le_bytes());
        v.extend_from_slice(&(i as u16 + 1).to_le_bytes()); // RT_ICON id
    }
    v
}

// -- VERSIONINFO ------------------------------------------------------------

/// The block Explorer shows in Properties and that names the process in
/// Task Manager. It is a tree of nodes with a uniform format.
fn version_info(version: &str) -> Vec<u8> {
    let n: Vec<u16> = version
        .split(['.', '-', '+'])
        .filter_map(|p| p.parse().ok())
        .chain([0, 0, 0, 0])
        .take(4)
        .collect();
    let (major, minor, patch, build) = (n[0] as u32, n[1] as u32, n[2] as u32, n[3] as u32);

    let mut fixed = Vec::new();
    fixed.extend_from_slice(&0xFEEF04BDu32.to_le_bytes()); // signature
    fixed.extend_from_slice(&0x00010000u32.to_le_bytes()); // struct version
    fixed.extend_from_slice(&(major << 16 | minor).to_le_bytes()); // file
    fixed.extend_from_slice(&(patch << 16 | build).to_le_bytes());
    fixed.extend_from_slice(&(major << 16 | minor).to_le_bytes()); // product
    fixed.extend_from_slice(&(patch << 16 | build).to_le_bytes());
    fixed.extend_from_slice(&0x3Fu32.to_le_bytes()); // FileFlagsMask
    fixed.extend_from_slice(&0u32.to_le_bytes()); // FileFlags
    fixed.extend_from_slice(&0x00040004u32.to_le_bytes()); // VOS_NT_WINDOWS32
    fixed.extend_from_slice(&1u32.to_le_bytes()); // VFT_APP
    fixed.extend_from_slice(&0u32.to_le_bytes()); // subtype
    fixed.extend_from_slice(&0u32.to_le_bytes()); // date (high)
    fixed.extend_from_slice(&0u32.to_le_bytes()); // date (low)

    let short_version = format!("{major}.{minor}.{patch}");
    let long_version = format!("{major}.{minor}.{patch}.{build}");
    let strings = [
        ("CompanyName", "torrra"),
        ("FileDescription", "Torrent 20X — BitTorrent client"),
        ("FileVersion", long_version.as_str()),
        ("InternalName", "torrra"),
        ("OriginalFilename", "torrra.exe"),
        ("ProductName", "Torrent 20X"),
        ("ProductVersion", short_version.as_str()),
    ];

    let table = node(
        // The table name is the language followed by the code page: 0409 04B0.
        &format!("{LANGUAGE:04X}04B0"),
        1,
        &[],
        0,
        &strings.map(|(k, v)| text(k, v)),
    );
    let translation = {
        let mut v = Vec::new();
        v.extend_from_slice(&LANGUAGE.to_le_bytes());
        v.extend_from_slice(&0x04B0u16.to_le_bytes());
        node("Translation", 0, &v, 4, &[])
    };

    node(
        "VS_VERSION_INFO",
        0,
        &fixed,
        fixed.len() as u16,
        &[
            node("StringFileInfo", 1, &[], 0, &[table]),
            node("VarFileInfo", 1, &[], 0, &[translation]),
        ],
    )
}

/// A text key/value pair. For text nodes the value length is counted in
/// characters, not bytes — the classic mistake in this format.
fn text(key: &str, value: &str) -> Vec<u8> {
    let utf16 = utf16z(value);
    node(key, 1, &utf16, (utf16.len() / 2) as u16, &[])
}

/// A VERSIONINFO node: total size, value size, type, key in UTF-16, the
/// value and the children — each part aligned to 4 bytes.
fn node(key: &str, kind: u16, value: &[u8], value_len: u16, children: &[Vec<u8>]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&0u16.to_le_bytes()); // placeholder for the size
    v.extend_from_slice(&value_len.to_le_bytes());
    v.extend_from_slice(&kind.to_le_bytes());
    v.extend_from_slice(&utf16z(key));
    align(&mut v);
    v.extend_from_slice(value);
    align(&mut v);
    for child in children {
        v.extend_from_slice(child);
        align(&mut v);
    }
    let total = u16::try_from(v.len()).expect("VERSIONINFO node larger than 64 KiB");
    v[..2].copy_from_slice(&total.to_le_bytes());
    v
}

/// UTF-16 little-endian, null-terminated.
fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn align(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
}
