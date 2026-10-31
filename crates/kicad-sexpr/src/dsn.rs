// SPDX-License-Identifier: GPL-3.0-or-later
//! The Specctra design file (`.dsn`) KiCad exports for an autorouter, edited as text (Phase 14.8):
//! which nets the router is asked to route, and which of the tracks already on the board it may
//! rip up. The file is scanned, not parsed, so every byte that is not changed is kept.
//!
//! What KiCad writes (checked on 10.99): the existing tracks and vias as
//! `(wire (path F.Cu 200 x y x y …)(net NAME)(type protect))` and `(via "name" x y (net NAME)(type
//! protect))` in `(wiring …)`, and each net as `(net NAME (pins A-1 B-2 …))` in `(network …)`. A
//! protected wire is kept as it is by the router, and written again in its session.

/// Units per millimetre for the file's `(resolution <unit> <n>)`.
pub fn units_per_mm(text: &str) -> f64 {
    let Some(at) = text.find("(resolution") else {
        return 10_000.0;
    };
    let rest = &text[at + "(resolution".len()..];
    let end = rest.find(')').unwrap_or(rest.len());
    let mut parts = rest[..end].split_whitespace();
    let unit = parts.next().unwrap_or("um");
    let n: f64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    match unit {
        "mm" => n,
        "mil" => n / 0.0254,
        "inch" => n / 25.4,
        _ => 1000.0 * n,
    }
}

/// One track or via of the `(wiring …)` section.
#[derive(Debug, Clone, PartialEq)]
pub struct WiringItem {
    /// Byte range of the whole `(wire …)` or `(via …)`.
    pub range: std::ops::Range<usize>,
    pub is_via: bool,
    pub net: String,
    pub layer: String,
    pub width: i64,
    /// The path's points, or the via's place, in the file's units, y up.
    pub points: Vec<(i64, i64)>,
}

/// The index just past the `)` that closes the `(` at `open`, skipping quoted strings.
fn close_of(text: &str, open: usize) -> usize {
    let (mut depth, mut quoted) = (0usize, false);
    for (i, c) in text[open..].char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return open + i + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// The quoted or bare token that starts `text`, and what is left after it.
fn token(text: &str) -> (String, &str) {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('"') {
        let end = rest.find('"').unwrap_or(rest.len());
        let after = rest.get(end + 1..).unwrap_or("");
        return (rest[..end].to_owned(), after);
    }
    let end = text
        .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .unwrap_or(text.len());
    (text[..end].to_owned(), &text[end..])
}

/// The value of `(key value)` inside `item`.
fn field(item: &str, key: &str) -> Option<String> {
    let at = item.find(&format!("({key} "))?;
    let (value, _) = token(&item[at + key.len() + 2..]);
    Some(value)
}

/// Every track and via in the file's `(wiring …)`, in order.
pub fn wiring_items(text: &str) -> Vec<WiringItem> {
    let Some(start) = text.find("(wiring") else {
        return vec![];
    };
    let end = close_of(text, start);
    let mut out = Vec::new();
    let mut at = start + "(wiring".len();
    while at < end {
        let rest = &text[at..end];
        let Some(next) = rest.find(['(']) else { break };
        let open = at + next;
        let close = close_of(text, open);
        let item = &text[open..close];
        let numbers = |s: &str| -> Vec<i64> { s.split_whitespace().filter_map(|n| n.parse().ok()).collect() };
        if item.starts_with("(wire") {
            // `(path LAYER WIDTH x y x y …)`
            if let Some(p) = item.find("(path ") {
                let inner = &item[p + "(path ".len()..];
                let inner = &inner[..inner.find(')').unwrap_or(inner.len())];
                let (layer, rest) = token(inner);
                let nums = numbers(rest);
                if let Some((width, coords)) = nums.split_first() {
                    out.push(WiringItem {
                        range: open..close,
                        is_via: false,
                        net: field(item, "net").unwrap_or_default(),
                        layer,
                        width: *width,
                        points: coords.as_chunks::<2>().0.iter().map(|c| (c[0], c[1])).collect(),
                    });
                }
            }
        } else if let Some(after_via) = item.strip_prefix("(via") {
            // `(via "name" x y (net NAME)(type protect))`
            let (_, rest) = token(after_via);
            let nums = numbers(&rest[..rest.find('(').unwrap_or(rest.len())]);
            if let [x, y] = nums[..] {
                out.push(WiringItem {
                    range: open..close,
                    is_via: true,
                    net: field(item, "net").unwrap_or_default(),
                    layer: String::new(),
                    width: 0,
                    points: vec![(x, y)],
                });
            }
        }
        at = close;
    }
    out
}

/// The text without the wiring items `keep` says no to (the router starts those from nothing).
/// Returns the new text and how many were taken out.
pub fn without_wiring(text: &str, keep: impl Fn(&WiringItem) -> bool) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let (mut cursor, mut removed) = (0usize, 0usize);
    for item in wiring_items(text) {
        if keep(&item) {
            continue;
        }
        let mut from = item.range.start;
        // The whitespace before the item goes with it.
        while from > cursor && text.as_bytes()[from - 1].is_ascii_whitespace() {
            from -= 1;
        }
        out.push_str(&text[cursor..from]);
        cursor = item.range.end;
        removed += 1;
    }
    out.push_str(&text[cursor..]);
    (out, removed)
}

/// The names of the nets in `(network …)`.
pub fn network_nets(text: &str) -> Vec<String> {
    let Some(start) = text.find("(network") else {
        return vec![];
    };
    let end = close_of(text, start);
    let mut out = Vec::new();
    let mut at = start + "(network".len();
    while at < end {
        let Some(next) = text[at..end].find('(') else { break };
        let open = at + next;
        let close = close_of(text, open);
        if text[open..].starts_with("(net ") {
            out.push(token(&text[open + "(net ".len()..]).0);
        }
        at = close;
    }
    out
}

/// The text with the pins of the nets named in `nets` taken out of `(network …)`: the router has
/// nothing to connect for them, and their pads stay where they are as obstacles. Returns the new
/// text and how many nets were emptied.
pub fn without_pins_of(text: &str, nets: &[String]) -> (String, usize) {
    let Some(start) = text.find("(network") else {
        return (text.to_owned(), 0);
    };
    let end = close_of(text, start);
    let mut out = String::with_capacity(text.len());
    let (mut cursor, mut emptied) = (0usize, 0usize);
    let mut at = start + "(network".len();
    while at < end {
        let Some(next) = text[at..end].find('(') else { break };
        let open = at + next;
        let close = close_of(text, open);
        if text[open..].starts_with("(net ") {
            let (name, _) = token(&text[open + "(net ".len()..]);
            if nets.contains(&name) {
                if let Some(p) = text[open..close].find("(pins") {
                    let pins_open = open + p;
                    let pins_close = close_of(text, pins_open);
                    out.push_str(&text[cursor..pins_open]);
                    cursor = pins_close;
                    emptied += 1;
                }
            }
        }
        at = close;
    }
    out.push_str(&text[cursor..]);
    (out, emptied)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSN: &str = r#"(pcb board.dsn
  (resolution um 10)
  (structure (rule (width 200)))
  (network
    (net /VBUS
      (pins J1-1 F1-1)
    )
    (net GND
      (pins J1-2 U1-1 C1-2)
    )
    (net "Net-(D2-A)"
      (pins R3-2 D2-2)
    )
    (class kicad_default /VBUS GND "Net-(D2-A)"
      (circuit (use_via "Via[0-1]_600:300_um"))
      (rule (width 200) (clearance 200))
    )
  )
  (wiring
    (wire (path F.Cu 200  102600 -96000  102000 -92000)(net /VBUS)(type protect))
    (wire (path B.Cu 200  132030 -86970  128030 -86970)(net GND)(type protect))
    (wire (path F.Cu 200  119288 -86000  119325 -88500)(net "Net-(D2-A)")(type protect))
    (via "Via[0-1]_600:300_um"  132030 -86970 (net GND)(type protect))
  )
)"#;

    #[test]
    fn existing_tracks_and_vias_are_read() {
        let items = wiring_items(DSN);
        assert_eq!(items.len(), 4);
        assert_eq!(
            (
                items[0].net.as_str(),
                items[0].layer.as_str(),
                items[0].width,
                items[0].points.clone()
            ),
            ("/VBUS", "F.Cu", 200, vec![(102600, -96000), (102000, -92000)])
        );
        assert_eq!(items[2].net, "Net-(D2-A)", "a quoted name is read without its quotes");
        assert!(items[3].is_via);
        assert_eq!(
            (items[3].net.as_str(), items[3].points.clone()),
            ("GND", vec![(132030, -86970)])
        );
        assert_eq!(network_nets(DSN), ["/VBUS", "GND", "Net-(D2-A)"]);
        assert!(wiring_items("(pcb)").is_empty());
    }

    #[test]
    fn the_units_are_read() {
        assert_eq!(units_per_mm(DSN), 10_000.0);
        assert_eq!(units_per_mm("(pcb (resolution mm 1000))"), 1000.0);
        assert_eq!(units_per_mm("(pcb)"), 10_000.0);
    }

    #[test]
    fn some_wiring_is_taken_out_and_the_rest_is_kept_byte_for_byte() {
        let (out, removed) = without_wiring(DSN, |i| i.net == "GND");
        assert_eq!(removed, 2);
        assert!(out.contains("(net GND)"), "the wire of GND stays");
        assert!(!out.contains("/VBUS)(type"), "{out}");
        assert!(!out.contains("Net-(D2-A))(type"));
        let kept = wiring_items(&out);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|i| i.net == "GND"));
        // Nothing to take out: the same text.
        assert_eq!(without_wiring(DSN, |_| true), (DSN.to_owned(), 0));
        // All of it.
        let (none, n) = without_wiring(DSN, |_| false);
        assert_eq!((n, wiring_items(&none).len()), (4, 0));
        assert!(none.contains("(wiring"), "{none}");
    }

    #[test]
    fn a_net_the_router_should_leave_has_no_pins() {
        let (out, emptied) = without_pins_of(DSN, &["GND".to_owned(), "Net-(D2-A)".to_owned(), "NOPE".to_owned()]);
        assert_eq!(emptied, 2);
        assert!(
            out.contains("(net GND\n      \n    )") || out.contains("(net GND"),
            "{out}"
        );
        assert!(!out.contains("J1-2"), "{out}");
        assert!(!out.contains("R3-2"));
        assert!(out.contains("(pins J1-1 F1-1)"), "the others keep theirs");
        assert_eq!(network_nets(&out), network_nets(DSN), "the nets are still there");
        assert!(out.contains("(class kicad_default"), "{out}");
        assert_eq!(without_pins_of(DSN, &[]).0, DSN);
    }
}
