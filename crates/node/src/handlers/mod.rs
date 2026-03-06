//! HTTP request handlers for the MSearchDB REST API.
//!
//! Each sub-module corresponds to a logical group of endpoints:
//!
//! | Module | Endpoints |
//! |--------|-----------|
//! | [`collections`] | `PUT/DELETE/GET /collections/{name}` |
//! | [`documents`] | `POST/PUT/GET/DELETE /collections/{name}/docs/{id}` |
//! | [`search`] | `POST/GET /collections/{name}/_search` |
//! | [`bulk`] | `POST /collections/{name}/docs/_bulk` |
//! | [`cluster`] | `GET /_cluster/health`, `GET /_cluster/state`, `GET /_nodes` |
//! | [`admin`] | `POST /_refresh`, `GET /_stats` |

pub mod admin;
pub mod bulk;
pub mod cluster;
pub mod collections;
pub mod documents;
pub mod search;
