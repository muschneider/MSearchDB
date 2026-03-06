//! Dynamic schema builder for Tantivy.
//!
//! Maps MSearchDB [`FieldValue`] types to Tantivy schema fields automatically.
//! The builder inspects document fields and constructs a Tantivy [`Schema`] with
//! appropriate indexing options for each type.
//!
//! Every schema includes three reserved meta-fields:
//! - `_id` — the document identifier (TEXT, stored + indexed)
//! - `_score` — relevance score placeholder (F64)
//! - `_timestamp` — ingestion timestamp (U64, fast)
//!
//! # Examples
//!
//! ```
//! use msearchdb_index::schema_builder::{DynamicSchemaBuilder, SchemaConfig, FieldConfig, FieldType};
//!
//! let config = SchemaConfig::new()
//!     .with_field(FieldConfig::new("title", FieldType::Text))
//!     .with_field(FieldConfig::new("price", FieldType::Number))
//!     .with_field(FieldConfig::new("active", FieldType::Boolean));
//!
//! let (schema, field_map) = DynamicSchemaBuilder::build(&config);
//! assert!(field_map.contains_key("_id"));
//! assert!(field_map.contains_key("title"));
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tantivy::schema::{
    Field, NumericOptions, Schema, SchemaBuilder, TextFieldIndexing, TextOptions,
};

// ---------------------------------------------------------------------------
// FieldType — mapping from MSearchDB types to Tantivy types
// ---------------------------------------------------------------------------

/// The type of a field in the search schema, mapping to Tantivy field types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FieldType {
    /// UTF-8 text, tokenized and indexed for full-text search.
    Text,
    /// IEEE 754 double-precision number, stored as Tantivy F64 with fast field.
    Number,
    /// Boolean value, stored as Tantivy U64 (0 or 1) with fast field.
    Boolean,
}

// ---------------------------------------------------------------------------
// FieldConfig — per-field configuration
// ---------------------------------------------------------------------------

/// Configuration for a single field in the search schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldConfig {
    /// The name of the field as it appears in documents.
    pub name: String,
    /// The Tantivy-compatible type of this field.
    pub field_type: FieldType,
    /// Whether this field is stored (retrievable in search results).
    pub stored: bool,
    /// Whether this field is indexed (searchable).
    pub indexed: bool,
}

impl FieldConfig {
    /// Create a new field configuration with sensible defaults.
    ///
    /// Text fields default to stored=true, indexed=true.
    /// Number and Boolean fields default to stored=true, indexed=true.
    pub fn new(name: impl Into<String>, field_type: FieldType) -> Self {
        Self {
            name: name.into(),
            field_type,
            stored: true,
            indexed: true,
        }
    }

    /// Builder method: set whether the field is stored.
    pub fn with_stored(mut self, stored: bool) -> Self {
        self.stored = stored;
        self
    }

    /// Builder method: set whether the field is indexed.
    pub fn with_indexed(mut self, indexed: bool) -> Self {
        self.indexed = indexed;
        self
    }
}

// ---------------------------------------------------------------------------
// SchemaConfig — collection of field configs
// ---------------------------------------------------------------------------

/// Schema configuration describing all user-defined fields.
///
/// Meta-fields (`_id`, `_score`, `_timestamp`) are added automatically by
/// the [`DynamicSchemaBuilder`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaConfig {
    /// User-defined field configurations.
    pub fields: Vec<FieldConfig>,
}

impl SchemaConfig {
    /// Create an empty schema configuration.
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// Builder method: add a field configuration.
    pub fn with_field(mut self, field: FieldConfig) -> Self {
        self.fields.push(field);
        self
    }
}

impl Default for SchemaConfig {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// FieldMap — maps field names to Tantivy Field handles
// ---------------------------------------------------------------------------

/// A mapping from field names to Tantivy [`Field`] handles.
///
/// This is shared across threads via `Arc` and is immutable once created.
pub type FieldMap = HashMap<String, Field>;

// ---------------------------------------------------------------------------
// DynamicSchemaBuilder
// ---------------------------------------------------------------------------

/// Builds Tantivy schemas dynamically from [`SchemaConfig`].
///
/// The builder always adds reserved meta-fields (`_id`, `_score`, `_timestamp`)
/// and then maps each user-defined field to the appropriate Tantivy type:
///
/// - [`FieldType::Text`] → Tantivy TEXT (tokenized, stored, indexed)
/// - [`FieldType::Number`] → Tantivy F64 (stored, fast field)
/// - [`FieldType::Boolean`] → Tantivy U64 (stored, fast field)
///
/// The resulting [`Schema`] is immutable and should be wrapped in [`Arc`] for
/// sharing across threads.
pub struct DynamicSchemaBuilder;

impl DynamicSchemaBuilder {
    /// Build a Tantivy [`Schema`] and [`FieldMap`] from the given configuration.
    ///
    /// Returns `(Arc<Schema>, FieldMap)` where the field map contains handles
    /// for both meta-fields and user-defined fields.
    pub fn build(config: &SchemaConfig) -> (Arc<Schema>, FieldMap) {
        let mut builder = SchemaBuilder::default();
        let mut field_map = HashMap::new();

        // -- Reserved meta-fields --

        // _id: TEXT, stored and indexed (for exact lookups and retrieval)
        let id_options = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
        );
        let id_field = builder.add_text_field("_id", id_options);
        field_map.insert("_id".to_owned(), id_field);

        // _score: F64 (not indexed, used as a placeholder for relevance scores)
        let score_options = NumericOptions::default().set_stored().set_fast();
        let score_field = builder.add_f64_field("_score", score_options);
        field_map.insert("_score".to_owned(), score_field);

        // _timestamp: U64, fast (for sorting by ingestion time)
        let ts_options = NumericOptions::default()
            .set_stored()
            .set_fast()
            .set_indexed();
        let ts_field = builder.add_u64_field("_timestamp", ts_options);
        field_map.insert("_timestamp".to_owned(), ts_field);

        // -- User-defined fields --

        for fc in &config.fields {
            let field = match fc.field_type {
                FieldType::Text => {
                    let mut opts = TextOptions::default();
                    if fc.stored {
                        opts = opts.set_stored();
                    }
                    if fc.indexed {
                        opts = opts.set_indexing_options(
                            TextFieldIndexing::default()
                                .set_tokenizer("default")
                                .set_index_option(
                                    tantivy::schema::IndexRecordOption::WithFreqsAndPositions,
                                ),
                        );
                    }
                    builder.add_text_field(&fc.name, opts)
                }
                FieldType::Number => {
                    let mut opts = NumericOptions::default().set_fast();
                    if fc.stored {
                        opts = opts.set_stored();
                    }
                    if fc.indexed {
                        opts = opts.set_indexed();
                    }
                    builder.add_f64_field(&fc.name, opts)
                }
                FieldType::Boolean => {
                    let mut opts = NumericOptions::default().set_fast();
                    if fc.stored {
                        opts = opts.set_stored();
                    }
                    if fc.indexed {
                        opts = opts.set_indexed();
                    }
                    builder.add_u64_field(&fc.name, opts)
                }
            };
            field_map.insert(fc.name.clone(), field);
        }

        let schema = builder.build();
        (Arc::new(schema), field_map)
    }

    /// Build a default schema suitable for schemaless documents.
    ///
    /// Creates a "catch-all" text field named `_body` that all text content
    /// is indexed into, plus the standard meta-fields.
    pub fn build_default() -> (Arc<Schema>, FieldMap) {
        let config = SchemaConfig::new().with_field(FieldConfig::new("_body", FieldType::Text));
        Self::build(&config)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_config_builder_pattern() {
        let config = SchemaConfig::new()
            .with_field(FieldConfig::new("title", FieldType::Text))
            .with_field(FieldConfig::new("price", FieldType::Number));

        assert_eq!(config.fields.len(), 2);
        assert_eq!(config.fields[0].name, "title");
        assert_eq!(config.fields[1].field_type, FieldType::Number);
    }

    #[test]
    fn field_config_defaults() {
        let fc = FieldConfig::new("name", FieldType::Text);
        assert!(fc.stored);
        assert!(fc.indexed);
    }

    #[test]
    fn field_config_builder_overrides() {
        let fc = FieldConfig::new("name", FieldType::Text)
            .with_stored(false)
            .with_indexed(false);
        assert!(!fc.stored);
        assert!(!fc.indexed);
    }

    #[test]
    fn build_schema_has_meta_fields() {
        let config = SchemaConfig::new();
        let (schema, field_map) = DynamicSchemaBuilder::build(&config);

        // Meta-fields must be present
        assert!(field_map.contains_key("_id"));
        assert!(field_map.contains_key("_score"));
        assert!(field_map.contains_key("_timestamp"));

        // Schema should have exactly 3 fields (meta only)
        let _id_field = schema.get_field("_id").unwrap();
        let _score_field = schema.get_field("_score").unwrap();
        let _ts_field = schema.get_field("_timestamp").unwrap();
        assert_eq!(_id_field, field_map["_id"]);
        assert_eq!(_score_field, field_map["_score"]);
        assert_eq!(_ts_field, field_map["_timestamp"]);
    }

    #[test]
    fn build_schema_with_text_field() {
        let config = SchemaConfig::new().with_field(FieldConfig::new("title", FieldType::Text));

        let (schema, field_map) = DynamicSchemaBuilder::build(&config);

        assert!(field_map.contains_key("title"));
        let title_field = schema.get_field("title").unwrap();
        assert_eq!(title_field, field_map["title"]);
    }

    #[test]
    fn build_schema_with_number_field() {
        let config = SchemaConfig::new().with_field(FieldConfig::new("price", FieldType::Number));

        let (_schema, field_map) = DynamicSchemaBuilder::build(&config);
        assert!(field_map.contains_key("price"));
    }

    #[test]
    fn build_schema_with_boolean_field() {
        let config = SchemaConfig::new().with_field(FieldConfig::new("active", FieldType::Boolean));

        let (_schema, field_map) = DynamicSchemaBuilder::build(&config);
        assert!(field_map.contains_key("active"));
    }

    #[test]
    fn build_default_has_body_field() {
        let (_schema, field_map) = DynamicSchemaBuilder::build_default();

        assert!(field_map.contains_key("_body"));
        assert!(field_map.contains_key("_id"));
    }

    #[test]
    fn schema_is_immutable_via_arc() {
        let config = SchemaConfig::new().with_field(FieldConfig::new("x", FieldType::Text));
        let (schema, _) = DynamicSchemaBuilder::build(&config);

        // Clone the Arc — both point to the same schema
        let schema2 = Arc::clone(&schema);
        assert_eq!(
            schema.get_field("x").unwrap(),
            schema2.get_field("x").unwrap()
        );
    }

    #[test]
    fn schema_config_serde_roundtrip() {
        let config = SchemaConfig::new()
            .with_field(FieldConfig::new("title", FieldType::Text))
            .with_field(FieldConfig::new("count", FieldType::Number))
            .with_field(FieldConfig::new("flag", FieldType::Boolean));

        let json = serde_json::to_string(&config).unwrap();
        let back: SchemaConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }
}
