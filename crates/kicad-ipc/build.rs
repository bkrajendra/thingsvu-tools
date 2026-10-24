// SPDX-License-Identifier: GPL-3.0-or-later
//! Generates Rust types from the vendored KiCad API protos (see proto/README.md).

use std::path::{Path, PathBuf};

fn collect_protos(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_protos(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "proto") {
            out.push(path);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from("proto");
    let mut protos = Vec::new();
    collect_protos(&root, &mut protos)?;
    protos.sort();
    println!("cargo:rerun-if-changed=proto");

    let mut config = prost_build::Config::new();
    // Gives every message a `prost::Name` impl so it can be packed into / unpacked from `Any`.
    config.enable_type_names();
    config.type_name_domain(["."], "type.googleapis.com");
    config.include_file("kiapi.rs");
    config.compile_protos(&protos, &[root])?;
    Ok(())
}
