// SPDX-License-Identifier: GPL-3.0-or-later
//! Validated identifiers. Values from an AI client are untrusted, so every identifier is checked
//! when it is constructed or deserialized.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("logical id '{0}' must start with a letter and contain only letters, digits, or '_' (max 64)")]
    LogicalId(String),
    #[error("'{0}' is not a KiCad object id (expected a UUID like 3b207621-7eae-4bd8-b196-812e883401a5)")]
    Kiid(String),
    #[error("library id '{0}' must be 'Library:Entry' with no empty parts (max 256 characters)")]
    LibraryId(String),
}

/// Identity of a component or net in a circuit spec. Stable across edits, unlike `R1`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LogicalId(String);

impl LogicalId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        let mut chars = value.chars();
        let valid = value.len() <= 64
            && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if valid {
            Ok(Self(value))
        } else {
            Err(IdError::LogicalId(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LogicalId {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<LogicalId> for String {
    fn from(id: LogicalId) -> Self {
        id.0
    }
}

impl fmt::Display for LogicalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A KiCad object id (KIID). The same value appears in `.kicad_sch` files and in IPC replies.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Kiid(String);

impl Kiid {
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        let groups: Vec<&str> = value.split('-').collect();
        let lengths = [8, 4, 4, 4, 12];
        let valid = groups.len() == lengths.len()
            && groups
                .iter()
                .zip(lengths)
                .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()));
        if valid {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(IdError::Kiid(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Kiid {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Kiid> for String {
    fn from(id: Kiid) -> Self {
        id.0
    }
}

impl fmt::Display for Kiid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A symbol library reference such as `Device:R`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LibraryId {
    library: String,
    entry: String,
}

impl LibraryId {
    pub fn parse(value: &str) -> Result<Self, IdError> {
        let err = || IdError::LibraryId(value.to_owned());
        if value.len() > 256 || value.chars().any(char::is_control) {
            return Err(err());
        }
        let (library, entry) = value.split_once(':').ok_or_else(err)?;
        if library.is_empty() || entry.is_empty() || entry.contains(':') {
            return Err(err());
        }
        Ok(Self {
            library: library.to_owned(),
            entry: entry.to_owned(),
        })
    }

    pub fn library(&self) -> &str {
        &self.library
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }
}

impl TryFrom<String> for LibraryId {
    type Error = IdError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<LibraryId> for String {
    fn from(id: LibraryId) -> Self {
        id.to_string()
    }
}

impl fmt::Display for LibraryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.library, self.entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_ids() {
        assert!(LogicalId::new("resistor_input").is_ok());
        assert!(LogicalId::new("R1").is_ok());
        for bad in ["", "1abc", "has space", "dash-ed", &"a".repeat(65)] {
            assert!(LogicalId::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn kiids_are_normalized_to_lowercase() {
        let id = Kiid::new("3B207621-7EAE-4BD8-B196-812E883401A5").unwrap();
        assert_eq!(id.as_str(), "3b207621-7eae-4bd8-b196-812e883401a5");
        assert!(Kiid::new("not-a-uuid").is_err());
        assert!(Kiid::new("3b207621-7eae-4bd8-b196-812e883401a").is_err());
    }

    #[test]
    fn library_ids() {
        let id = LibraryId::parse("Device:R").unwrap();
        assert_eq!((id.library(), id.entry()), ("Device", "R"));
        assert_eq!(id.to_string(), "Device:R");
        for bad in ["Device", ":R", "Device:", "a:b:c", "Dev\nice:R"] {
            assert!(LibraryId::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn serde_rejects_invalid_values() {
        assert!(serde_json::from_str::<LogicalId>("\"9bad\"").is_err());
        assert_eq!(
            serde_json::from_str::<LibraryId>("\"Device:R\"").unwrap().to_string(),
            "Device:R"
        );
    }
}
