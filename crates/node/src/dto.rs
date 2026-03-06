//! Request and response data transfer objects for the MSearchDB REST API.
//!
//! These DTOs define the JSON schema that external clients (Python SDK, curl,
//! etc.) send and receive.  Each DTO derives [`serde::Serialize`] and/or
//! [`serde::Deserialize`] so that [`axum::Json`] can convert them automatically.
//!
//! # Design
//!
//! - **[`QueryDsl`]** provides a flexible JSON query language that mirrors
//!   Elasticsearch-style DSL and maps to the core [`Query`] type.
//! - **[`IndexDocumentRequest`]** / **[`IndexDocumentResponse`]** wrap
//!   single-document CRUD operations.
//! - **[`BulkAction`]** / **[`BulkResponse`]** handle NDJSON-based bulk
//!   indexing where action+document lines alternate.
//! - **[`ErrorResponse`]** provides a uniform error envelope for all
//!   failure modes.
//!
//! [`Query`]: msearchdb_core::query::Query

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use msearchdb_core::document::{Document, DocumentId, FieldValue};
use msearchdb_core::query::{
    BoolQuery, FullTextQuery, Operator, Query, RangeQuery, ScoredDocument, SearchResult, TermQuery,
};

// ---------------------------------------------------------------------------
// Document DTOs
// ---------------------------------------------------------------------------

/// Request body for indexing a single document.
///
/// If `id` is `None`, the server generates a UUID v4 identifier.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexDocumentRequest {
    /// Optional document id.  If absent, the server generates one.
    #[serde(default)]
    pub id: Option<String>,

    /// Document fields as a flat JSON object.
    pub fields: HashMap<String, Value>,
}

/// Response body after successfully indexing a document.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexDocumentResponse {
    /// The id of the indexed document.
    #[serde(rename = "_id")]
    pub id: String,

    /// The outcome: `"created"` or `"updated"`.
    pub result: String,

    /// Monotonically increasing version counter.
    pub version: u64,
}

// ---------------------------------------------------------------------------
// QueryDsl — flexible JSON query language
// ---------------------------------------------------------------------------

/// A flexible JSON query DSL that maps to the core [`Query`] type.
///
/// Supports four query shapes:
///
/// - **`match`**: full-text search against a field.
/// - **`term`**: exact value match.
/// - **`range`**: numeric range filter.
/// - **`bool`**: boolean combination of sub-queries.
///
/// # JSON Examples
///
/// ```json
/// { "match": { "title": { "query": "rust programming", "operator": "and" } } }
/// { "term":  { "status": "published" } }
/// { "range": { "price": { "gte": 10.0, "lte": 100.0 } } }
/// { "bool":  { "must": [...], "should": [...], "must_not": [...] } }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryDsl {
    /// Full-text match query.
    Match(HashMap<String, MatchFieldQuery>),

    /// Exact term query.
    Term(HashMap<String, String>),

    /// Numeric range query.
    Range(HashMap<String, RangeFieldQuery>),

    /// Boolean combination of sub-queries.
    Bool(BoolQueryDsl),
}

/// Inner match-field options.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchFieldQuery {
    /// The search text.
    pub query: String,

    /// Boolean operator between tokens (`"and"` or `"or"`).
    #[serde(default = "default_operator_str")]
    pub operator: String,
}

fn default_operator_str() -> String {
    "or".to_string()
}

/// Inner range-field bounds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RangeFieldQuery {
    /// Inclusive lower bound.
    #[serde(default)]
    pub gte: Option<f64>,

    /// Inclusive upper bound.
    #[serde(default)]
    pub lte: Option<f64>,
}

/// Boolean query DSL with must / should / must_not arrays.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BoolQueryDsl {
    /// All of these queries must match.
    #[serde(default)]
    pub must: Vec<QueryDsl>,

    /// At least one of these queries should match.
    #[serde(default)]
    pub should: Vec<QueryDsl>,

    /// None of these queries may match.
    #[serde(default)]
    pub must_not: Vec<QueryDsl>,
}

// ---------------------------------------------------------------------------
// QueryDsl → core::Query conversion
// ---------------------------------------------------------------------------

impl QueryDsl {
    /// Convert this DSL representation to the core [`Query`] type.
    pub fn into_core_query(self) -> Query {
        match self {
            QueryDsl::Match(map) => {
                let (field, opts) = map.into_iter().next().unwrap_or_else(|| {
                    (
                        "_body".to_string(),
                        MatchFieldQuery {
                            query: String::new(),
                            operator: "or".to_string(),
                        },
                    )
                });
                let operator = match opts.operator.to_lowercase().as_str() {
                    "and" => Operator::And,
                    _ => Operator::Or,
                };
                Query::FullText(FullTextQuery {
                    field,
                    query: opts.query,
                    operator,
                })
            }
            QueryDsl::Term(map) => {
                let (field, value) = map.into_iter().next().unwrap_or_default();
                Query::Term(TermQuery { field, value })
            }
            QueryDsl::Range(map) => {
                let (field, bounds) = map.into_iter().next().unwrap_or_else(|| {
                    (
                        "".to_string(),
                        RangeFieldQuery {
                            gte: None,
                            lte: None,
                        },
                    )
                });
                Query::Range(RangeQuery {
                    field,
                    gte: bounds.gte,
                    lte: bounds.lte,
                })
            }
            QueryDsl::Bool(b) => {
                let must = b.must.into_iter().map(|q| q.into_core_query()).collect();
                let should = b.should.into_iter().map(|q| q.into_core_query()).collect();
                let must_not = b
                    .must_not
                    .into_iter()
                    .map(|q| q.into_core_query())
                    .collect();
                Query::Bool(BoolQuery {
                    must,
                    should,
                    must_not,
                })
            }
        }
    }
}

impl From<QueryDsl> for Query {
    fn from(dsl: QueryDsl) -> Self {
        dsl.into_core_query()
    }
}

// ---------------------------------------------------------------------------
// Search DTOs
// ---------------------------------------------------------------------------

/// Sort direction for search results.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SortField {
    /// Field name to sort by.
    pub field: String,

    /// Sort order: `"asc"` or `"desc"`.
    #[serde(default = "default_sort_order")]
    pub order: String,
}

fn default_sort_order() -> String {
    "desc".to_string()
}

/// Highlight configuration options.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HighlightOptions {
    /// Fields to highlight.
    #[serde(default)]
    pub fields: Vec<String>,

    /// HTML tag before a highlighted term.
    #[serde(default = "default_pre_tag")]
    pub pre_tag: String,

    /// HTML tag after a highlighted term.
    #[serde(default = "default_post_tag")]
    pub post_tag: String,
}

fn default_pre_tag() -> String {
    "<em>".to_string()
}

fn default_post_tag() -> String {
    "</em>".to_string()
}

/// Request body for the `POST /_search` endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    /// The query DSL.
    pub query: QueryDsl,

    /// Maximum number of hits to return.
    #[serde(default = "default_size")]
    pub size: Option<usize>,

    /// Offset into the result set (for pagination).
    #[serde(default)]
    pub from: Option<usize>,

    /// Optional highlighting configuration.
    #[serde(default)]
    pub highlight: Option<HighlightOptions>,

    /// Optional sort specification.
    #[serde(default)]
    pub sort: Option<Vec<SortField>>,
}

fn default_size() -> Option<usize> {
    Some(10)
}

/// A single search hit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hit {
    /// Document id.
    #[serde(rename = "_id")]
    pub id: String,

    /// Relevance score.
    #[serde(rename = "_score")]
    pub score: f32,

    /// Full document source.
    #[serde(rename = "_source")]
    pub source: Value,

    /// Optional highlight fragments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight: Option<Value>,
}

/// Total hit count metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Total {
    /// Total number of matching documents.
    pub value: u64,

    /// Whether the count is exact or a lower bound.
    pub relation: String,
}

/// Container for search hits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hits {
    /// Total hit metadata.
    pub total: Total,

    /// The actual hits.
    pub hits: Vec<Hit>,
}

/// Response body from a search operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    /// The search hits.
    pub hits: Hits,

    /// Time taken in milliseconds.
    pub took: u64,

    /// Whether the search timed out.
    pub timed_out: bool,
}

impl SearchResponse {
    /// Build a [`SearchResponse`] from a core [`SearchResult`].
    pub fn from_search_result(result: SearchResult) -> Self {
        let hits: Vec<Hit> = result
            .documents
            .into_iter()
            .map(scored_doc_to_hit)
            .collect();

        Self {
            hits: Hits {
                total: Total {
                    value: result.total,
                    relation: "eq".to_string(),
                },
                hits,
            },
            took: result.took_ms,
            timed_out: false,
        }
    }
}

/// Convert a [`ScoredDocument`] to a [`Hit`].
fn scored_doc_to_hit(scored: ScoredDocument) -> Hit {
    let source = fields_to_value(&scored.document.fields);
    Hit {
        id: scored.document.id.as_str().to_owned(),
        score: scored.score,
        source,
        highlight: None,
    }
}

// ---------------------------------------------------------------------------
// Simple query-string search params
// ---------------------------------------------------------------------------

/// Query parameters for `GET /_search?q=text`.
#[derive(Clone, Debug, Deserialize)]
pub struct SimpleSearchParams {
    /// The search text.
    pub q: String,

    /// Maximum number of hits.
    #[serde(default = "default_simple_size")]
    pub size: usize,
}

fn default_simple_size() -> usize {
    10
}

// ---------------------------------------------------------------------------
// Bulk DTOs
// ---------------------------------------------------------------------------

/// A single action in a bulk NDJSON request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkAction {
    /// Index (insert/upsert) a document.
    Index(BulkActionMeta),

    /// Delete a document.
    Delete(BulkActionMeta),
}

/// Metadata for a bulk action (id and optional collection routing).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkActionMeta {
    /// Optional document id.
    #[serde(rename = "_id", default)]
    pub id: Option<String>,
}

/// A single item result in a bulk response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkItem {
    /// The action performed.
    pub action: String,

    /// The document id.
    #[serde(rename = "_id")]
    pub id: String,

    /// HTTP status code for this item.
    pub status: u16,

    /// Optional error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response body for a bulk indexing operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BulkResponse {
    /// Time taken in milliseconds.
    pub took: u64,

    /// Whether any items had errors.
    pub errors: bool,

    /// Per-item results.
    pub items: Vec<BulkItem>,
}

// ---------------------------------------------------------------------------
// Cluster DTOs
// ---------------------------------------------------------------------------

/// Response for `GET /_cluster/health`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClusterHealthResponse {
    /// Cluster health status: `"green"`, `"yellow"`, or `"red"`.
    pub status: String,

    /// Number of nodes in the cluster.
    pub number_of_nodes: usize,

    /// The id of the current leader node.
    pub leader_id: Option<u64>,

    /// Whether this node is the leader.
    pub is_leader: bool,
}

/// Request to join a node to the cluster.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JoinNodeRequest {
    /// Address of the node to join (e.g., `"host:port"`).
    pub address: String,
}

/// Response for `GET /_stats`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatsResponse {
    /// Node identifier.
    pub node_id: u64,

    /// Number of collections.
    pub collections: usize,

    /// Whether the node is the Raft leader.
    pub is_leader: bool,

    /// Current Raft term.
    pub current_term: u64,
}

// ---------------------------------------------------------------------------
// Collection DTOs
// ---------------------------------------------------------------------------

/// Request body for creating a collection.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateCollectionRequest {
    /// Optional schema configuration.
    #[serde(default)]
    pub schema: Option<Value>,
}

/// Response body for collection information.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectionInfoResponse {
    /// Collection name.
    pub name: String,

    /// Number of documents in the collection.
    pub doc_count: u64,
}

// ---------------------------------------------------------------------------
// Error DTOs
// ---------------------------------------------------------------------------

/// Detailed error information.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// The error type (e.g., `"not_found"`, `"invalid_input"`).
    #[serde(rename = "type")]
    pub error_type: String,

    /// Human-readable error reason.
    pub reason: String,
}

/// Uniform error response envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Error details.
    pub error: ErrorDetail,

    /// HTTP status code.
    pub status: u16,
}

impl ErrorResponse {
    /// Create a new error response.
    pub fn new(status: u16, error_type: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                error_type: error_type.into(),
                reason: reason.into(),
            },
            status,
        }
    }

    /// Create a 404 Not Found response.
    pub fn not_found(reason: impl Into<String>) -> Self {
        Self::new(404, "not_found", reason)
    }

    /// Create a 400 Bad Request response.
    pub fn bad_request(reason: impl Into<String>) -> Self {
        Self::new(400, "invalid_input", reason)
    }

    /// Create a 500 Internal Server Error response.
    pub fn internal(reason: impl Into<String>) -> Self {
        Self::new(500, "internal_error", reason)
    }

    /// Create a 503 Service Unavailable response.
    pub fn service_unavailable(reason: impl Into<String>) -> Self {
        Self::new(503, "service_unavailable", reason)
    }

    /// Create a 401 Unauthorized response.
    pub fn unauthorized(reason: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", reason)
    }

    /// Create a 307 Temporary Redirect response.
    pub fn redirect(reason: impl Into<String>) -> Self {
        Self::new(307, "redirect", reason)
    }
}

// ---------------------------------------------------------------------------
// Helpers — FieldValue ↔ serde_json::Value conversion
// ---------------------------------------------------------------------------

/// Convert a [`FieldValue`] to a [`serde_json::Value`].
pub fn field_value_to_json(fv: &FieldValue) -> Value {
    match fv {
        FieldValue::Text(s) => Value::String(s.clone()),
        FieldValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        FieldValue::Boolean(b) => Value::Bool(*b),
        FieldValue::Array(arr) => Value::Array(arr.iter().map(field_value_to_json).collect()),
        _ => Value::Null,
    }
}

/// Convert a [`serde_json::Value`] to a [`FieldValue`].
pub fn json_to_field_value(v: &Value) -> Option<FieldValue> {
    match v {
        Value::String(s) => Some(FieldValue::Text(s.clone())),
        Value::Number(n) => n.as_f64().map(FieldValue::Number),
        Value::Bool(b) => Some(FieldValue::Boolean(*b)),
        Value::Array(arr) => {
            let items: Vec<FieldValue> = arr.iter().filter_map(json_to_field_value).collect();
            Some(FieldValue::Array(items))
        }
        _ => None,
    }
}

/// Convert a [`HashMap<String, FieldValue>`] to a [`serde_json::Value`] object.
pub fn fields_to_value(fields: &HashMap<String, FieldValue>) -> Value {
    let map: serde_json::Map<String, Value> = fields
        .iter()
        .map(|(k, v)| (k.clone(), field_value_to_json(v)))
        .collect();
    Value::Object(map)
}

/// Convert an [`IndexDocumentRequest`] into a core [`Document`].
pub fn request_to_document(req: IndexDocumentRequest) -> Document {
    let id = req
        .id
        .map(DocumentId::new)
        .unwrap_or_else(DocumentId::generate);

    let mut doc = Document::new(id);
    for (key, val) in req.fields {
        if let Some(fv) = json_to_field_value(&val) {
            doc.set_field(key, fv);
        }
    }
    doc
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_dsl_match_parse() {
        let json = r#"{"match": {"title": {"query": "rust programming", "operator": "and"}}}"#;
        let dsl: QueryDsl = serde_json::from_str(json).unwrap();

        let query = dsl.into_core_query();
        match &query {
            Query::FullText(ft) => {
                assert_eq!(ft.field, "title");
                assert_eq!(ft.query, "rust programming");
                assert_eq!(ft.operator, Operator::And);
            }
            _ => panic!("expected FullText query"),
        }
    }

    #[test]
    fn query_dsl_match_default_operator() {
        let json = r#"{"match": {"body": {"query": "hello world"}}}"#;
        let dsl: QueryDsl = serde_json::from_str(json).unwrap();

        let query = dsl.into_core_query();
        match &query {
            Query::FullText(ft) => {
                assert_eq!(ft.operator, Operator::Or);
            }
            _ => panic!("expected FullText query"),
        }
    }

    #[test]
    fn query_dsl_term_parse() {
        let json = r#"{"term": {"status": "published"}}"#;
        let dsl: QueryDsl = serde_json::from_str(json).unwrap();

        let query = dsl.into_core_query();
        match &query {
            Query::Term(t) => {
                assert_eq!(t.field, "status");
                assert_eq!(t.value, "published");
            }
            _ => panic!("expected Term query"),
        }
    }

    #[test]
    fn query_dsl_range_parse() {
        let json = r#"{"range": {"price": {"gte": 10.0, "lte": 100.0}}}"#;
        let dsl: QueryDsl = serde_json::from_str(json).unwrap();

        let query = dsl.into_core_query();
        match &query {
            Query::Range(r) => {
                assert_eq!(r.field, "price");
                assert_eq!(r.gte, Some(10.0));
                assert_eq!(r.lte, Some(100.0));
            }
            _ => panic!("expected Range query"),
        }
    }

    #[test]
    fn query_dsl_bool_parse() {
        let json = r#"{
            "bool": {
                "must": [
                    {"term": {"status": "active"}}
                ],
                "should": [
                    {"match": {"title": {"query": "rust"}}}
                ],
                "must_not": [
                    {"range": {"price": {"gte": 1000.0}}}
                ]
            }
        }"#;
        let dsl: QueryDsl = serde_json::from_str(json).unwrap();

        let query = dsl.into_core_query();
        match &query {
            Query::Bool(b) => {
                assert_eq!(b.must.len(), 1);
                assert_eq!(b.should.len(), 1);
                assert_eq!(b.must_not.len(), 1);
                assert!(matches!(b.must[0], Query::Term(_)));
                assert!(matches!(b.should[0], Query::FullText(_)));
                assert!(matches!(b.must_not[0], Query::Range(_)));
            }
            _ => panic!("expected Bool query"),
        }
    }

    #[test]
    fn query_dsl_roundtrip() {
        let json = r#"{"match": {"title": {"query": "hello", "operator": "or"}}}"#;
        let dsl: QueryDsl = serde_json::from_str(json).unwrap();
        let serialized = serde_json::to_string(&dsl).unwrap();
        let back: QueryDsl = serde_json::from_str(&serialized).unwrap();
        // Re-convert both to core query and compare
        let q1 = dsl.into_core_query();
        let q2 = back.into_core_query();
        assert_eq!(q1, q2);
    }

    #[test]
    fn index_document_request_parse() {
        let json = r#"{"id": "doc-1", "fields": {"title": "hello", "count": 42}}"#;
        let req: IndexDocumentRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, Some("doc-1".to_string()));
        assert_eq!(req.fields.len(), 2);
    }

    #[test]
    fn index_document_request_no_id() {
        let json = r#"{"fields": {"title": "hello"}}"#;
        let req: IndexDocumentRequest = serde_json::from_str(json).unwrap();
        assert!(req.id.is_none());
    }

    #[test]
    fn index_document_response_serialize() {
        let resp = IndexDocumentResponse {
            id: "doc-1".into(),
            result: "created".into(),
            version: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"_id\""));
        assert!(json.contains("created"));
    }

    #[test]
    fn search_response_from_result() {
        let result = SearchResult {
            documents: vec![ScoredDocument {
                document: Document::new(DocumentId::new("d1"))
                    .with_field("title", FieldValue::Text("hello".into())),
                score: 1.5,
            }],
            total: 1,
            took_ms: 5,
        };

        let resp = SearchResponse::from_search_result(result);
        assert_eq!(resp.hits.total.value, 1);
        assert_eq!(resp.hits.hits.len(), 1);
        assert_eq!(resp.hits.hits[0].id, "d1");
        assert_eq!(resp.took, 5);
        assert!(!resp.timed_out);
    }

    #[test]
    fn error_response_not_found() {
        let err = ErrorResponse::not_found("document 'x' not found");
        assert_eq!(err.status, 404);
        assert_eq!(err.error.error_type, "not_found");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("404"));
    }

    #[test]
    fn error_response_bad_request() {
        let err = ErrorResponse::bad_request("invalid JSON");
        assert_eq!(err.status, 400);
        assert_eq!(err.error.error_type, "invalid_input");
    }

    #[test]
    fn error_response_internal() {
        let err = ErrorResponse::internal("disk failure");
        assert_eq!(err.status, 500);
    }

    #[test]
    fn error_response_unauthorized() {
        let err = ErrorResponse::unauthorized("invalid API key");
        assert_eq!(err.status, 401);
    }

    #[test]
    fn error_response_redirect() {
        let err = ErrorResponse::redirect("forward to leader");
        assert_eq!(err.status, 307);
    }

    #[test]
    fn bulk_action_index_parse() {
        let json = r#"{"index": {"_id": "doc-1"}}"#;
        let action: BulkAction = serde_json::from_str(json).unwrap();
        match action {
            BulkAction::Index(meta) => assert_eq!(meta.id, Some("doc-1".into())),
            _ => panic!("expected Index"),
        }
    }

    #[test]
    fn bulk_action_delete_parse() {
        let json = r#"{"delete": {"_id": "doc-2"}}"#;
        let action: BulkAction = serde_json::from_str(json).unwrap();
        match action {
            BulkAction::Delete(meta) => assert_eq!(meta.id, Some("doc-2".into())),
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn bulk_response_serialize() {
        let resp = BulkResponse {
            took: 100,
            errors: false,
            items: vec![BulkItem {
                action: "index".into(),
                id: "d1".into(),
                status: 201,
                error: None,
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"took\":100"));
        assert!(json.contains("\"_id\":\"d1\""));
    }

    #[test]
    fn field_value_json_roundtrip() {
        let fv = FieldValue::Text("hello".into());
        let json = field_value_to_json(&fv);
        let back = json_to_field_value(&json).unwrap();
        assert_eq!(fv, back);

        let fv = FieldValue::Number(42.5);
        let json = field_value_to_json(&fv);
        let back = json_to_field_value(&json).unwrap();
        assert_eq!(fv, back);

        let fv = FieldValue::Boolean(true);
        let json = field_value_to_json(&fv);
        let back = json_to_field_value(&json).unwrap();
        assert_eq!(fv, back);
    }

    #[test]
    fn request_to_document_with_id() {
        let req = IndexDocumentRequest {
            id: Some("my-doc".into()),
            fields: {
                let mut m = HashMap::new();
                m.insert("title".into(), Value::String("test".into()));
                m
            },
        };
        let doc = request_to_document(req);
        assert_eq!(doc.id.as_str(), "my-doc");
        assert_eq!(
            doc.get_field("title"),
            Some(&FieldValue::Text("test".into()))
        );
    }

    #[test]
    fn request_to_document_generates_id() {
        let req = IndexDocumentRequest {
            id: None,
            fields: HashMap::new(),
        };
        let doc = request_to_document(req);
        assert!(!doc.id.as_str().is_empty());
    }

    #[test]
    fn search_request_parse() {
        let json = r#"{
            "query": {"match": {"title": {"query": "hello"}}},
            "size": 20,
            "from": 5
        }"#;
        let req: SearchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.size, Some(20));
        assert_eq!(req.from, Some(5));
    }

    #[test]
    fn search_request_defaults() {
        let json = r#"{"query": {"term": {"status": "active"}}}"#;
        let req: SearchRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.size, Some(10)); // default
        assert_eq!(req.from, None);
        assert!(req.highlight.is_none());
        assert!(req.sort.is_none());
    }

    #[test]
    fn cluster_health_response_serialize() {
        let resp = ClusterHealthResponse {
            status: "green".into(),
            number_of_nodes: 3,
            leader_id: Some(1),
            is_leader: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("green"));
        assert!(json.contains("\"number_of_nodes\":3"));
    }

    #[test]
    fn collection_info_response_serialize() {
        let resp = CollectionInfoResponse {
            name: "articles".into(),
            doc_count: 1000,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("articles"));
        assert!(json.contains("1000"));
    }

    #[test]
    fn ndjson_bulk_request_parse() {
        let ndjson = r#"{"index": {"_id": "1"}}
{"title": "doc one", "count": 10}
{"index": {"_id": "2"}}
{"title": "doc two", "count": 20}
{"delete": {"_id": "3"}}
"#;
        let lines: Vec<&str> = ndjson.lines().collect();
        assert_eq!(lines.len(), 5);

        // Parse action lines
        let action1: BulkAction = serde_json::from_str(lines[0]).unwrap();
        assert!(matches!(action1, BulkAction::Index(_)));

        let action2: BulkAction = serde_json::from_str(lines[2]).unwrap();
        assert!(matches!(action2, BulkAction::Index(_)));

        let action3: BulkAction = serde_json::from_str(lines[4]).unwrap();
        assert!(matches!(action3, BulkAction::Delete(_)));
    }
}
