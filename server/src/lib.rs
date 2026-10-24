//! HTTP API modeled on Algolia's REST API (`/1/indexes/...`), so its JSON and
//! headers look familiar to Algolia clients.
//!
//! Keys travel in the `X-Algolia-API-Key` header (or the
//! `x-algolia-api-key` query parameter). The admin key can do everything;
//! the search key can only search; keys made with `/1/keys` and secured
//! keys have the rights and restrictions described in `keys`. A key with
//! `maxQueriesPerIPPerHour` is answered `429` past its limit, and so is a
//! client address after too many requests with unknown keys.
//!
//! Writes are applied before the response is sent, so every returned
//! `taskID` is already `published`. With a data directory, each write is
//! first made durable in the index's write-ahead log (see `storage`).

pub mod keys;
pub mod params;
pub mod replication;
pub mod state;
pub mod storage;

pub use state::AppState;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use engine::index::{Index, Settings};
use engine::search::{SearchParams, search};
use keys::{ApiKey, Caller, Grant, KeyRequest};
use params::ResponseOptions;
use serde_json::{Map, Value, json};
use state::Op;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use storage::StorageError;
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

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        ApiError { status: StatusCode::NOT_FOUND, message: message.into() }
    }

    pub(crate) fn storage(e: StorageError) -> Self {
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

pub(crate) fn validate_index_name(name: &str) -> Result<(), ApiError> {
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

/// Requests with unknown keys allowed per client address per hour, to slow
/// down guessing.
pub const MAX_FAILED_AUTH_PER_HOUR: u32 = 1000;

/// The rights of the request's key, set by `authenticate`.
type Key = Extension<Arc<Grant>>;

fn presented_key<'a>(headers: &'a HeaderMap, query: Option<&'a str>) -> Option<String> {
    if let Some(key) = headers.get("x-algolia-api-key").and_then(|v| v.to_str().ok()) {
        return Some(key.to_owned());
    }
    form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k.eq_ignore_ascii_case("x-algolia-api-key"))
        .map(|(_, v)| v.into_owned())
}

fn caller_of(state: &AppState, request: &Request) -> Caller {
    let headers = request.headers();
    let forwarded = || -> Option<IpAddr> {
        let value = headers.get("x-forwarded-for")?.to_str().ok()?;
        value.split(',').next()?.trim().parse().ok()
    };
    let ip = if state.trust_proxy { forwarded() } else { None }
        .or_else(|| request.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()));
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    Caller { ip, referer: header("referer").or_else(|| header("origin")) }
}

fn too_many_requests(wait_seconds: u64) -> Response {
    let mut response =
        ApiError { status: StatusCode::TOO_MANY_REQUESTS, message: "Too many requests".into() }.into_response();
    response.headers_mut().insert("retry-after", HeaderValue::from(wait_seconds));
    response
}

/// Checks the key of every request, applies rate limits, and passes the
/// key's rights on to the handler.
async fn authenticate(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    let caller = caller_of(&state, &request);
    let now = keys::unix_now();
    let failures = format!("unknown-key|{}", caller.ip.map_or_else(|| "unknown".into(), |ip| ip.to_string()));
    if let Err(wait) = state.limiter.peek(&failures, MAX_FAILED_AUTH_PER_HOUR, now) {
        return too_many_requests(wait);
    }
    let presented = presented_key(request.headers(), request.uri().query());
    let resolved = presented.and_then(|k| state.keys.resolve(&k, &state.admin_key, &state.search_key, &caller));
    let grant = match resolved {
        Some(Ok(grant)) => grant,
        Some(Err(e)) => return e.into_response(),
        None => {
            let _ = state.limiter.hit(&failures, MAX_FAILED_AUTH_PER_HOUR, now);
            return ApiError { status: StatusCode::FORBIDDEN, message: "Invalid Application-ID or API key".into() }
                .into_response();
        }
    };
    if let Some((limit, who)) = &grant.rate_limit
        && let Err(wait) = state.limiter.hit(who, *limit, now)
    {
        return too_many_requests(wait);
    }
    request.extensions_mut().insert(Arc::new(grant));
    next.run(request).await
}

async fn admin_only(Extension(grant): Key, request: Request, next: Next) -> Response {
    match grant.require_admin() {
        Ok(()) => next.run(request).await,
        Err(e) => e.into_response(),
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    // Algolia's multi-query path has a literal `*` segment.
    let api = Router::new()
        .without_v07_checks()
        .route("/1/indexes/{index}/query", post(query))
        .route("/1/indexes/*/queries", post(multi_query))
        .route("/1/indexes", get(list_indexes))
        .route("/1/indexes/{index}", get(query_get).post(add_object).delete(delete_index))
        .route("/1/indexes/{index}/batch", post(batch))
        .route("/1/indexes/{index}/clear", post(clear_index))
        .route("/1/indexes/{index}/settings", get(get_settings).put(set_settings))
        .route("/1/indexes/{index}/task/{task}", get(task_status))
        .route("/1/indexes/{index}/{object_id}", get(get_object).put(put_object).delete(delete_object))
        .route("/1/indexes/{index}/{object_id}/partial", post(partial_update))
        .route("/1/keys", get(list_keys).post(create_key))
        .route("/1/keys/{key}", get(get_key).put(update_key).delete(delete_key))
        .merge(replication::routes().without_v07_checks().route_layer(middleware::from_fn(admin_only)))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));

    Router::new()
        .without_v07_checks()
        .route("/1/isalive", get(|| async { Json(json!({ "message": "server is alive" })) }))
        .merge(api)
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
    if !results.facets_stats.is_empty() {
        response["facets_stats"] = serde_json::to_value(&results.facets_stats).expect("serializable");
    }
    if let Some(query) = &results.query_after_removal {
        response["queryAfterRemoval"] = json!(query);
    }
    Ok(response)
}

fn search_index(state: &AppState, grant: &Grant, name: &str, mut map: Map<String, Value>) -> Result<Value, ApiError> {
    grant.require("search", Some(name))?;
    grant.force_params(&mut map);
    let (mut params, options) = params::into_search_params(map)?;
    params.enforced_filters = grant.enforced_filters().to_vec();
    params.hits_per_page = grant.cap_hits_per_page(params.hits_per_page);
    let entry = state.index(name)?;
    let index = entry.index.read().unwrap();
    let mut response = run_search(&index, &params, &options)?;
    response["index"] = json!(name);
    Ok(response)
}

async fn query(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(name): Path<String>, body: Bytes) -> ApiResult {
    let map = params::from_body(&json_body(&body)?)?;
    Ok(Json(search_index(&state, &grant, &name, map)?))
}

async fn query_get(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path(name): Path<String>,
    RawQuery(q): RawQuery,
) -> ApiResult {
    let mut map = params::from_query_string(q.as_deref().unwrap_or(""));
    map.retain(|k, _| !k.to_ascii_lowercase().starts_with("x-algolia-"));
    Ok(Json(search_index(&state, &grant, &name, map)?))
}

async fn multi_query(State(state): State<Arc<AppState>>, Extension(grant): Key, body: Bytes) -> ApiResult {
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
        results.push(search_index(&state, &grant, name, params::from_body(request)?)?);
    }
    Ok(Json(json!({ "results": results })))
}

// ---- writes ----

/// Commits on a blocking thread: waiting for the disk (and for other
/// writers of a group commit) must not hold up the threads serving searches.
async fn commit(state: &Arc<AppState>, entry: state::SharedIndex, ops: Vec<Op>) -> Result<(), ApiError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || state.commit(&entry, ops)).await.expect("commit task")
}

fn object_id_of(record: &Map<String, Value>) -> Option<String> {
    match record.get("objectID") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

// ---- indexes and settings ----

async fn list_indexes(State(state): State<Arc<AppState>>, Extension(grant): Key) -> ApiResult {
    grant.require("listIndexes", None)?;
    let items: Vec<Value> = state
        .indexes()
        .iter()
        .filter(|(name, _)| grant.admin || grant.allows_index(name))
        .map(|(name, entry)| json!({ "name": name, "entries": entry.index.read().unwrap().stats().records }))
        .collect();
    Ok(Json(json!({ "items": items, "nbPages": 1 })))
}

async fn delete_index(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(name): Path<String>) -> ApiResult {
    grant.require("deleteIndex", Some(&name))?;
    state.delete_index(&name)?;
    Ok(Json(json!({ "taskID": state.task(), "deletedAt": now() })))
}

async fn clear_index(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(name): Path<String>) -> ApiResult {
    grant.require("deleteIndex", Some(&name))?;
    commit(&state, state.index(&name)?, vec![Op::Clear]).await?;
    Ok(Json(json!({ "taskID": state.task(), "updatedAt": now() })))
}

async fn get_settings(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(name): Path<String>) -> ApiResult {
    grant.require("settings", Some(&name))?;
    let entry = state.index(&name)?;
    let settings = serde_json::to_value(entry.index.read().unwrap().settings()).expect("serializable");
    Ok(Json(settings))
}

/// Only the given settings change; the others keep their values.
async fn set_settings(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult {
    grant.require("editSettings", Some(&name))?;
    let changes = json_object(&body)?;
    let entry = state.index_or_create(&name)?;
    let mut merged = serde_json::to_value(entry.index.read().unwrap().settings()).expect("serializable");
    for (k, v) in changes {
        merged[k] = v;
    }
    let settings: Settings =
        serde_json::from_value(merged).map_err(|e| ApiError::bad_request(format!("invalid settings: {e}")))?;
    commit(&state, entry, vec![Op::Settings(settings)]).await?;
    Ok(Json(json!({ "taskID": state.task(), "updatedAt": now() })))
}

async fn task_status(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path((name, _task)): Path<(String, u64)>,
) -> ApiResult {
    if !grant.admin && !grant.allows_index(&name) {
        return Err(ApiError { status: StatusCode::FORBIDDEN, message: "Index not allowed with this API key".into() });
    }
    state.index(&name)?;
    Ok(Json(json!({ "status": "published", "pendingTask": false })))
}

// ---- records ----

async fn add_object(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(name): Path<String>, body: Bytes) -> ApiResult {
    grant.require("addObject", Some(&name))?;
    let record = json_object(&body)?;
    let object_id = object_id_of(&record).unwrap_or_else(|| state.new_object_id());
    commit(&state, state.index_or_create(&name)?, vec![Op::Put(object_id.clone(), record)]).await?;
    Ok(Json(json!({ "createdAt": now(), "taskID": state.task(), "objectID": object_id })))
}

async fn get_object(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path((name, object_id)): Path<(String, String)>,
) -> ApiResult {
    grant.require("search", Some(&name))?;
    if grant.restricts_records() {
        // The key's filters or hidden attributes apply to searches only.
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "Reading records by objectID is not allowed with this API key".into(),
        });
    }
    let entry = state.index(&name)?;
    let index = entry.index.read().unwrap();
    let record = index.get_object(&object_id).ok_or_else(|| ApiError::not_found("ObjectID does not exist"))?;
    Ok(Json(Value::Object(record.clone())))
}

async fn put_object(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path((name, object_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult {
    grant.require("addObject", Some(&name))?;
    let record = json_object(&body)?;
    commit(&state, state.index_or_create(&name)?, vec![Op::Put(object_id.clone(), record)]).await?;
    Ok(Json(json!({ "updatedAt": now(), "taskID": state.task(), "objectID": object_id })))
}

async fn partial_update(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path((name, object_id)): Path<(String, String)>,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> ApiResult {
    grant.require("addObject", Some(&name))?;
    let changes = json_object(&body)?;
    let query = params::from_query_string(q.as_deref().unwrap_or(""));
    let create = !query.get("createIfNotExists").is_some_and(|v| v == false);
    let op = Op::Partial { id: object_id.clone(), changes, create };
    commit(&state, state.index_or_create(&name)?, vec![op]).await?;
    Ok(Json(json!({ "updatedAt": now(), "taskID": state.task(), "objectID": object_id })))
}

async fn delete_object(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path((name, object_id)): Path<(String, String)>,
) -> ApiResult {
    grant.require("deleteObject", Some(&name))?;
    commit(&state, state.index(&name)?, vec![Op::Delete(object_id.clone())]).await?;
    Ok(Json(json!({ "deletedAt": now(), "taskID": state.task(), "objectID": object_id })))
}

/// Algolia's batch actions. The whole batch is validated before anything is
/// written, and it is logged as one unit, so a bad request changes nothing.
async fn batch(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(name): Path<String>, body: Bytes) -> ApiResult {
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
        let acl = match &op {
            Op::Put(..) | Op::Partial { .. } => "addObject",
            Op::Delete(_) => "deleteObject",
            Op::Clear | Op::Settings(_) => "deleteIndex",
        };
        grant.require(acl, Some(&name))?;
        match &op {
            Op::Put(id, _) | Op::Partial { id, .. } | Op::Delete(id) => object_ids.push(id.clone()),
            Op::Clear | Op::Settings(_) => {}
        }
        ops.push(op);
    }
    commit(&state, state.index_or_create(&name)?, ops).await?;
    Ok(Json(json!({ "taskID": state.task(), "objectIDs": object_ids })))
}

// ---- API keys (admin key only) ----

fn key_storage_error(e: std::io::Error) -> ApiError {
    ApiError::storage(StorageError::Io(e))
}

async fn list_keys(State(state): State<Arc<AppState>>, Extension(grant): Key) -> ApiResult {
    grant.require_admin()?;
    Ok(Json(json!({ "keys": state.keys.list() })))
}

async fn create_key(State(state): State<Arc<AppState>>, Extension(grant): Key, body: Bytes) -> ApiResult {
    grant.require_admin()?;
    state.ensure_writable()?;
    let request: KeyRequest = serde_json::from_value(Value::Object(json_object(&body)?))
        .map_err(|e| ApiError::bad_request(format!("invalid key: {e}")))?;
    let mut key = ApiKey {
        value: keys::new_key_value(),
        acl: Vec::new(),
        description: String::new(),
        indexes: Vec::new(),
        max_hits_per_query: 0,
        max_queries_per_ip_per_hour: 0,
        query_parameters: String::new(),
        referers: Vec::new(),
        validity: 0,
        created_at: keys::unix_now(),
    };
    request.apply(&mut key)?;
    let value = key.value.clone();
    state.keys.put(key).map_err(key_storage_error)?;
    state.changed();
    Ok(Json(json!({ "key": value, "createdAt": now() })))
}

fn stored_key(state: &AppState, value: &str) -> Result<ApiKey, ApiError> {
    state.keys.get(value).ok_or_else(|| ApiError::not_found("Key does not exist"))
}

async fn get_key(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(value): Path<String>) -> ApiResult {
    grant.require_admin()?;
    Ok(Json(serde_json::to_value(stored_key(&state, &value)?).expect("serializable")))
}

/// Only the given fields change.
async fn update_key(
    State(state): State<Arc<AppState>>,
    Extension(grant): Key,
    Path(value): Path<String>,
    body: Bytes,
) -> ApiResult {
    grant.require_admin()?;
    state.ensure_writable()?;
    let request: KeyRequest = serde_json::from_value(Value::Object(json_object(&body)?))
        .map_err(|e| ApiError::bad_request(format!("invalid key: {e}")))?;
    let mut key = stored_key(&state, &value)?;
    request.apply(&mut key)?;
    state.keys.put(key).map_err(key_storage_error)?;
    state.changed();
    Ok(Json(json!({ "key": value, "updatedAt": now() })))
}

async fn delete_key(State(state): State<Arc<AppState>>, Extension(grant): Key, Path(value): Path<String>) -> ApiResult {
    grant.require_admin()?;
    state.ensure_writable()?;
    if !state.keys.remove(&value).map_err(key_storage_error)? {
        return Err(ApiError::not_found("Key does not exist"));
    }
    state.changed();
    Ok(Json(json!({ "deletedAt": now() })))
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
