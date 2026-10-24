// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal reader for KiCad s-expression files (enough for netlists).

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

impl Sexp {
    /// For `(name …)`, returns `name`.
    pub fn head(&self) -> Option<&str> {
        match self {
            Sexp::List(items) => match items.first() {
                Some(Sexp::Atom(a)) => Some(a),
                _ => None,
            },
            Sexp::Atom(_) => None,
        }
    }

    /// Child lists whose head is `name`.
    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Sexp> + 'a {
        let items: &[Sexp] = match self {
            Sexp::List(items) => items,
            Sexp::Atom(_) => &[],
        };
        items.iter().filter(move |c| c.head() == Some(name))
    }

    pub fn child(&self, name: &str) -> Option<&Sexp> {
        match self {
            Sexp::List(items) => items.iter().find(|c| c.head() == Some(name)),
            Sexp::Atom(_) => None,
        }
    }

    /// For `(name value …)`, returns `value`.
    pub fn value(&self) -> Option<&str> {
        match self {
            Sexp::List(items) => match items.get(1) {
                Some(Sexp::Atom(a)) => Some(a),
                _ => None,
            },
            Sexp::Atom(_) => None,
        }
    }

    /// Shorthand for `self.child(name)?.value()`.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.child(name)?.value()
    }
}

pub(crate) fn parse(text: &str) -> Result<Sexp, String> {
    let mut stack: Vec<Vec<Sexp>> = Vec::new();
    let mut root = None;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '(' => stack.push(Vec::new()),
            ')' => {
                let list = Sexp::List(stack.pop().ok_or(format!("unbalanced ')' at byte {i}"))?);
                match stack.last_mut() {
                    Some(parent) => parent.push(list),
                    None if root.is_none() => root = Some(list),
                    None => return Err(format!("more than one top-level list (byte {i})")),
                }
            }
            '"' => {
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some((_, '\\')) => match chars.next() {
                            Some((_, 'n')) => s.push('\n'),
                            Some((_, other)) => s.push(other),
                            None => return Err("unterminated escape".into()),
                        },
                        Some((_, '"')) => break,
                        Some((_, other)) => s.push(other),
                        None => return Err(format!("unterminated string starting at byte {i}")),
                    }
                }
                stack.last_mut().ok_or("string outside a list")?.push(Sexp::Atom(s));
            }
            c if c.is_whitespace() => {}
            _ => {
                let mut s = String::from(c);
                while let Some(&(_, n)) = chars.peek() {
                    if n.is_whitespace() || n == '(' || n == ')' {
                        break;
                    }
                    s.push(n);
                    chars.next();
                }
                stack.last_mut().ok_or("atom outside a list")?.push(Sexp::Atom(s));
            }
        }
    }
    if !stack.is_empty() {
        return Err("unbalanced '(' at end of input".into());
    }
    root.ok_or_else(|| "empty input".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_lists_strings_and_escapes() {
        let s = parse(r#"(export (version "E") (net (code "1") (name "/A \"quoted\""))) "#).unwrap();
        assert_eq!(s.head(), Some("export"));
        assert_eq!(s.field("version"), Some("E"));
        assert_eq!(s.child("net").unwrap().field("name"), Some("/A \"quoted\""));
        assert!(parse("(a (b)").is_err());
        assert!(parse("(a))").is_err());
    }
}
