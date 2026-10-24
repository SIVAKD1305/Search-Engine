//! HTTP API modeled on Algolia's REST API (`/1/indexes/...`), so its JSON and
//! headers look familiar to Algolia clients.
//!
//! Keys travel in the `X-Algolia-API-Key` header (or the
//! `x-algolia-api-key` query parameter). The admin key can do everything;
//! the search key can only search.
//!
//! Writes are applied before the response is sent, so every returned
//! `taskID` is already `published`. With a data directory, each write is
//! first made durable in the index's write-ahead log (see `storage`).

pub mod params;
pub mod storage;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use engine::index::{Index, Settings};
use engine::search::{SearchParams, search};
use params::ResponseOptions;
use storage::{Entry, IndexStore, StorageError};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use tower_http::cors::CorsLayer;

/// Largest accepted request body (batches of records).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError { status: StatusCode::BAD_REQUEST, message: message.into() }
    }

    fn not_found(message: impl Into<String>) -> Self {
        ApiError { status: StatusCode::NOT_FOUND, message: message.into() }
    }

    fn storage(e: StorageError) -> Self {
        eprintln!("{e}");
        ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: "could not save the change".into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "message": self.message, "status": self.status.as_u16() });
        (self.status, Json(body)).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;
/// One index and its storage. Writers take `store` first, so writes to an
/// index are serialized while searches keep reading `index`; the index lock
/// is held for writing only while changes are applied in memory.
pub struct IndexEntry {
    index: RwLock<Index>,
    store: Mutex<Option<IndexStore>>,
}

type SharedIndex = Arc<IndexEntry>;

pub struct AppState {
    indexes: RwLock<HashMap<String, SharedIndex>>,
    /// `None` keeps everything in memory.
    data_dir: Option<PathBuf>,
    admin_key: String,
    search_key: String,
    next_task: AtomicU64,
    next_object: AtomicU64,
}

impl AppState {
    /// A server that keeps its data in memory only.
    pub fn new(admin_key: String, search_key: String) -> Self {
        assert!(!admin_key.is_empty() && !search_key.is_empty(), "API keys must not be empty");
        AppState {
            indexes: RwLock::new(HashMap::new()),
            data_dir: None,
            admin_key,
            search_key,
            next_task: AtomicU64::new(1),
            next_object: AtomicU64::new(1),
        }
    }

    /// A server that stores its data under `dir`, loading what is there.
    pub fn open(admin_key: String, search_key: String, dir: &FsPath) -> Result<Self, StorageError> {
        let mut state = AppState::new(admin_key, search_key);
        state.data_dir = Some(dir.to_owned());
        for (name, index, store) in storage::open_all(dir)? {
            let entry = IndexEntry { index: RwLock::new(index), store: Mutex::new(Some(store)) };
            state.indexes.get_mut().unwrap().insert(name, Arc::new(entry));
        }
        Ok(state)
    }

    pub fn index_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.indexes.read().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    /// Adds or replaces an index with a pre-built one (used at startup).
    pub fn insert_index(&self, name: &str, index: Index) -> Result<(), StorageError> {
        validate_index_name(name).map_err(|e| StorageError::Corrupt(e.message))?;
        if let Some(old) = self.indexes.write().unwrap().remove(name)
            && let Some(store) = old.store.lock().unwrap().take()
        {
            store.destroy()?;
        }
        let store = match &self.data_dir {
            Some(dir) => Some(IndexStore::create(&storage::index_dir(dir, name), &index)?),
            None => None,
        };
        let entry = IndexEntry { index: RwLock::new(index), store: Mutex::new(store) };
        self.indexes.write().unwrap().insert(name.to_owned(), Arc::new(entry));
        Ok(())
    }

    fn index(&self, name: &str) -> Result<SharedIndex, ApiError> {
        self.indexes
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("Index {name} does not exist")))
    }

    /// Writes create the index if needed, as in Algolia.
    fn index_or_create(&self, name: &str) -> Result<SharedIndex, ApiError> {
        validate_index_name(name)?;
        let mut indexes = self.indexes.write().unwrap();
        if let Some(entry) = indexes.get(name) {
            return Ok(entry.clone());
        }
        let index = Index::new(Settings::default());
        let store = match &self.data_dir {
            Some(dir) => Some(IndexStore::create(&storage::index_dir(dir, name), &index).map_err(ApiError::storage)?),
            None => None,
        };
        let entry = Arc::new(IndexEntry { index: RwLock::new(index), store: Mutex::new(store) });
        indexes.insert(name.to_owned(), entry.clone());
        Ok(entry)
    }

    fn task(&self) -> u64 {
        self.next_task.fetch_add(1, Ordering::Relaxed)
    }

    /// An objectID for records added without one.
    fn new_object_id(&self) -> String {
        let n = self.next_object.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        format!("{nanos:x}{n:x}")
    }
}

fn validate_index_name(name: &str) -> Result<(), ApiError> {
    // Names are also directory names, so "." and ".." must not be possible.
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 256
        && name.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c));
    if ok { Ok(()) } else { Err(ApiError::bad_request("index names use letters, digits, '-', '_' and '.'")) }
}

/// The current time in RFC 3339, as Algolia returns it.
fn now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    rfc3339(secs)
}

/// Formats seconds since 1970 as `YYYY-MM-DDTHH:MM:SSZ` (UTC), using Howard
/// Hinnant's days-to-civil algorithm.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Search,
    Admin,
}

fn presented_key<'a>(headers: &'a HeaderMap, query: Option<&'a str>) -> Option<String> {
    if let Some(key) = headers.get("x-algolia-api-key").and_then(|v| v.to_str().ok()) {
        return Some(key.to_owned());
    }
    form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k.eq_ignore_ascii_case("x-algolia-api-key"))
        .map(|(_, v)| v.into_owned())
}

fn key_matches(presented: &str, expected: &str) -> bool {
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

async fn require(state: Arc<AppState>, needed: Access, request: Request, next: Next) -> Response {
    let key = presented_key(request.headers(), request.uri().query());
    let access = match key.as_deref() {
        Some(k) if key_matches(k, &state.admin_key) => Some(Access::Admin),
        Some(k) if key_matches(k, &state.search_key) => Some(Access::Search),
        _ => None,
    };
    match access {
        None => ApiError { status: StatusCode::FORBIDDEN, message: "Invalid Application-ID or API key".into() }
            .into_response(),
        Some(Access::Search) if needed == Access::Admin => {
            ApiError { status: StatusCode::FORBIDDEN, message: "Method not allowed with this API key".into() }
                .into_response()
        }
        Some(_) => next.run(request).await,
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    // Algolia's multi-query path has a literal `*` segment.
    let search_routes = Router::new()
        .without_v07_checks()
        .route("/1/indexes/{index}/query", post(query))
        .route("/1/indexes/*/queries", post(multi_query))
        .route("/1/indexes/{index}", get(query_get))
        .route_layer(middleware::from_fn_with_state(state.clone(), |State(s), req, next| {
            require(s, Access::Search, req, next)
        }));

    let admin_routes = Router::new()
        .route("/1/indexes", get(list_indexes))
        .route("/1/indexes/{index}", post(add_object).delete(delete_index))
        .route("/1/indexes/{index}/batch", post(batch))
        .route("/1/indexes/{index}/clear", post(clear_index))
        .route("/1/indexes/{index}/settings", get(get_settings).put(set_settings))
        .route("/1/indexes/{index}/task/{task}", get(task_status))
        .route("/1/indexes/{index}/{object_id}", get(get_object).put(put_object).delete(delete_object))
        .route("/1/indexes/{index}/{object_id}/partial", post(partial_update))
        .route_layer(middleware::from_fn_with_state(state.clone(), |State(s), req, next| {
            require(s, Access::Admin, req, next)
        }));

    Router::new()
        .without_v07_checks()
        .route("/1/isalive", get(|| async { Json(json!({ "message": "server is alive" })) }))
        .merge(search_routes)
        .merge(admin_routes)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Algolia's JS client sends JSON without a JSON content type (to avoid CORS
/// preflights), so the body is parsed whatever the header says.
fn json_body(body: &Bytes) -> Result<Value, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid JSON body: {e}")))
}

fn json_object(body: &Bytes) -> Result<Map<String, Value>, ApiError> {
    match json_body(body)? {
        Value::Object(o) => Ok(o),
        _ => Err(ApiError::bad_request("request body must be a JSON object")),
    }
}

// ---- search ----

fn run_search(index: &Index, params: &SearchParams, options: &ResponseOptions) -> Result<Value, ApiError> {
    let start = Instant::now();
    // Without an explicit list, highlight only attributes that are returned.
    let narrowed;
    let params = match (&options.attributes_to_retrieve, &params.attributes_to_highlight) {
        (Some(retrieved), None) => {
            let attributes = index.attributes().iter().filter(|a| retrieved.contains(a)).cloned().collect();
            narrowed = SearchParams { attributes_to_highlight: Some(attributes), ..params.clone() };
            &narrowed
        }
        _ => params,
    };
    let results = search(index, params).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let hits: Vec<Value> = results
        .hits
        .iter()
        .map(|hit| {
            let record = index.store().get(hit.doc).expect("hit is live");
            let mut out: Map<String, Value> = match &options.attributes_to_retrieve {
                Some(attrs) => record
                    .iter()
                    .filter(|(k, _)| *k == "objectID" || attrs.contains(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                None => record.clone(),
            };
            if !hit.highlight.is_empty() {
                out.insert("_highlightResult".into(), Value::Object(hit.highlight.clone()));
            }
            if !hit.snippet.is_empty() {
                out.insert("_snippetResult".into(), Value::Object(hit.snippet.clone()));
            }
            if options.get_ranking_info {
                out.insert("_rankingInfo".into(), serde_json::to_value(&hit.info).expect("serializable"));
            }
            Value::Object(out)
        })
        .collect();
    let facets: Map<String, Value> = results
        .facets
        .iter()
        .map(|(attr, counts)| {
            let values: Map<String, Value> = counts.iter().map(|c| (c.value.clone(), json!(c.count))).collect();
            (attr.clone(), Value::Object(values))
        })
        .collect();
    let mut response = json!({
        "hits": hits,
        "nbHits": results.nb_hits,
        "page": results.page,
        "nbPages": results.nb_pages,
        "hitsPerPage": results.hits_per_page,
        "exhaustiveNbHits": true,
        "query": params.query,
        "processingTimeMS": start.elapsed().as_millis() as u64,
    });
    if !params.facets.is_empty() {
        response["facets"] = Value::Object(facets);
    }
    if let Some(query) = &results.query_after_removal {
        response["queryAfterRemoval"] = json!(query);
    }
    Ok(response)
}

fn search_index(state: &AppState, name: &str, map: Map<String, Value>) -> Result<Value, ApiError> {
    let (params, options) = params::into_search_params(map)?;
    let entry = state.index(name)?;
    let index = entry.index.read().unwrap();
    let mut response = run_search(&index, &params, &options)?;
    response["index"] = json!(name);
    Ok(response)
}

async fn query(State(state): State<Arc<AppState>>, Path(name): Path<String>, body: Bytes) -> ApiResult {
    let map = params::from_body(&json_body(&body)?)?;
    Ok(Json(search_index(&state, &name, map)?))
}

async fn query_get(State(state): State<Arc<AppState>>, Path(name): Path<String>, RawQuery(q): RawQuery) -> ApiResult {
    let mut map = params::from_query_string(q.as_deref().unwrap_or(""));
    map.retain(|k, _| !k.to_ascii_lowercase().starts_with("x-algolia-"));
    Ok(Json(search_index(&state, &name, map)?))
}

async fn multi_query(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult {
    let body = json_object(&body)?;
    let Some(Value::Array(requests)) = body.get("requests") else {
        return Err(ApiError::bad_request("body needs a requests array"));
    };
    let mut results = Vec::with_capacity(requests.len());
    for request in requests {
        let name = request
            .get("indexName")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad_request("every request needs an indexName"))?;
        results.push(search_index(&state, name, params::from_body(request)?)?);
    }
    Ok(Json(json!({ "results": results })))
}

// ---- writes ----

/// A requested change, before it is resolved against the stored records.
enum Op {
    Put(String, Map<String, Value>),
    Partial { id: String, changes: Map<String, Value>, create: bool },
    Delete(String),
    Clear,
    Settings(Settings),
}

fn object_id_of(record: &Map<String, Value>) -> Option<String> {
    match record.get("objectID") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Turns operations into log entries. Partial updates become full records,
/// looking first at earlier operations of the same request.
fn resolve(index: &Index, ops: Vec<Op>) -> Vec<Entry> {
    let mut pending: HashMap<String, Option<Map<String, Value>>> = HashMap::new();
    let mut cleared = false;
    let mut entries = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Put(id, mut record) => {
                record.insert("objectID".into(), Value::String(id.clone()));
                pending.insert(id, Some(record.clone()));
                entries.push(Entry::Put { record });
            }
            Op::Partial { id, changes, create } => {
                let current = match pending.get(&id) {
                    Some(record) => record.clone(),
                    None if cleared => None,
                    None => index.get_object(&id).cloned(),
                };
                let mut record = match current {
                    Some(record) => record,
                    None if create => Map::new(),
                    None => continue,
                };
                record.extend(changes);
                record.insert("objectID".into(), Value::String(id.clone()));
                pending.insert(id, Some(record.clone()));
                entries.push(Entry::Put { record });
            }
            Op::Delete(id) => {
                pending.insert(id.clone(), None);
                entries.push(Entry::Delete { object_id: id });
            }
            Op::Clear => {
                pending.clear();
                cleared = true;
                entries.push(Entry::Clear);
            }
            Op::Settings(settings) => entries.push(Entry::Settings { settings }),
        }
    }
    entries
}

/// Logs the changes durably, then applies them in memory.
fn commit(entry: &SharedIndex, ops: Vec<Op>) -> Result<(), ApiError> {
    let mut store = entry.store.lock().unwrap();
    let entries = resolve(&entry.index.read().unwrap(), ops);
    if let Some(store) = store.as_mut() {
        store.append(&entries).map_err(ApiError::storage)?;
    }
    {
        let mut index = entry.index.write().unwrap();
        for e in &entries {
            e.apply(&mut index);
        }
    }
    if let Some(store) = store.as_mut()
        && store.needs_snapshot()
    {
        // The log still holds everything, so a failed snapshot loses nothing.
        if let Err(e) = store.snapshot(&entry.index.read().unwrap()) {
            eprintln!("snapshot failed: {e}");
        }
    }
    Ok(())
}

// ---- indexes and settings ----

async fn list_indexes(State(state): State<Arc<AppState>>) -> ApiResult {
    let indexes = state.indexes.read().unwrap();
    let mut items: Vec<Value> = indexes
        .iter()
        .map(|(name, entry)| json!({ "name": name, "entries": entry.index.read().unwrap().stats().records }))
        .collect();
    items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(json!({ "items": items, "nbPages": 1 })))
}

async fn delete_index(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> ApiResult {
    let entry = state
        .indexes
        .write()
        .unwrap()
        .remove(&name)
        .ok_or_else(|| ApiError::not_found(format!("Index {name} does not exist")))?;
    if let Some(store) = entry.store.lock().unwrap().take() {
        store.destroy().map_err(|e| ApiError::storage(e.into()))?;
    }
    Ok(Json(json!({ "taskID": state.task(), "deletedAt": now() })))
}

async fn clear_index(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> ApiResult {
    commit(&state.index(&name)?, vec![Op::Clear])?;
    Ok(Json(json!({ "taskID": state.task(), "updatedAt": now() })))
}

async fn get_settings(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> ApiResult {
    let entry = state.index(&name)?;
    let settings = serde_json::to_value(entry.index.read().unwrap().settings()).expect("serializable");
    Ok(Json(settings))
}

/// Only the given settings change; the others keep their values.
async fn set_settings(State(state): State<Arc<AppState>>, Path(name): Path<String>, body: Bytes) -> ApiResult {
    let changes = json_object(&body)?;
    let entry = state.index_or_create(&name)?;
    let mut merged = serde_json::to_value(entry.index.read().unwrap().settings()).expect("serializable");
    for (k, v) in changes {
        merged[k] = v;
    }
    let settings: Settings =
        serde_json::from_value(merged).map_err(|e| ApiError::bad_request(format!("invalid settings: {e}")))?;
    commit(&entry, vec![Op::Settings(settings)])?;
    Ok(Json(json!({ "taskID": state.task(), "updatedAt": now() })))
}

async fn task_status(State(state): State<Arc<AppState>>, Path((name, _task)): Path<(String, u64)>) -> ApiResult {
    state.index(&name)?;
    Ok(Json(json!({ "status": "published", "pendingTask": false })))
}

// ---- records ----

async fn add_object(State(state): State<Arc<AppState>>, Path(name): Path<String>, body: Bytes) -> ApiResult {
    let record = json_object(&body)?;
    let object_id = object_id_of(&record).unwrap_or_else(|| state.new_object_id());
    commit(&state.index_or_create(&name)?, vec![Op::Put(object_id.clone(), record)])?;
    Ok(Json(json!({ "createdAt": now(), "taskID": state.task(), "objectID": object_id })))
}

async fn get_object(State(state): State<Arc<AppState>>, Path((name, object_id)): Path<(String, String)>) -> ApiResult {
    let entry = state.index(&name)?;
    let index = entry.index.read().unwrap();
    let record = index.get_object(&object_id).ok_or_else(|| ApiError::not_found("ObjectID does not exist"))?;
    Ok(Json(Value::Object(record.clone())))
}

async fn put_object(
    State(state): State<Arc<AppState>>,
    Path((name, object_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult {
    let record = json_object(&body)?;
    commit(&state.index_or_create(&name)?, vec![Op::Put(object_id.clone(), record)])?;
    Ok(Json(json!({ "updatedAt": now(), "taskID": state.task(), "objectID": object_id })))
}

async fn partial_update(
    State(state): State<Arc<AppState>>,
    Path((name, object_id)): Path<(String, String)>,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> ApiResult {
    let changes = json_object(&body)?;
    let query = params::from_query_string(q.as_deref().unwrap_or(""));
    let create = !query.get("createIfNotExists").is_some_and(|v| v == false);
    let op = Op::Partial { id: object_id.clone(), changes, create };
    commit(&state.index_or_create(&name)?, vec![op])?;
    Ok(Json(json!({ "updatedAt": now(), "taskID": state.task(), "objectID": object_id })))
}

async fn delete_object(
    State(state): State<Arc<AppState>>,
    Path((name, object_id)): Path<(String, String)>,
) -> ApiResult {
    commit(&state.index(&name)?, vec![Op::Delete(object_id.clone())])?;
    Ok(Json(json!({ "deletedAt": now(), "taskID": state.task(), "objectID": object_id })))
}

/// Algolia's batch actions. The whole batch is validated before anything is
/// written, and it is logged as one unit, so a bad request changes nothing.
async fn batch(State(state): State<Arc<AppState>>, Path(name): Path<String>, body: Bytes) -> ApiResult {
    let body = json_object(&body)?;
    let Some(Value::Array(requests)) = body.get("requests") else {
        return Err(ApiError::bad_request("body needs a requests array"));
    };
    let mut ops = Vec::with_capacity(requests.len());
    let mut object_ids = Vec::new();
    for (i, request) in requests.iter().enumerate() {
        let fail = |msg: &str| ApiError::bad_request(format!("requests[{i}]: {msg}"));
        let action = request.get("action").and_then(Value::as_str).ok_or_else(|| fail("missing action"))?;
        let body = match request.get("body") {
            Some(Value::Object(o)) => o.clone(),
            None | Some(Value::Null) => Map::new(),
            Some(_) => return Err(fail("body must be an object")),
        };
        let id = object_id_of(&body);
        let op = match action {
            "addObject" => Op::Put(id.unwrap_or_else(|| state.new_object_id()), body),
            "updateObject" => Op::Put(id.ok_or_else(|| fail("updateObject needs an objectID"))?, body),
            "partialUpdateObject" => {
                Op::Partial { id: id.ok_or_else(|| fail("needs an objectID"))?, changes: body, create: true }
            }
            "partialUpdateObjectNoCreate" => {
                Op::Partial { id: id.ok_or_else(|| fail("needs an objectID"))?, changes: body, create: false }
            }
            "deleteObject" => Op::Delete(id.ok_or_else(|| fail("deleteObject needs an objectID"))?),
            "clear" => Op::Clear,
            other => return Err(fail(&format!("unknown action {other:?}"))),
        };
        match &op {
            Op::Put(id, _) | Op::Partial { id, .. } | Op::Delete(id) => object_ids.push(id.clone()),
            Op::Clear | Op::Settings(_) => {}
        }
        ops.push(op);
    }
    commit(&state.index_or_create(&name)?, ops)?;
    Ok(Json(json!({ "taskID": state.task(), "objectIDs": object_ids })))
}

#[cfg(test)]
mod tests {
    use super::rfc3339;

    #[test]
    fn formats_utc_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
    }
}
