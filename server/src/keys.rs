//! API keys, following Algolia's model.
//!
//! - The admin key (`SEARCH_ADMIN_KEY`) can do everything, including
//!   managing keys.
//! - The search key (`SEARCH_SEARCH_KEY`) can search every index.
//! - Keys made with `POST /1/keys` have a list of rights (`acl`) and,
//!   optionally, index patterns (`dev_*`), an expiry (`validity`, seconds), a
//!   rate limit (`maxQueriesPerIPPerHour`), a page size cap
//!   (`maxHitsPerQuery`), search parameters forced on every query
//!   (`queryParameters`) and allowed referers.
//! - Secured API keys are made by an application's backend from a key with
//!   the `search` right, without calling the server:
//!   `base64(hex(HMAC-SHA256(parent, params)) + params)`, exactly as
//!   Algolia's `generateSecuredApiKey`. `params` is a query string of
//!   restrictions: `filters`, `validUntil` (Unix time), `restrictIndices`,
//!   `restrictSources` (IP ranges), `userToken` (who the rate limit
//!   counts), and any search parameter to force. The server finds the
//!   parent by checking the HMAC against every key with the `search` right,
//!   so the restrictions cannot be changed without the parent key. A secured
//!   key can only search, and only within its parent's limits.
//!
//! Filters from keys are applied as separate conditions
//! (`SearchParams::enforced_filters`), never joined into the request's
//! filter text, so no request can lift them.

use crate::ApiError;
use crate::params;
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use subtle::ConstantTimeEq;

/// Rights a key can have (Algolia's ACL names). Only some have endpoints
/// here; the others are accepted so Algolia key definitions load as they are.
pub const ACLS: &[&str] = &[
    "search",
    "browse",
    "addObject",
    "deleteObject",
    "deleteIndex",
    "listIndexes",
    "settings",
    "editSettings",
    "analytics",
    "logs",
    "recommendation",
    "usage",
    "seeUnretrievableAttributes",
];

/// A stored key, in Algolia's JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    pub value: String,
    pub acl: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// Index name patterns (`*` matches anything); empty allows every index.
    #[serde(default)]
    pub indexes: Vec<String>,
    /// Largest `hitsPerPage` a query may ask for; 0 is no limit.
    #[serde(default)]
    pub max_hits_per_query: u32,
    /// Requests per hour from one IP address (or one `userToken` of a
    /// secured key); 0 is no limit.
    #[serde(default, rename = "maxQueriesPerIPPerHour")]
    pub max_queries_per_ip_per_hour: u32,
    /// Search parameters forced on every query, as a query string.
    #[serde(default)]
    pub query_parameters: String,
    /// Allowed `Referer` (or `Origin`) patterns; empty allows any request.
    #[serde(default)]
    pub referers: Vec<String>,
    /// Seconds the key is valid after it is created; 0 is forever.
    #[serde(default)]
    pub validity: u64,
    /// Unix time.
    #[serde(default)]
    pub created_at: u64,
}

/// The fields a client sets when creating or updating a key.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct KeyRequest {
    pub acl: Option<Vec<String>>,
    pub description: Option<String>,
    pub indexes: Option<Vec<String>>,
    pub max_hits_per_query: Option<u32>,
    #[serde(rename = "maxQueriesPerIPPerHour")]
    pub max_queries_per_ip_per_hour: Option<u32>,
    pub query_parameters: Option<String>,
    pub referers: Option<Vec<String>>,
    pub validity: Option<u64>,
}

impl KeyRequest {
    /// Applies the request to `key` (only the fields it sets).
    pub fn apply(self, key: &mut ApiKey) -> Result<(), ApiError> {
        if let Some(acl) = self.acl {
            if let Some(bad) = acl.iter().find(|a| !ACLS.contains(&a.as_str())) {
                return Err(ApiError::bad_request(format!("unknown ACL {bad:?}; known: {}", ACLS.join(", "))));
            }
            key.acl = acl;
        }
        if let Some(query) = self.query_parameters {
            parse_restrictions(&query).map_err(ApiError::bad_request)?;
            key.query_parameters = query;
        }
        if let Some(v) = self.description {
            key.description = v;
        }
        if let Some(v) = self.indexes {
            key.indexes = v;
        }
        if let Some(v) = self.max_hits_per_query {
            key.max_hits_per_query = v;
        }
        if let Some(v) = self.max_queries_per_ip_per_hour {
            key.max_queries_per_ip_per_hour = v;
        }
        if let Some(v) = self.referers {
            key.referers = v;
        }
        if let Some(v) = self.validity {
            key.validity = v;
        }
        if key.acl.is_empty() {
            return Err(ApiError::bad_request("a key needs at least one ACL"));
        }
        Ok(())
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A new random key value (32 hex characters, like Algolia's).
pub fn new_key_value() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn hmac_hex(parent: &str, params: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(parent.as_bytes()).expect("HMAC takes any key length");
    mac.update(params.as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Algolia's `generateSecuredApiKey`: `params` is a URL query string such
/// as `filters=user%3A42&validUntil=1767225600`.
pub fn generate_secured_key(parent: &str, params: &str) -> String {
    BASE64.encode(format!("{}{params}", hmac_hex(parent, params)))
}

/// Splits a secured key into (HMAC hex, params), if it has that shape.
fn decode_secured_key(key: &str) -> Option<(String, String)> {
    let text = String::from_utf8(BASE64.decode(key.trim()).ok()?).ok()?;
    if text.len() < 64 || !text.is_char_boundary(64) {
        return None;
    }
    let (mac, params) = text.split_at(64);
    mac.bytes().all(|b| b.is_ascii_hexdigit()).then(|| (mac.to_ascii_lowercase(), params.to_owned()))
}

/// `*` matches any run of characters; everything else matches itself.
pub fn pattern_matches(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || !text[first.len()..].ends_with(last) || text.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

/// Whether `ip` is in `range`: an address, or `address/prefix-length`.
pub fn ip_in_range(ip: IpAddr, range: &str) -> bool {
    let (address, bits) = match range.trim().split_once('/') {
        Some((a, b)) => match b.parse::<u32>() {
            Ok(b) => (a, Some(b)),
            Err(_) => return false,
        },
        None => (range.trim(), None),
    };
    let Ok(network) = address.parse::<IpAddr>() else { return false };
    let as_bits = |ip: IpAddr| -> (u128, u32) {
        match ip {
            IpAddr::V4(v4) => (u128::from(u32::from(v4)), 32),
            // An IPv4 client seen through an IPv6 socket (::ffff:a.b.c.d).
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => (u128::from(u32::from(v4)), 32),
                None => (u128::from(v6), 128),
            },
        }
    };
    let ((ip, len), (net, net_len)) = (as_bits(ip), as_bits(network));
    if len != net_len {
        return false;
    }
    let bits = bits.unwrap_or(len).min(len);
    let mask = if bits == 0 { 0 } else { u128::MAX << (len - bits) & (u128::MAX >> (128 - len)) };
    ip & mask == net & mask
}

/// The restrictions in a key's `queryParameters` or a secured key's params.
#[derive(Debug, Default, Clone)]
struct Restrictions {
    filters: Option<String>,
    valid_until: Option<u64>,
    indexes: Option<Vec<String>>,
    sources: Option<Vec<String>>,
    user_token: Option<String>,
    referers: Option<Vec<String>>,
    /// Every other parameter, forced on the search.
    forced: Map<String, Value>,
}

fn string_list(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        Value::String(s) => s.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
        other => vec![other.to_string()],
    }
}

fn parse_restrictions(query: &str) -> Result<Restrictions, String> {
    let mut r = Restrictions::default();
    for (key, value) in params::from_query_string(query) {
        match key.as_str() {
            "filters" => r.filters = Some(value.as_str().map_or_else(|| value.to_string(), str::to_owned)),
            "validUntil" => r.valid_until = Some(value.as_u64().ok_or("validUntil must be a Unix time")?),
            "restrictIndices" => r.indexes = Some(string_list(&value)),
            "restrictSources" => r.sources = Some(string_list(&value)),
            "userToken" => r.user_token = Some(value.as_str().map_or_else(|| value.to_string(), str::to_owned)),
            "referers" => r.referers = Some(string_list(&value)),
            _ => {
                r.forced.insert(key, value);
            }
        }
    }
    Ok(r)
}

/// What the request carries besides the key.
#[derive(Debug, Default, Clone)]
pub struct Caller {
    pub ip: Option<IpAddr>,
    /// `Referer`, or else `Origin`.
    pub referer: Option<String>,
}

impl Caller {
    fn ip_text(&self) -> String {
        self.ip.map_or_else(|| "unknown".into(), |ip| ip.to_string())
    }
}

/// What a request may do, worked out from its key.
#[derive(Debug, Clone, Default)]
pub struct Grant {
    pub admin: bool,
    acl: Vec<String>,
    /// Every list must allow the index (the key's, then the secured key's).
    index_rules: Vec<Vec<String>>,
    /// Search parameters forced on every query, later ones winning.
    forced: Map<String, Value>,
    enforced_filters: Vec<String>,
    max_hits_per_query: Option<usize>,
    /// (requests per hour, who they are counted for)
    pub rate_limit: Option<(u32, String)>,
}

fn forbidden(message: &str) -> ApiError {
    ApiError { status: StatusCode::FORBIDDEN, message: message.into() }
}

impl Grant {
    fn admin() -> Self {
        Grant { admin: true, ..Grant::default() }
    }

    fn search_only() -> Self {
        Grant { acl: vec!["search".into()], ..Grant::default() }
    }

    pub fn allows_index(&self, index: &str) -> bool {
        self.index_rules.iter().all(|rule| rule.is_empty() || rule.iter().any(|p| pattern_matches(p, index)))
    }

    /// Fails unless the key has `acl` and, if given, may use `index`.
    pub fn require(&self, acl: &str, index: Option<&str>) -> Result<(), ApiError> {
        if self.admin {
            return Ok(());
        }
        if !self.acl.iter().any(|a| a == acl) {
            return Err(forbidden("Method not allowed with this API key"));
        }
        match index {
            Some(index) if !self.allows_index(index) => Err(forbidden("Index not allowed with this API key")),
            _ => Ok(()),
        }
    }

    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.admin { Ok(()) } else { Err(forbidden("Method not allowed with this API key")) }
    }

    /// Forces the key's search parameters onto a request's parameters.
    pub fn force_params(&self, params: &mut Map<String, Value>) {
        for (k, v) in &self.forced {
            params.insert(k.clone(), v.clone());
        }
    }

    /// Filters the key adds to every search.
    pub fn enforced_filters(&self) -> &[String] {
        &self.enforced_filters
    }

    pub fn cap_hits_per_page(&self, hits_per_page: usize) -> usize {
        self.max_hits_per_query.map_or(hits_per_page, |max| hits_per_page.min(max))
    }

    /// Whether a forced parameter hides record attributes; such keys may
    /// not read records outside a search.
    pub fn restricts_records(&self) -> bool {
        !self.enforced_filters.is_empty() || self.forced.contains_key("attributesToRetrieve")
    }

    /// Narrows the grant by `restrictions`.
    fn narrow(&mut self, r: Restrictions, rate_subject: &str, caller: &Caller) -> Result<(), ApiError> {
        if r.valid_until.is_some_and(|t| t <= unix_now()) {
            return Err(forbidden("The API key has expired"));
        }
        if let Some(sources) = &r.sources {
            let allowed = caller.ip.is_some_and(|ip| sources.iter().any(|s| ip_in_range(ip, s)));
            if !allowed {
                return Err(forbidden("This API key is not allowed from this IP address"));
            }
        }
        if let Some(referers) = &r.referers {
            check_referer(referers, caller)?;
        }
        if let Some(indexes) = r.indexes {
            self.index_rules.push(indexes);
        }
        self.enforced_filters.extend(r.filters.filter(|f| !f.trim().is_empty()));
        self.forced.extend(r.forced);
        if let (Some((_, who)), Some(token)) = (self.rate_limit.as_mut(), r.user_token) {
            // Count a secured key's users separately rather than by IP.
            *who = format!("{rate_subject}|user:{token}");
        }
        Ok(())
    }
}

fn check_referer(patterns: &[String], caller: &Caller) -> Result<(), ApiError> {
    if patterns.is_empty() {
        return Ok(());
    }
    let ok = caller.referer.as_deref().is_some_and(|r| patterns.iter().any(|p| pattern_matches(p, r)));
    if ok { Ok(()) } else { Err(forbidden("This API key is not allowed from this referer")) }
}

/// The keys created with `/1/keys`, saved in `keys.json` in the data
/// directory.
pub struct KeyStore {
    keys: RwLock<BTreeMap<String, ApiKey>>,
    path: Option<PathBuf>,
    /// Secured key -> the parent that signed it, so a repeated secured key
    /// costs one HMAC instead of one per key. Entries are re-verified on use.
    parents: Mutex<HashMap<String, String>>,
}

/// Secured keys remembered before the cache is emptied.
const MAX_CACHED_PARENTS: usize = 10_000;

impl KeyStore {
    pub fn in_memory() -> Self {
        KeyStore { keys: RwLock::new(BTreeMap::new()), path: None, parents: Mutex::default() }
    }

    pub fn open(path: PathBuf) -> io::Result<Self> {
        let keys = match fs::read(&path) {
            Ok(bytes) => {
                let list: Vec<ApiKey> = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
                list.into_iter().map(|k| (k.value.clone(), k)).collect()
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(KeyStore { keys: RwLock::new(keys), path: Some(path), parents: Mutex::default() })
    }

    pub fn list(&self) -> Vec<ApiKey> {
        self.keys.read().unwrap().values().cloned().collect()
    }

    pub fn get(&self, value: &str) -> Option<ApiKey> {
        self.keys.read().unwrap().get(value).cloned()
    }

    /// Adds or replaces a key and saves.
    pub fn put(&self, key: ApiKey) -> io::Result<()> {
        let mut keys = self.keys.write().unwrap();
        let mut next = keys.clone();
        next.insert(key.value.clone(), key);
        self.save(&next)?;
        *keys = next;
        Ok(())
    }

    pub fn remove(&self, value: &str) -> io::Result<bool> {
        let mut keys = self.keys.write().unwrap();
        if !keys.contains_key(value) {
            return Ok(false);
        }
        let mut next = keys.clone();
        next.remove(value);
        self.save(&next)?;
        *keys = next;
        Ok(true)
    }

    /// Replaces every key (a replica copying its primary's). Returns
    /// whether anything changed.
    pub fn replace_all(&self, list: Vec<ApiKey>) -> io::Result<bool> {
        let next: BTreeMap<String, ApiKey> = list.into_iter().map(|k| (k.value.clone(), k)).collect();
        let mut keys = self.keys.write().unwrap();
        if *keys == next {
            return Ok(false);
        }
        self.save(&next)?;
        *keys = next;
        Ok(true)
    }

    /// Writes the file atomically: a temporary file, synced, renamed.
    fn save(&self, keys: &BTreeMap<String, ApiKey>) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let tmp = path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&keys.values().collect::<Vec<_>>())?)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent() {
            #[cfg(unix)]
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    /// The grant of a stored key, if it is valid for this caller.
    fn grant_of(&self, key: &ApiKey, caller: &Caller) -> Result<Grant, ApiError> {
        if key.validity > 0 && key.created_at.saturating_add(key.validity) <= unix_now() {
            return Err(forbidden("The API key has expired"));
        }
        check_referer(&key.referers, caller)?;
        let mut grant = Grant {
            acl: key.acl.clone(),
            index_rules: vec![key.indexes.clone()],
            max_hits_per_query: (key.max_hits_per_query > 0).then_some(key.max_hits_per_query as usize),
            rate_limit: (key.max_queries_per_ip_per_hour > 0)
                .then(|| (key.max_queries_per_ip_per_hour, format!("{}|ip:{}", key.value, caller.ip_text()))),
            ..Grant::default()
        };
        let restrictions = parse_restrictions(&key.query_parameters).map_err(ApiError::bad_request)?;
        grant.narrow(restrictions, &key.value, caller)?;
        Ok(grant)
    }

    /// Works out what `presented` may do. `None` means the key is unknown.
    pub fn resolve(
        &self,
        presented: &str,
        admin_key: &str,
        search_key: &str,
        caller: &Caller,
    ) -> Option<Result<Grant, ApiError>> {
        if equal(presented, admin_key) {
            return Some(Ok(Grant::admin()));
        }
        if equal(presented, search_key) {
            return Some(Ok(Grant::search_only()));
        }
        if let Some(key) = self.keys.read().unwrap().get(presented)
            && equal(&key.value, presented)
        {
            return Some(self.grant_of(key, caller));
        }
        let (mac, params) = decode_secured_key(presented)?;
        // The parent: a key with the search right whose HMAC matches.
        let keys = self.keys.read().unwrap();
        let candidate = |value: &str| -> Option<(String, Option<&ApiKey>)> {
            let parent = if equal(value, search_key) {
                None
            } else {
                Some(keys.get(value).filter(|k| k.acl.iter().any(|a| a == "search"))?)
            };
            equal(&hmac_hex(value, &params), &mac).then(|| (value.to_owned(), parent))
        };
        let cached = self.parents.lock().unwrap().get(presented).cloned();
        let found = cached.and_then(|value| candidate(&value)).or_else(|| {
            let found = std::iter::once(search_key).chain(keys.keys().map(String::as_str)).find_map(&candidate)?;
            let mut parents = self.parents.lock().unwrap();
            if parents.len() >= MAX_CACHED_PARENTS {
                parents.clear();
            }
            parents.insert(presented.to_owned(), found.0.clone());
            Some(found)
        });
        let (parent_value, parent) = found?;
        let parent_value = parent_value.as_str();
        let result = (|| {
            let mut grant = match parent {
                Some(key) => self.grant_of(key, caller)?,
                None => Grant::search_only(),
            };
            // Secured keys only search, whatever else the parent may do.
            grant.acl.retain(|a| a == "search");
            let restrictions = parse_restrictions(&params).map_err(ApiError::bad_request)?;
            grant.narrow(restrictions, parent_value, caller)?;
            Ok(grant)
        })();
        Some(result)
    }
}

/// Counts requests per hour with a sliding window: the previous hour's
/// count weighted by how much of it is still within the last 60 minutes,
/// plus the current hour's.
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, Window>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Window {
    hour: u64,
    current: u32,
    previous: u32,
}

/// Counters kept before old ones are dropped.
const MAX_TRACKED: usize = 100_000;

impl RateLimiter {
    /// Counts one request for `who` at `now` (Unix seconds). Fails with the
    /// seconds to wait when `who` is already at `limit`.
    pub fn hit(&self, who: &str, limit: u32, now: u64) -> Result<(), u64> {
        self.check(who, limit, now, true)
    }

    /// Like `hit`, without counting the request.
    pub fn peek(&self, who: &str, limit: u32, now: u64) -> Result<(), u64> {
        self.check(who, limit, now, false)
    }

    fn check(&self, who: &str, limit: u32, now: u64, count: bool) -> Result<(), u64> {
        let (hour, into_hour) = (now / 3600, now % 3600);
        let mut windows = self.windows.lock().unwrap();
        if windows.len() >= MAX_TRACKED && !windows.contains_key(who) {
            windows.retain(|_, w| w.hour + 1 >= hour);
        }
        let mut w = windows.get(who).copied().unwrap_or(Window { hour, ..Window::default() });
        if w.hour != hour {
            w.previous = if w.hour + 1 == hour { w.current } else { 0 };
            w.current = 0;
            w.hour = hour;
        }
        let previous_weight = 1.0 - into_hour as f64 / 3600.0;
        let estimate = f64::from(w.previous) * previous_weight + f64::from(w.current);
        if estimate >= f64::from(limit) {
            // Wait until the previous hour's share drops below the margin,
            // or until the next hour if this one alone is over.
            let wait = if w.current >= limit || w.previous == 0 {
                3600 - into_hour
            } else {
                let needed_weight = f64::from(limit - w.current) / f64::from(w.previous);
                let at = ((1.0 - needed_weight) * 3600.0).ceil() as u64;
                at.saturating_sub(into_hour).max(1)
            };
            return Err(wait);
        }
        if count {
            w.current += 1;
            windows.insert(who.to_owned(), w);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str, acl: &[&str]) -> ApiKey {
        ApiKey {
            value: value.into(),
            acl: acl.iter().map(|a| a.to_string()).collect(),
            description: String::new(),
            indexes: Vec::new(),
            max_hits_per_query: 0,
            max_queries_per_ip_per_hour: 0,
            query_parameters: String::new(),
            referers: Vec::new(),
            validity: 0,
            created_at: unix_now(),
        }
    }

    fn resolve(store: &KeyStore, presented: &str, caller: &Caller) -> Option<Result<Grant, ApiError>> {
        store.resolve(presented, "admin", "search", caller)
    }

    #[test]
    fn secured_keys_match_algolias_algorithm() {
        // The format: hex HMAC of the params, then the params, in base64.
        let params = "filters=_tags%3Auser_42";
        let secured = generate_secured_key("YourSearchOnlyApiKey", params);
        let decoded = String::from_utf8(BASE64.decode(&secured).unwrap()).unwrap();
        assert_eq!(&decoded[64..], params);
        assert_eq!(&decoded[..64], hmac_hex("YourSearchOnlyApiKey", params));
        assert_eq!(decode_secured_key(&secured), Some((decoded[..64].to_owned(), params.to_owned())));
        // RFC 4231 test case 2 for HMAC-SHA256.
        assert_eq!(
            hmac_hex("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn secured_keys_restrict_their_parent() {
        let store = KeyStore::in_memory();
        let caller = Caller { ip: Some("10.1.2.3".parse().unwrap()), referer: None };
        let secured = generate_secured_key("search", "filters=owner%3A42&restrictIndices=shop_*&hitsPerPage=5");
        let grant = resolve(&store, &secured, &caller).unwrap().unwrap();
        assert_eq!(grant.enforced_filters(), ["owner:42"]);
        assert!(grant.require("search", Some("shop_fr")).is_ok());
        assert!(grant.require("search", Some("admin_logs")).is_err());
        assert!(grant.require("addObject", None).is_err());
        let mut params = Map::new();
        params.insert("hitsPerPage".into(), Value::from(1000));
        grant.force_params(&mut params);
        assert_eq!(params["hitsPerPage"], 5);

        // A changed restriction no longer matches the HMAC.
        let decoded = String::from_utf8(BASE64.decode(&secured).unwrap()).unwrap();
        let tampered = BASE64.encode(decoded.replace("owner%3A42", "owner%3A43"));
        assert!(resolve(&store, &tampered, &caller).is_none());
        // Signed with a key that is not a search key.
        assert!(resolve(&store, &generate_secured_key("admin", "filters=x%3A1"), &caller).is_none());
        assert!(resolve(&store, &generate_secured_key("nope", "filters=x%3A1"), &caller).is_none());
    }

    #[test]
    fn secured_key_expiry_and_sources() {
        let store = KeyStore::in_memory();
        let caller = Caller { ip: Some("192.168.1.20".parse().unwrap()), referer: None };
        let past = generate_secured_key("search", "validUntil=1000");
        assert_eq!(resolve(&store, &past, &caller).unwrap().unwrap_err().status, StatusCode::FORBIDDEN);
        let future = generate_secured_key("search", &format!("validUntil={}", unix_now() + 60));
        assert!(resolve(&store, &future, &caller).unwrap().is_ok());
        let lan = generate_secured_key("search", "restrictSources=192.168.1.0%2F24");
        assert!(resolve(&store, &lan, &caller).unwrap().is_ok());
        let elsewhere = Caller { ip: Some("10.0.0.1".parse().unwrap()), referer: None };
        assert!(resolve(&store, &lan, &elsewhere).unwrap().is_err());
        assert!(resolve(&store, &lan, &Caller::default()).unwrap().is_err());
    }

    #[test]
    fn stored_keys_have_rights_indexes_and_limits() {
        let store = KeyStore::in_memory();
        let mut writer = key("w1", &["addObject", "search"]);
        writer.indexes = vec!["dev_*".into()];
        writer.max_queries_per_ip_per_hour = 100;
        writer.max_hits_per_query = 10;
        writer.query_parameters = "filters=public%3Atrue".into();
        store.put(writer).unwrap();
        let caller = Caller { ip: Some("1.2.3.4".parse().unwrap()), referer: None };
        let grant = resolve(&store, "w1", &caller).unwrap().unwrap();
        assert!(grant.require("addObject", Some("dev_products")).is_ok());
        assert!(grant.require("addObject", Some("prod_products")).is_err());
        assert!(grant.require("deleteIndex", Some("dev_products")).is_err());
        assert_eq!(grant.cap_hits_per_page(50), 10);
        assert_eq!(grant.enforced_filters(), ["public:true"]);
        assert_eq!(grant.rate_limit, Some((100, "w1|ip:1.2.3.4".into())));

        // A secured key from it keeps the limits, and counts by userToken.
        let secured = generate_secured_key("w1", "userToken=alice&restrictIndices=dev_a");
        let grant = resolve(&store, &secured, &caller).unwrap().unwrap();
        assert_eq!(grant.rate_limit, Some((100, "w1|user:alice".into())));
        assert!(grant.require("search", Some("dev_a")).is_ok());
        assert!(grant.require("search", Some("dev_b")).is_err());
        assert!(grant.require("addObject", Some("dev_a")).is_err());

        let mut expired = key("old", &["search"]);
        expired.validity = 10;
        expired.created_at = unix_now() - 11;
        store.put(expired).unwrap();
        assert!(resolve(&store, "old", &caller).unwrap().is_err());
        assert!(resolve(&store, "unknown", &caller).is_none());
    }

    #[test]
    fn referers_are_checked() {
        let store = KeyStore::in_memory();
        let mut web = key("web", &["search"]);
        web.referers = vec!["https://shop.example.com/*".into()];
        store.put(web).unwrap();
        let from = |r: Option<&str>| Caller { ip: None, referer: r.map(Into::into) };
        assert!(resolve(&store, "web", &from(Some("https://shop.example.com/cart"))).unwrap().is_ok());
        assert!(resolve(&store, "web", &from(Some("https://evil.example.net/"))).unwrap().is_err());
        assert!(resolve(&store, "web", &from(None)).unwrap().is_err());
    }

    #[test]
    fn patterns_and_ip_ranges() {
        assert!(pattern_matches("dev_*", "dev_products"));
        assert!(pattern_matches("*_products", "dev_products"));
        assert!(pattern_matches("a*b*c", "a123b45c"));
        assert!(!pattern_matches("a*b*c", "a123c"));
        assert!(pattern_matches("*", "anything") && pattern_matches("exact", "exact") && !pattern_matches("exact", "exact2"));
        assert!(!pattern_matches("ab*ba", "aba"));
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(ip_in_range(ip("10.1.2.3"), "10.0.0.0/8"));
        assert!(!ip_in_range(ip("11.1.2.3"), "10.0.0.0/8"));
        assert!(ip_in_range(ip("10.1.2.3"), "10.1.2.3"));
        assert!(ip_in_range(ip("::ffff:10.1.2.3"), "10.1.0.0/16"));
        assert!(ip_in_range(ip("2001:db8::1"), "2001:db8::/32"));
        assert!(!ip_in_range(ip("2001:db9::1"), "2001:db8::/32"));
        assert!(ip_in_range(ip("8.8.8.8"), "0.0.0.0/0"));
        assert!(!ip_in_range(ip("8.8.8.8"), "garbage"));
    }

    #[test]
    fn rate_limits_slide_over_the_hour() {
        let limiter = RateLimiter::default();
        let start = 1_000 * 3600; // the start of an hour
        for _ in 0..3 {
            assert!(limiter.hit("k", 3, start + 10).is_ok());
        }
        assert_eq!(limiter.hit("k", 3, start + 10), Err(3590));
        // Others are counted separately.
        assert!(limiter.hit("other", 3, start + 10).is_ok());
        // Half-way through the next hour, half of last hour's 3 still count.
        let next = start + 3600 + 1800;
        assert!(limiter.peek("k", 3, next).is_ok());
        assert!(limiter.hit("k", 3, next).is_ok()); // 1.5 + 0 before it
        assert!(limiter.hit("k", 3, next).is_ok()); // 1.5 + 1
        assert!(limiter.hit("k", 3, next).is_err()); // 1.5 + 2 >= 3
        // Two hours later everything has expired.
        assert!(limiter.hit("k", 3, start + 3 * 3600).is_ok());
    }
}
