// SPDX-License-Identifier: GPL-3.0-or-later
//! A Specctra session (`.ses`) as an autorouter writes it (Phase 12.7): what it routed, per net,
//! for the plan the user sees before it is imported. Coordinates are in the session's resolution
//! (`(resolution um 10)`: tenths of a micrometre) with y up; only lengths and widths are used here.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::node::{parse, Node, ParseError};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SesNet {
    pub net: String,
    /// Straight track segments (a wire path of n points is n - 1 segments).
    pub segments: usize,
    pub vias: usize,
    pub length_mm: f64,
    pub layers: Vec<String>,
    /// The narrowest track, mm.
    pub min_width_mm: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SesSummary {
    pub nets: Vec<SesNet>,
    pub segments: usize,
    pub vias: usize,
    pub length_mm: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum SesError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("not a Specctra session: expected (session …)")]
    NotASession,
}

/// Units per millimetre for `(resolution <unit> <n>)`.
fn per_mm(res: Option<&Node>) -> f64 {
    let Some(r) = res else { return 10_000.0 };
    let n = r.arg_f64(1).unwrap_or(1.0);
    match r.arg_text(0) {
        Some("um") => 1000.0 * n,
        Some("mm") => n,
        Some("mil") => n / 0.0254,
        Some("inch") => n / 25.4,
        _ => 1000.0 * n,
    }
}

pub fn summarize(text: &str) -> Result<SesSummary, SesError> {
    let root = parse(text)?;
    if root.head() != Some("session") {
        return Err(SesError::NotASession);
    }
    let Some(routes) = root.child("routes") else {
        return Ok(SesSummary::default());
    };
    let scale = per_mm(routes.child("resolution"));
    let mut nets: BTreeMap<String, SesNet> = BTreeMap::new();
    if let Some(out) = routes.child("network_out") {
        for net in out.children("net") {
            let name = net.arg_text(0).unwrap_or_default().to_owned();
            let entry = nets.entry(name.clone()).or_insert_with(|| SesNet {
                net: name,
                min_width_mm: f64::INFINITY,
                ..Default::default()
            });
            for wire in net.children("wire") {
                let Some(path) = wire.child("path") else {
                    continue;
                };
                let layer = path.arg_text(0).unwrap_or_default().to_owned();
                let width = path.arg_f64(1).unwrap_or(0.0) / scale;
                let coords: Vec<f64> = (2..).map_while(|i| path.arg_f64(i)).collect();
                let points: Vec<(f64, f64)> = coords.as_chunks::<2>().0.iter().map(|c| (c[0], c[1])).collect();
                if points.len() < 2 {
                    continue;
                }
                entry.segments += points.len() - 1;
                entry.length_mm += points
                    .windows(2)
                    .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1) / scale)
                    .sum::<f64>();
                entry.min_width_mm = entry.min_width_mm.min(width);
                if !entry.layers.contains(&layer) {
                    entry.layers.push(layer);
                }
            }
            entry.vias += net.children("via").count();
        }
    }
    let mut out = SesSummary::default();
    for (_, mut n) in nets {
        if !n.min_width_mm.is_finite() {
            n.min_width_mm = 0.0;
        }
        n.length_mm = (n.length_mm * 100.0).round() / 100.0;
        n.min_width_mm = (n.min_width_mm * 1000.0).round() / 1000.0;
        out.segments += n.segments;
        out.vias += n.vias;
        out.length_mm += n.length_mm;
        out.nets.push(n);
    }
    out.length_mm = (out.length_mm * 100.0).round() / 100.0;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SES: &str = r#"(session board
  (base_design board)
  (routes
    (resolution um 10)
    (network_out
      (net "/VBUS"
        (wire (path F.Cu 2000 1020000 -920000 1031517 -920000))
        (wire (path F.Cu 2000 1080000 -961000 1069638 -961000 1031517 -922879))
      )
      (net GND
        (wire (path B.Cu 1500 0 0 30000 40000))
        (via "Via[0-1]_600:300_um" 0 0)
      )
    )
  )
)"#;

    #[test]
    fn a_session_is_summarized_per_net() {
        let s = summarize(SES).unwrap();
        assert_eq!(s.nets.len(), 2);
        let vbus = &s.nets[0];
        assert_eq!((vbus.net.as_str(), vbus.segments, vbus.vias), ("/VBUS", 3, 0));
        assert_eq!(vbus.min_width_mm, 0.2);
        let gnd = &s.nets[1];
        assert_eq!(
            (gnd.segments, gnd.vias, gnd.length_mm, gnd.min_width_mm),
            (1, 1, 5.0, 0.15)
        );
        assert_eq!(gnd.layers, vec!["B.Cu".to_owned()]);
        assert_eq!((s.segments, s.vias), (4, 1));
    }

    #[test]
    fn something_else_is_refused() {
        assert!(summarize("(kicad_pcb)").is_err());
    }
}
