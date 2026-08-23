//! Compiles the icon GResource bundle and indexes the bundled fonts.
//!
//! glib-compile-resources is invoked directly rather than through the
//! glib-build-tools crate: it is a two-line call and this keeps the dependency
//! tree (and the vendored crate set) unchanged.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    compile_icons(&out_dir);
    index_fonts(&out_dir);
}

fn compile_icons(out_dir: &Path) {
    let target = out_dir.join("trayplay.gresource");

    // Recursive, so adding an icon under data/icons triggers a rebuild without
    // listing it here as well as in the XML.
    println!("cargo:rerun-if-changed=data/icons");

    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir=data/icons")
        .arg("--target")
        .arg(&target)
        .arg("data/icons/trayplay.gresource.xml")
        .status()
        .expect("running glib-compile-resources (provided by glib; in Nix it is a nativeBuildInput)");

    assert!(status.success(), "glib-compile-resources failed: {status}");
}

/// Writes a table of the fonts in `data/fonts` for `src/fonts.rs` to include.
///
/// Dropping a font in that directory is meant to be the whole procedure, so the
/// family name cannot come from a hand-maintained list. It is read out of the
/// font's own `name` table here rather than by shelling out to `fc-scan`: that
/// would put fontconfig's binaries in the build closure for a job that is forty
/// lines of parsing, and it would have to be added to the Nix
/// `nativeBuildInputs` as well.
fn index_fonts(out_dir: &Path) {
    println!("cargo:rerun-if-changed=data/fonts");

    let mut fonts: Vec<(String, String)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir("data/fonts") {
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                matches!(
                    path.extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase).as_deref(),
                    Some("ttf" | "otf" | "ttc")
                )
            })
            .collect();
        // Sorted, so the generated table (and with it which album gets which
        // font) does not depend on directory order.
        paths.sort();

        for path in paths {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("font file name")
                .to_string();
            let bytes = std::fs::read(&path).expect("reading a bundled font");
            match family_name(&bytes) {
                Some(family) => fonts.push((name, family)),
                // Not fatal: a font whose family cannot be read is simply not
                // offered, and the panel falls back to the default family.
                None => println!(
                    "cargo:warning=cannot read a family name from data/fonts/{name}, skipping it"
                ),
            }
        }
    }

    let mut generated = String::from("pub const FONTS: &[Font] = &[\n");
    for (file, family) in &fonts {
        writeln!(
            generated,
            "    Font {{ file: {file:?}, family: {family:?}, bytes: include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/data/fonts/{file}\")) }},"
        )
        .expect("building the font table");
    }
    generated.push_str("];\n");

    std::fs::write(out_dir.join("fonts.rs"), generated).expect("writing the font table");
}

/// Family name from a font's `name` table, preferring the typographic family.
///
/// Only the parts of the format that answer this one question are handled. A
/// font that does not parse is skipped rather than failing the build.
fn family_name(bytes: &[u8]) -> Option<String> {
    // A collection (.ttc) is a header followed by offsets to ordinary font
    // tables; the first one is as good as any for a name.
    let base = if bytes.get(..4)? == b"ttcf" {
        read_u32(bytes, 12)? as usize
    } else {
        0
    };

    let table_count = read_u16(bytes, base + 4)? as usize;
    let mut name_table = None;
    for index in 0..table_count {
        let record = base + 12 + index * 16;
        if bytes.get(record..record + 4)? == b"name" {
            name_table = Some(read_u32(bytes, record + 8)? as usize);
            break;
        }
    }
    let name_table = name_table?;

    let record_count = read_u16(bytes, name_table + 2)? as usize;
    let storage = name_table + read_u16(bytes, name_table + 4)? as usize;

    // nameID 16 is the typographic family ("Inter"), 1 the legacy one, which for
    // a font with more than four weights says things like "Inter Semibold". Take
    // 16 when it is there.
    let mut best: Option<(u16, String)> = None;
    for index in 0..record_count {
        let record = name_table + 6 + index * 12;
        let platform = read_u16(bytes, record)?;
        let name_id = read_u16(bytes, record + 6)?;
        if name_id != 1 && name_id != 16 {
            continue;
        }

        let length = read_u16(bytes, record + 8)? as usize;
        let offset = read_u16(bytes, record + 10)? as usize;
        let raw = bytes.get(storage + offset..storage + offset + length)?;

        // Platform 3 (Windows) and platform 0 (Unicode) are UTF-16BE; platform 1
        // (Macintosh) is a single-byte encoding whose ASCII range is all a family
        // name realistically uses.
        let decoded = if platform == 1 {
            raw.iter().map(|byte| *byte as char).collect()
        } else {
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16(&units).ok()?
        };

        let decoded = decoded.trim().to_string();
        if decoded.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(id, _)| *id < name_id) {
            best = Some((name_id, decoded));
        }
    }

    best.map(|(_, family)| family)
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
