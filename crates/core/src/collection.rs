//! Collection model for MSearchDB.
//!
//! A [`Collection`] is an isolated namespace within MSearchDB that groups
//! related documents together with their own schema, settings, and
//! per-collection storage/index isolation (separate RocksDB column families
//! and Tantivy indices).
//!
//! # Field Mapping & Schema Evolution
//!
//! Collections track a [`FieldMapping`] that records the type of every field
//! seen so far.  When a document is indexed, each field is auto-detected on
//! first encounter and registered in the mapping.  Once a field's type is
//! established it **cannot** be changed — attempting to index a value of a
//! different type returns [`DbError::SchemaConflict`].  Adding entirely new
//! fields always succeeds (append-only schema evolution).
//!
//! # Collection Aliases
//!
//! A [`CollectionAlias`] maps a single name to one or more backing
//! collections.  Queries against an alias fan out across all targets and
//! merge results.  This enables zero-downtime reindexing: build the new
//! collection, swap the alias, then drop the old collection.
//!
//! # Examples
//!
//! ```
//! use msearchdb_core::collection::{
//!     Collection, CollectionSettings, FieldMapping, MappedFieldType,
//! };
//!
//! let mut mapping = FieldMapping::new();
//! mapping.register_field("title", MappedFieldType::Text).unwrap();
//! mapping.register_field("price", MappedFieldType::Number).unwrap();
//!
//! let collection = Collection::new("products")
//!     .with_settings(CollectionSettings::default())
//!     .with_mapping(mapping);
//!
//! assert_eq!(collection.name(), "products");
//! assert_eq!(collection.mapping().fields().len(), 2);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

use crate::error::DbError;

// ---------------------------------------------------------------------------
// MappedFieldType — the type recorded for a field in the mapping
// ---------------------------------------------------------------------------

/// The detected/declared type of a field inside a collection's mapping.
///
/// This mirrors [`FieldValue`](crate::document::FieldValue) but is a
/// type-level concept (what a field *is*), not a value-level concept (what
/// a field *holds*).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MappedFieldType {
    /// Full-text indexed UTF-8 text.
    Text,
    /// IEEE 754 f64 numeric.
    Number,
    /// Boolean flag (stored as u64 0/1).
    Boolean,
    /// Array of values — the element type is tracked separately.
    Array,
}

impl fmt::Display for MappedFieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MappedFieldType::Text => write!(f, "text"),
            MappedFieldType::Number => write!(f, "number"),
            MappedFieldType::Boolean => write!(f, "boolean"),
            MappedFieldType::Array => write!(f, "array"),
        }
    }
}

// ---------------------------------------------------------------------------
// FieldMapping — tracks field names → types for a collection
// ---------------------------------------------------------------------------

/// Dynamic field mapping that records the type of every field seen in a
/// collection.
///
/// Once a field type is registered it is immutable.  Attempting to register
/// the same field with a different type returns
/// [`DbError::SchemaConflict`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldMapping {
    /// Known field names and their types.
    fields: HashMap<String, MappedFieldType>,
}

impl FieldMapping {
    /// Create an empty field mapping.
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
        }
    }

    /// Register a field with the given type.
    ///
    /// - If the field does not yet exist it is added and `Ok(true)` is
    ///   returned (new field).
    /// - If the field already exists with the **same** type, `Ok(false)` is
    ///   returned (no change).
    /// - If the field already exists with a **different** type,
    ///   `Err(DbError::SchemaConflict)` is returned.
    pub fn register_field(
        &mut self,
        name: impl Into<String>,
        field_type: MappedFieldType,
    ) -> Result<bool, DbError> {
        let name = name.into();
        if let Some(existing) = self.fields.get(&name) {
            if *existing == field_type {
                Ok(false) // already known, same type
            } else {
                Err(DbError::SchemaConflict {
                    field: name,
                    existing: existing.to_string(),
                    incoming: field_type.to_string(),
                })
            }
        } else {
            self.fields.insert(name, field_type);
            Ok(true) // newly registered
        }
    }

    /// Look up the type of a field, if known.
    pub fn get_field_type(&self, name: &str) -> Option<&MappedFieldType> {
        self.fields.get(name)
    }

    /// Return a reference to the full field map.
    pub fn fields(&self) -> &HashMap<String, MappedFieldType> {
        &self.fields
    }

    /// Return the number of mapped fields.
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// Merge another mapping into this one (schema evolution).
    ///
    /// All fields from `other` are registered into `self`.  Returns the
    /// list of newly added field names, or an error on the first type
    /// conflict.
    pub fn merge(&mut self, other: &FieldMapping) -> Result<Vec<String>, DbError> {
        let mut added = Vec::new();
        for (name, ft) in &other.fields {
            if self.register_field(name.clone(), ft.clone())? {
                added.push(name.clone());
            }
        }
        Ok(added)
    }
}

impl Default for FieldMapping {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CollectionSettings — per-collection configuration
// ---------------------------------------------------------------------------

/// Per-collection configuration knobs.
///
/// These settings are specified at collection creation time and are
/// immutable once created (mirroring Elasticsearch index settings).
///
/// All fields default to sensible values so that partial JSON payloads
/// (e.g. `{"number_of_shards": 3}`) work out of the box.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectionSettings {
    /// Number of primary shards (logical partitions).
    ///
    /// In single-node mode every shard lives on the same node.
    pub number_of_shards: u32,

    /// Replication factor — how many copies of each shard to maintain.
    pub replication_factor: u32,

    /// Name of the default text analyzer for text fields.
    ///
    /// Must match a registered analyzer (e.g. `"standard"`, `"keyword"`,
    /// `"ngram"`, `"cjk"`).
    pub default_analyzer: String,

    /// Optional per-field analyzer overrides.
    ///
    /// Keys are field names, values are analyzer names.
    pub field_analyzers: HashMap<String, String>,

    /// Whether to maintain a `_body` catch-all text field that
    /// concatenates all text content for simple query-string search.
    pub enable_body_field: bool,
}

impl Default for CollectionSettings {
    fn default() -> Self {
        Self {
            number_of_shards: 1,
            replication_factor: 1,
            default_analyzer: "standard".to_owned(),
            field_analyzers: HashMap::new(),
            enable_body_field: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Collection — the top-level collection descriptor
// ---------------------------------------------------------------------------

/// A named, isolated collection within MSearchDB.
///
/// Each collection owns:
/// - A unique name.
/// - A [`FieldMapping`] tracking all known field types.
/// - [`CollectionSettings`] controlling sharding, replication, and analysis.
/// - A document count (approximate).
/// - A creation timestamp.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    /// Unique name of this collection.
    name: String,

    /// Dynamic field mapping tracking discovered field types.
    mapping: FieldMapping,

    /// Collection-level settings.
    settings: CollectionSettings,

    /// Approximate document count.
    doc_count: u64,

    /// Unix timestamp (seconds) of collection creation.
    created_at: u64,
}

impl Collection {
    /// Create a new empty collection.
    pub fn new(name: impl Into<String>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            name: name.into(),
            mapping: FieldMapping::new(),
            settings: CollectionSettings::default(),
            doc_count: 0,
            created_at: now,
        }
    }

    // -- Builder methods -----------------------------------------------------

    /// Set the field mapping (builder pattern).
    pub fn with_mapping(mut self, mapping: FieldMapping) -> Self {
        self.mapping = mapping;
        self
    }

    /// Set the collection settings (builder pattern).
    pub fn with_settings(mut self, settings: CollectionSettings) -> Self {
        self.settings = settings;
        self
    }

    // -- Accessors -----------------------------------------------------------

    /// The collection name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The current field mapping.
    pub fn mapping(&self) -> &FieldMapping {
        &self.mapping
    }

    /// Mutable access to the field mapping (for registering new fields).
    pub fn mapping_mut(&mut self) -> &mut FieldMapping {
        &mut self.mapping
    }

    /// The collection settings.
    pub fn settings(&self) -> &CollectionSettings {
        &self.settings
    }

    /// Approximate document count.
    pub fn doc_count(&self) -> u64 {
        self.doc_count
    }

    /// Increment the document count by `n`.
    pub fn increment_doc_count(&mut self, n: u64) {
        self.doc_count += n;
    }

    /// Decrement the document count by `n` (saturating).
    pub fn decrement_doc_count(&mut self, n: u64) {
        self.doc_count = self.doc_count.saturating_sub(n);
    }

    /// Set the document count explicitly.
    pub fn set_doc_count(&mut self, count: u64) {
        self.doc_count = count;
    }

    /// Unix timestamp when the collection was created.
    pub fn created_at(&self) -> u64 {
        self.created_at
    }
}

impl fmt::Display for Collection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Collection({})", self.name)
    }
}

// ---------------------------------------------------------------------------
// CollectionAlias — maps a name to one or more collections
// ---------------------------------------------------------------------------

/// An alias that maps a single name to one or more backing collections.
///
/// Aliases enable zero-downtime reindexing:
///
/// 1. Create a new collection with updated settings/schema.
/// 2. Reindex documents into the new collection.
/// 3. Atomically swap the alias to point to the new collection.
/// 4. Drop the old collection.
///
/// Queries against an alias fan out across all target collections and
/// merge results.  Writes require exactly one target (the *write index*).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionAlias {
    /// The alias name (used in API paths instead of a collection name).
    name: String,

    /// The collections this alias points to.
    ///
    /// For search queries all targets are queried.  For writes, the first
    /// entry is the designated write target.
    collections: Vec<String>,
}

impl CollectionAlias {
    /// Create a new alias pointing to a single collection.
    pub fn new(name: impl Into<String>, collection: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            collections: vec![collection.into()],
        }
    }

    /// Create a new alias pointing to multiple collections.
    pub fn new_multi(name: impl Into<String>, collections: Vec<String>) -> Self {
        Self {
            name: name.into(),
            collections,
        }
    }

    /// The alias name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The list of backing collections.
    pub fn collections(&self) -> &[String] {
        &self.collections
    }

    /// The write-target collection (first in the list).
    ///
    /// Returns `None` if the alias has no backing collections.
    pub fn write_target(&self) -> Option<&str> {
        self.collections.first().map(|s| s.as_str())
    }

    /// Add a collection to the alias.
    pub fn add_collection(&mut self, collection: impl Into<String>) {
        let name = collection.into();
        if !self.collections.contains(&name) {
            self.collections.push(name);
        }
    }

    /// Remove a collection from the alias.
    pub fn remove_collection(&mut self, collection: &str) {
        self.collections.retain(|c| c != collection);
    }

    /// Replace all backing collections atomically.
    pub fn set_collections(&mut self, collections: Vec<String>) {
        self.collections = collections;
    }

    /// Return the number of backing collections.
    pub fn collection_count(&self) -> usize {
        self.collections.len()
    }
}

impl fmt::Display for CollectionAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Alias({} -> [{}])",
            self.name,
            self.collections.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- MappedFieldType tests -----------------------------------------------

    #[test]
    fn mapped_field_type_display() {
        assert_eq!(MappedFieldType::Text.to_string(), "text");
        assert_eq!(MappedFieldType::Number.to_string(), "number");
        assert_eq!(MappedFieldType::Boolean.to_string(), "boolean");
        assert_eq!(MappedFieldType::Array.to_string(), "array");
    }

    #[test]
    fn mapped_field_type_serde_roundtrip() {
        for ft in &[
            MappedFieldType::Text,
            MappedFieldType::Number,
            MappedFieldType::Boolean,
            MappedFieldType::Array,
        ] {
            let json = serde_json::to_string(ft).unwrap();
            let back: MappedFieldType = serde_json::from_str(&json).unwrap();
            assert_eq!(*ft, back);
        }
    }

    // -- FieldMapping tests --------------------------------------------------

    #[test]
    fn field_mapping_register_new_field() {
        let mut m = FieldMapping::new();
        let added = m.register_field("title", MappedFieldType::Text).unwrap();
        assert!(added);
        assert_eq!(m.field_count(), 1);
        assert_eq!(m.get_field_type("title"), Some(&MappedFieldType::Text));
    }

    #[test]
    fn field_mapping_register_same_type_is_noop() {
        let mut m = FieldMapping::new();
        m.register_field("title", MappedFieldType::Text).unwrap();
        let added = m.register_field("title", MappedFieldType::Text).unwrap();
        assert!(!added); // no change
        assert_eq!(m.field_count(), 1);
    }

    #[test]
    fn field_mapping_register_conflicting_type_errors() {
        let mut m = FieldMapping::new();
        m.register_field("price", MappedFieldType::Number).unwrap();
        let err = m
            .register_field("price", MappedFieldType::Text)
            .unwrap_err();
        assert!(matches!(err, DbError::SchemaConflict { .. }));
        assert!(err.to_string().contains("price"));
    }

    #[test]
    fn field_mapping_merge_adds_new_fields() {
        let mut m1 = FieldMapping::new();
        m1.register_field("title", MappedFieldType::Text).unwrap();

        let mut m2 = FieldMapping::new();
        m2.register_field("price", MappedFieldType::Number).unwrap();
        m2.register_field("title", MappedFieldType::Text).unwrap();

        let added = m1.merge(&m2).unwrap();
        assert_eq!(added, vec!["price".to_string()]);
        assert_eq!(m1.field_count(), 2);
    }

    #[test]
    fn field_mapping_merge_detects_conflict() {
        let mut m1 = FieldMapping::new();
        m1.register_field("x", MappedFieldType::Text).unwrap();

        let mut m2 = FieldMapping::new();
        m2.register_field("x", MappedFieldType::Number).unwrap();

        let err = m1.merge(&m2).unwrap_err();
        assert!(matches!(err, DbError::SchemaConflict { .. }));
    }

    #[test]
    fn field_mapping_serde_roundtrip() {
        let mut m = FieldMapping::new();
        m.register_field("title", MappedFieldType::Text).unwrap();
        m.register_field("price", MappedFieldType::Number).unwrap();

        let json = serde_json::to_string(&m).unwrap();
        let back: FieldMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    // -- CollectionSettings tests --------------------------------------------

    #[test]
    fn collection_settings_defaults() {
        let s = CollectionSettings::default();
        assert_eq!(s.number_of_shards, 1);
        assert_eq!(s.replication_factor, 1);
        assert_eq!(s.default_analyzer, "standard");
        assert!(s.field_analyzers.is_empty());
        assert!(s.enable_body_field);
    }

    #[test]
    fn collection_settings_serde_roundtrip() {
        let mut s = CollectionSettings::default();
        s.number_of_shards = 3;
        s.replication_factor = 2;
        s.field_analyzers.insert("body".into(), "cjk".into());

        let json = serde_json::to_string(&s).unwrap();
        let back: CollectionSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // -- Collection tests ----------------------------------------------------

    #[test]
    fn collection_new_defaults() {
        let c = Collection::new("products");
        assert_eq!(c.name(), "products");
        assert_eq!(c.doc_count(), 0);
        assert_eq!(c.mapping().field_count(), 0);
        assert_eq!(c.settings().number_of_shards, 1);
        assert!(c.created_at() > 0);
    }

    #[test]
    fn collection_builder_pattern() {
        let mut mapping = FieldMapping::new();
        mapping
            .register_field("title", MappedFieldType::Text)
            .unwrap();

        let settings = CollectionSettings {
            number_of_shards: 3,
            ..Default::default()
        };

        let c = Collection::new("articles")
            .with_mapping(mapping.clone())
            .with_settings(settings.clone());

        assert_eq!(c.name(), "articles");
        assert_eq!(c.mapping(), &mapping);
        assert_eq!(c.settings(), &settings);
    }

    #[test]
    fn collection_doc_count_operations() {
        let mut c = Collection::new("test");
        c.increment_doc_count(5);
        assert_eq!(c.doc_count(), 5);
        c.decrement_doc_count(2);
        assert_eq!(c.doc_count(), 3);
        c.decrement_doc_count(100); // saturating
        assert_eq!(c.doc_count(), 0);
        c.set_doc_count(42);
        assert_eq!(c.doc_count(), 42);
    }

    #[test]
    fn collection_display() {
        let c = Collection::new("my_collection");
        assert_eq!(format!("{}", c), "Collection(my_collection)");
    }

    #[test]
    fn collection_serde_roundtrip() {
        let mut mapping = FieldMapping::new();
        mapping
            .register_field("title", MappedFieldType::Text)
            .unwrap();
        mapping
            .register_field("price", MappedFieldType::Number)
            .unwrap();

        let c = Collection::new("products").with_mapping(mapping);

        let json = serde_json::to_string(&c).unwrap();
        let back: Collection = serde_json::from_str(&json).unwrap();
        assert_eq!(c.name(), back.name());
        assert_eq!(c.mapping(), back.mapping());
    }

    // -- CollectionAlias tests -----------------------------------------------

    #[test]
    fn alias_single_collection() {
        let alias = CollectionAlias::new("current_products", "products_v2");
        assert_eq!(alias.name(), "current_products");
        assert_eq!(alias.collections(), &["products_v2"]);
        assert_eq!(alias.write_target(), Some("products_v2"));
        assert_eq!(alias.collection_count(), 1);
    }

    #[test]
    fn alias_multi_collection() {
        let alias =
            CollectionAlias::new_multi("all_logs", vec!["logs_2024".into(), "logs_2025".into()]);
        assert_eq!(alias.collection_count(), 2);
        assert_eq!(alias.write_target(), Some("logs_2024"));
    }

    #[test]
    fn alias_add_remove_collections() {
        let mut alias = CollectionAlias::new("my_alias", "col_a");
        alias.add_collection("col_b");
        assert_eq!(alias.collection_count(), 2);

        // Adding duplicate is a no-op
        alias.add_collection("col_a");
        assert_eq!(alias.collection_count(), 2);

        alias.remove_collection("col_a");
        assert_eq!(alias.collection_count(), 1);
        assert_eq!(alias.collections(), &["col_b"]);
    }

    #[test]
    fn alias_set_collections_atomic() {
        let mut alias = CollectionAlias::new("idx", "old");
        alias.set_collections(vec!["new_v1".into(), "new_v2".into()]);
        assert_eq!(alias.collection_count(), 2);
        assert_eq!(alias.write_target(), Some("new_v1"));
    }

    #[test]
    fn alias_display() {
        let alias = CollectionAlias::new_multi("a", vec!["c1".into(), "c2".into()]);
        assert_eq!(format!("{}", alias), "Alias(a -> [c1, c2])");
    }

    #[test]
    fn alias_serde_roundtrip() {
        let alias =
            CollectionAlias::new_multi("prod", vec!["products_v1".into(), "products_v2".into()]);
        let json = serde_json::to_string(&alias).unwrap();
        let back: CollectionAlias = serde_json::from_str(&json).unwrap();
        assert_eq!(alias, back);
    }
}
