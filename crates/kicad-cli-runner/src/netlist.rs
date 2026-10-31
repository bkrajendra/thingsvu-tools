// SPDX-License-Identifier: GPL-3.0-or-later
//! `kicad-cli sch export netlist --format kicadsexpr` with the result parsed into components and
//! nets. This is independent evidence of connectivity: KiCad's own netlister, not our model.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::install::KicadInstall;
use crate::run::{check, run, CliError};
use kicad_sexpr::Node as Sexp;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Netlist {
    /// The tool line, e.g. `Eeschema 10.99.0-5896-gd033635628`.
    pub tool: String,
    pub components: Vec<NetlistComponent>,
    pub nets: Vec<Net>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetlistComponent {
    pub reference: String,
    pub value: String,
    /// `library:part` as recorded in the netlist's `libsource`.
    pub lib_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Net {
    pub name: String,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Node {
    pub reference: String,
    pub pin: String,
}

impl Netlist {
    pub fn parse(text: &str) -> Result<Self, String> {
        let root = kicad_sexpr::parse(text).map_err(|e| e.to_string())?;
        if root.head() != Some("export") {
            return Err("not a KiCad netlist: expected (export …)".into());
        }
        let tool = root
            .child("design")
            .and_then(|d| d.child_text("tool"))
            .unwrap_or_default()
            .to_owned();
        let components = root
            .child("components")
            .map(|c| c.children("comp").map(component).collect())
            .unwrap_or_default();
        let nets = root
            .child("nets")
            .map(|n| {
                n.children("net")
                    .map(|net| Net {
                        name: net.child_text("name").unwrap_or_default().to_owned(),
                        nodes: net
                            .children("node")
                            .map(|node| Node {
                                reference: node.child_text("ref").unwrap_or_default().to_owned(),
                                pin: node.child_text("pin").unwrap_or_default().to_owned(),
                            })
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self { tool, components, nets })
    }

    pub fn net(&self, name: &str) -> Option<&Net> {
        self.nets.iter().find(|n| n.name == name)
    }

    /// Nets as (name, sorted set of `REF.PIN`) pairs, for order-independent comparison.
    pub fn topology(&self) -> BTreeSet<(String, BTreeSet<String>)> {
        self.nets
            .iter()
            .map(|n| {
                (
                    n.name.clone(),
                    n.nodes.iter().map(|p| format!("{}.{}", p.reference, p.pin)).collect(),
                )
            })
            .collect()
    }
}

fn component(comp: &Sexp) -> NetlistComponent {
    let lib = comp.child("libsource");
    NetlistComponent {
        reference: comp.child_text("ref").unwrap_or_default().to_owned(),
        value: comp.child_text("value").unwrap_or_default().to_owned(),
        lib_source: format!(
            "{}:{}",
            lib.and_then(|l| l.child_text("lib")).unwrap_or_default(),
            lib.and_then(|l| l.child_text("part")).unwrap_or_default()
        ),
    }
}

/// Exports the netlist of `schematic` to `output` and parses it.
pub async fn export_netlist(
    install: &KicadInstall,
    schematic: &Path,
    output: &Path,
    timeout: Duration,
) -> Result<Netlist, CliError> {
    let args: Vec<OsString> = vec![
        "sch".into(),
        "export".into(),
        "netlist".into(),
        "--format".into(),
        "kicadsexpr".into(),
        "--output".into(),
        output.into(),
        schematic.into(),
    ];
    check(run(&install.cli, &args, timeout).await?, schematic)?;
    let text = std::fs::read_to_string(output).map_err(|e| CliError::BadReport {
        path: output.to_owned(),
        detail: e.to_string(),
    })?;
    Netlist::parse(&text).map_err(|detail| CliError::BadReport {
        path: output.to_owned(),
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_netlist_from_kicad_10_99() {
        let netlist = Netlist::parse(include_str!("../tests/data/netlist-two-resistor.net")).unwrap();
        assert!(netlist.tool.starts_with("Eeschema 10.99.0"));
        let refs: Vec<_> = netlist
            .components
            .iter()
            .map(|c| (c.reference.as_str(), c.value.as_str()))
            .collect();
        assert_eq!(refs, [("R1", "10k"), ("R2", "10k")]);
        // A duplicated library symbol would show up here as ":R_1".
        assert!(
            netlist.components.iter().all(|c| c.lib_source == "Device:R"),
            "{:?}",
            netlist.components
        );
        let expected: BTreeSet<(String, BTreeSet<String>)> = [
            ("/INPUT", vec!["R1.1"]),
            ("/MID", vec!["R1.2", "R2.1"]),
            ("/OUTPUT", vec!["R2.2"]),
        ]
        .into_iter()
        .map(|(n, pins)| (n.to_owned(), pins.into_iter().map(str::to_owned).collect()))
        .collect();
        assert_eq!(netlist.topology(), expected);
    }

    #[test]
    fn reads_quoted_names_and_rejects_other_files() {
        let text = r#"(export (version "E") (nets (net (code "1") (name "/A \"q\"") (node (ref "R1") (pin "1")))))"#;
        let netlist = Netlist::parse(text).unwrap();
        assert_eq!(netlist.nets[0].name, "/A \"q\"");
        assert_eq!(netlist.nets[0].nodes[0].reference, "R1");
        assert!(Netlist::parse("(kicad_sch (version 1))").is_err());
        assert!(Netlist::parse("(export (nets)").is_err());
    }
}
