//! Document model for MSearchDB.
//!
//! A [`Document`] is the primary unit of data stored in MSearchDB. Each document
//! has a unique [`DocumentId`] and a map of named fields to typed [`FieldValue`]s.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::document::{Document, DocumentId, FieldValue};
//!
//! let doc = Document::new(DocumentId::new("doc-1"))
//!     .with_field("title", FieldValue::Text("Hello World".into()))
//!     .with_field("views", FieldValue::Number(42.0));
//!
//! assert_eq!(doc.id.as_str(), "doc-1");
//! assert_eq!(doc.fields.len(), 2);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// DocumentId — newtype over String for type safety
// ---------------------------------------------------------------------------

/// A unique identifier for a document.
///
/// Uses the *newtype pattern* to prevent accidental mixing of raw strings
/// with document identifiers at compile time.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentId(String);

impl DocumentId {
    /// Create a new `DocumentId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Generate a random V4 UUID-based document id.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Borrow the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DocumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for DocumentId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for DocumentId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

// ---------------------------------------------------------------------------
// FieldValue — typed values within a document
// ---------------------------------------------------------------------------

/// A typed value that can be stored in a document field.
///
/// Marked `#[non_exhaustive]` so that new variants (e.g., `Date`, `Geo`)
/// can be added in future versions without a breaking change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FieldValue {
    /// UTF-8 text value, eligible for full-text indexing.
    Text(String),
    /// IEEE 754 double-precision number.
    Number(f64),
    /// Boolean flag.
    Boolean(bool),
    /// Ordered list of nested field values.
    Array(Vec<FieldValue>),
}

impl fmt::Display for FieldValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldValue::Text(s) => write!(f, "\"{}\"", s),
            FieldValue::Number(n) => write!(f, "{}", n),
            FieldValue::Boolean(b) => write!(f, "{}", b),
            FieldValue::Array(arr) => {
                write!(f, "[")?;
                for (i, v) in arr.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
        }
    }
}

impl From<String> for FieldValue {
    fn from(s: String) -> Self {
        FieldValue::Text(s)
    }
}

impl From<&str> for FieldValue {
    fn from(s: &str) -> Self {
        FieldValue::Text(s.to_owned())
    }
}

impl From<f64> for FieldValue {
    fn from(n: f64) -> Self {
        FieldValue::Number(n)
    }
}

impl From<i64> for FieldValue {
    fn from(n: i64) -> Self {
        FieldValue::Number(n as f64)
    }
}

impl From<bool> for FieldValue {
    fn from(b: bool) -> Self {
        FieldValue::Boolean(b)
    }
}

impl From<Vec<FieldValue>> for FieldValue {
    fn from(v: Vec<FieldValue>) -> Self {
        FieldValue::Array(v)
    }
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// A document stored in MSearchDB.
///
/// Each document is a schemaless collection of named fields, identified by a
/// unique [`DocumentId`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Unique identifier for this document.
    pub id: DocumentId,
    /// Map of field names to typed values.
    pub fields: HashMap<String, FieldValue>,
}

impl Document {
    /// Create a new empty document with the given id.
    pub fn new(id: DocumentId) -> Self {
        Self {
            id,
            fields: HashMap::new(),
        }
    }

    /// Builder-style method to add a field.
    pub fn with_field(mut self, name: impl Into<String>, value: FieldValue) -> Self {
        self.fields.insert(name.into(), value);
        self
    }

    /// Insert or update a field value, returning the previous value if any.
    pub fn set_field(&mut self, name: impl Into<String>, value: FieldValue) -> Option<FieldValue> {
        self.fields.insert(name.into(), value)
    }

    /// Retrieve a field value by name.
    pub fn get_field(&self, name: &str) -> Option<&FieldValue> {
        self.fields.get(name)
    }
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Document({})", self.id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_id_newtype_equality() {
        let a = DocumentId::new("abc");
        let b = DocumentId::from("abc".to_string());
        let c: DocumentId = "abc".into();
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn document_id_display() {
        let id = DocumentId::new("doc-42");
        assert_eq!(format!("{}", id), "doc-42");
        assert_eq!(id.as_str(), "doc-42");
    }

    #[test]
    fn document_id_generate_is_unique() {
        let a = DocumentId::generate();
        let b = DocumentId::generate();
        assert_ne!(a, b);
    }

    #[test]
    fn field_value_display() {
        assert_eq!(format!("{}", FieldValue::Text("hello".into())), "\"hello\"");
        assert_eq!(format!("{}", FieldValue::Number(3.14)), "3.14");
        assert_eq!(format!("{}", FieldValue::Boolean(true)), "true");
        assert_eq!(
            format!(
                "{}",
                FieldValue::Array(vec![
                    FieldValue::Number(1.0),
                    FieldValue::Text("two".into()),
                ])
            ),
            "[1, \"two\"]"
        );
    }

    #[test]
    fn field_value_from_conversions() {
        let text: FieldValue = "hello".into();
        assert_eq!(text, FieldValue::Text("hello".into()));

        let num: FieldValue = 42.0_f64.into();
        assert_eq!(num, FieldValue::Number(42.0));

        let int: FieldValue = 10_i64.into();
        assert_eq!(int, FieldValue::Number(10.0));

        let boolean: FieldValue = true.into();
        assert_eq!(boolean, FieldValue::Boolean(true));
    }

    #[test]
    fn field_value_serde_roundtrip() {
        let values = vec![
            FieldValue::Text("search engine".into()),
            FieldValue::Number(99.9),
            FieldValue::Boolean(false),
            FieldValue::Array(vec![
                FieldValue::Text("nested".into()),
                FieldValue::Number(1.0),
            ]),
        ];

        for val in &values {
            let json = serde_json::to_string(val).expect("serialize");
            let back: FieldValue = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*val, back);
        }
    }

    #[test]
    fn document_builder_pattern() {
        let doc = Document::new(DocumentId::new("d1"))
            .with_field("title", FieldValue::Text("MSearchDB".into()))
            .with_field("stars", FieldValue::Number(5.0));

        assert_eq!(doc.fields.len(), 2);
        assert_eq!(
            doc.get_field("title"),
            Some(&FieldValue::Text("MSearchDB".into()))
        );
    }

    #[test]
    fn document_set_field_returns_old_value() {
        let mut doc = Document::new(DocumentId::new("d2")).with_field("x", FieldValue::Number(1.0));

        let old = doc.set_field("x", FieldValue::Number(2.0));
        assert_eq!(old, Some(FieldValue::Number(1.0)));
        assert_eq!(doc.get_field("x"), Some(&FieldValue::Number(2.0)));
    }

    #[test]
    fn document_display() {
        let doc = Document::new(DocumentId::new("abc-123"));
        assert_eq!(format!("{}", doc), "Document(abc-123)");
    }

    #[test]
    fn document_serde_roundtrip() {
        let doc = Document::new(DocumentId::new("serde-test"))
            .with_field("name", FieldValue::Text("test".into()))
            .with_field("active", FieldValue::Boolean(true))
            .with_field("score", FieldValue::Number(0.95));

        let json = serde_json::to_string(&doc).expect("serialize");
        let back: Document = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(doc.id, back.id);
        assert_eq!(doc.fields.len(), back.fields.len());
        assert_eq!(doc.get_field("name"), back.get_field("name"));
    }
}
