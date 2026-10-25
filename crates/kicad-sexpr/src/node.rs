// SPDX-License-Identifier: GPL-3.0-or-later
//! A parsed s-expression that remembers where it came from, so edits can be made to the original
//! text and every byte they do not touch stays as it was.

use std::ops::Range;

/// Nesting deeper than this is rejected (KiCad files nest about ten levels).
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A bare token such as `symbol`, `20260306`, or `yes`, as written.
    Atom(String),
    /// A quoted string, unescaped.
    Str(String),
    List(Vec<Node>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Byte range in the parsed text; `0..0` for a node built in code.
    pub span: Range<usize>,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("s-expression error at byte {offset}: {message}")]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

fn fail<T>(offset: usize, message: &str) -> Result<T, ParseError> {
    Err(ParseError {
        offset,
        message: message.to_owned(),
    })
}

/// Parses text holding exactly one list.
pub fn parse(text: &str) -> Result<Node, ParseError> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut stack: Vec<(usize, Vec<Node>)> = Vec::new();
    let mut root: Option<Node> = None;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'(' => {
                if stack.len() >= MAX_DEPTH {
                    return fail(i, "nested too deeply");
                }
                stack.push((i, Vec::new()));
                i += 1;
            }
            b')' => {
                let Some((start, children)) = stack.pop() else {
                    return fail(i, "unbalanced ')'");
                };
                let node = Node {
                    span: start..i + 1,
                    kind: Kind::List(children),
                };
                i += 1;
                match stack.last_mut() {
                    Some((_, siblings)) => siblings.push(node),
                    None if root.is_none() => root = Some(node),
                    None => return fail(start, "more than one top-level list"),
                }
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut value = Vec::new();
                loop {
                    match bytes.get(i) {
                        None => return fail(start, "unterminated string"),
                        Some(b'"') => break,
                        Some(b'\\') => {
                            i += 1;
                            match bytes.get(i) {
                                None => return fail(start, "unterminated string"),
                                Some(b'n') => value.push(b'\n'),
                                Some(b'r') => value.push(b'\r'),
                                Some(b't') => value.push(b'\t'),
                                Some(&c) => value.push(c),
                            }
                            i += 1;
                        }
                        Some(&c) => {
                            value.push(c);
                            i += 1;
                        }
                    }
                }
                i += 1;
                let Some((_, siblings)) = stack.last_mut() else {
                    return fail(start, "string outside a list");
                };
                siblings.push(Node {
                    span: start..i,
                    // Quotes and backslashes are ASCII, so the content is still valid UTF-8.
                    kind: Kind::Str(String::from_utf8(value).map_err(|_| ParseError {
                        offset: start,
                        message: "string is not valid UTF-8".into(),
                    })?),
                });
            }
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'"') {
                    i += 1;
                }
                let Some((_, siblings)) = stack.last_mut() else {
                    return fail(start, "token outside a list");
                };
                siblings.push(Node {
                    span: start..i,
                    kind: Kind::Atom(text[start..i].to_owned()),
                });
            }
        }
    }
    if let Some((start, _)) = stack.last() {
        return fail(*start, "unclosed '('");
    }
    match root {
        Some(root) => Ok(root),
        None => fail(0, "no list found"),
    }
}

/// Quotes a string the way KiCad does.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Nanometres as KiCad writes millimetres: exact, with trailing zeros trimmed.
pub fn mm(nm: i64) -> String {
    let sign = if nm < 0 { "-" } else { "" };
    let (whole, frac) = (nm.unsigned_abs() / 1_000_000, nm.unsigned_abs() % 1_000_000);
    if frac == 0 {
        return format!("{sign}{whole}");
    }
    let frac = format!("{frac:06}");
    format!("{sign}{whole}.{}", frac.trim_end_matches('0'))
}

impl Node {
    pub fn atom(s: impl Into<String>) -> Node {
        Node {
            span: 0..0,
            kind: Kind::Atom(s.into()),
        }
    }

    pub fn string(s: impl Into<String>) -> Node {
        Node {
            span: 0..0,
            kind: Kind::Str(s.into()),
        }
    }

    pub fn list(items: Vec<Node>) -> Node {
        Node {
            span: 0..0,
            kind: Kind::List(items),
        }
    }

    pub fn items(&self) -> &[Node] {
        match &self.kind {
            Kind::List(items) => items,
            _ => &[],
        }
    }

    pub fn is_list(&self) -> bool {
        matches!(self.kind, Kind::List(_))
    }

    /// The content of an atom or a string.
    pub fn text(&self) -> Option<&str> {
        match &self.kind {
            Kind::Atom(s) | Kind::Str(s) => Some(s),
            Kind::List(_) => None,
        }
    }

    /// The first element of a list, when it is an atom: `symbol` for `(symbol "R" …)`.
    pub fn head(&self) -> Option<&str> {
        match self.items().first()?.kind {
            Kind::Atom(ref s) => Some(s),
            _ => None,
        }
    }

    /// The `i`-th element after the head.
    pub fn arg(&self, i: usize) -> Option<&Node> {
        self.items().get(i + 1)
    }

    pub fn arg_text(&self, i: usize) -> Option<&str> {
        self.arg(i)?.text()
    }

    pub fn arg_f64(&self, i: usize) -> Option<f64> {
        self.arg_text(i)?.parse().ok()
    }

    /// The first child list with this head.
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.items().iter().find(|n| n.head() == Some(name))
    }

    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.items().iter().filter(move |n| n.head() == Some(name))
    }

    /// First argument of the child `name`: `child_text("uuid")` for `(uuid "…")`.
    /// The first argument of the child `name`, as a number.
    pub fn child_f64(&self, name: &str) -> Option<f64> {
        self.child(name)?.arg_f64(0)
    }

    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name)?.arg_text(0)
    }

    /// Text in KiCad's layout: leading atoms on the first line, each nested list on its own line.
    pub fn render(&self, depth: usize) -> String {
        let mut out = String::new();
        self.write(&mut out, depth);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        match &self.kind {
            Kind::Atom(s) => out.push_str(s),
            Kind::Str(s) => out.push_str(&quote(s)),
            Kind::List(items) => {
                out.push('(');
                let split = items.iter().position(Node::is_list).unwrap_or(items.len());
                for (i, item) in items[..split].iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    item.write(out, depth);
                }
                for item in &items[split..] {
                    out.push('\n');
                    out.push_str(&"\t".repeat(depth + 1));
                    item.write(out, depth + 1);
                }
                if split < items.len() {
                    out.push('\n');
                    out.push_str(&"\t".repeat(depth));
                }
                out.push(')');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_lists_with_spans() {
        let text = "(a (b \"x y\" 1.5) (c))";
        let root = parse(text).unwrap();
        assert_eq!(root.head(), Some("a"));
        let b = root.child("b").unwrap();
        assert_eq!(&text[b.span.clone()], "(b \"x y\" 1.5)");
        assert_eq!(b.arg_text(0), Some("x y"));
        assert_eq!(b.arg_f64(1), Some(1.5));
        assert!(root.child("c").unwrap().items().len() == 1);
    }

    #[test]
    fn unescapes_and_requotes_strings() {
        let root = parse(r#"(p "a \"q\" \\ \n z")"#).unwrap();
        let s = root.arg_text(0).unwrap();
        assert_eq!(s, "a \"q\" \\ \n z");
        assert_eq!(quote(s), r#""a \"q\" \\ \n z""#);
    }

    #[test]
    fn rejects_malformed_text() {
        for bad in ["", "(a", "a)", "(a))", "(a) (b)", "(a \"x)", "x (a)"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(parse(&format!("{}{}", "(".repeat(200), ")".repeat(200))).is_err());
    }

    #[test]
    fn writes_millimetres_exactly() {
        assert_eq!(mm(50_800_000), "50.8");
        assert_eq!(mm(-3_810_000), "-3.81");
        assert_eq!(mm(0), "0");
        assert_eq!(mm(-500), "-0.0005");
        assert_eq!(mm(1_000_000), "1");
    }

    #[test]
    fn renders_in_kicads_layout_and_reparses() {
        let root = parse("(symbol \"R\" (pin 1 (at 0 3.81 270)) (length 1))").unwrap();
        let text = root.render(1);
        assert_eq!(
            text,
            "(symbol \"R\"\n\t\t(pin 1\n\t\t\t(at 0 3.81 270)\n\t\t)\n\t\t(length 1)\n\t)"
        );
        let again = parse(&text).unwrap();
        assert_eq!(again.child("pin").unwrap().child("at").unwrap().arg_f64(1), Some(3.81));
    }
}
