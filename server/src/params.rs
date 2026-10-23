//! Search parameters as Algolia clients send them: JSON fields, a URL-encoded
//! `params` string (`query=x&hitsPerPage=5&facets=["brand"]`), or a GET
//! query string. All forms end up as one JSON object read into
//! `SearchParams`.

use crate::ApiError;
use engine::search::SearchParams;
use serde_json::{Map, Value};

/// Largest page Algolia allows.
pub const MAX_HITS_PER_PAGE: usize = 1000;

/// Parameters that shape the HTTP response rather than the search.
#[derive(Debug, Default)]
pub struct ResponseOptions {
    pub get_ranking_info: bool,
    /// `None` returns every attribute.
    pub attributes_to_retrieve: Option<Vec<String>>,
}

const STRING_KEYS: &[&str] = &["query", "filters", "highlightPreTag", "highlightPostTag", "snippetEllipsisText"];
const LIST_KEYS: &[&str] =
    &["facets", "optionalFilters", "attributesToHighlight", "attributesToSnippet", "attributesToRetrieve"];

/// Converts one URL-encoded value to the JSON type its key expects.
fn convert(key: &str, raw: &str) -> Value {
    if STRING_KEYS.contains(&key) {
        return Value::String(raw.to_owned());
    }
    if LIST_KEYS.contains(&key) {
        if raw.trim_start().starts_with('[')
            && let Ok(list) = serde_json::from_str(raw)
        {
            return list;
        }
        // Algolia also accepts comma-separated lists.
        return raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(Value::from).collect();
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// Parses `key=value&...` into a JSON object with typed values.
pub fn from_query_string(query: &str) -> Map<String, Value> {
    form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.to_string(), convert(&k, &v))).collect()
}

/// Reads a request body: top-level fields win over the `params` string.
pub fn from_body(body: &Value) -> Result<Map<String, Value>, ApiError> {
    let object = match body {
        Value::Object(o) => o,
        Value::Null => return Ok(Map::new()),
        _ => return Err(ApiError::bad_request("request body must be a JSON object")),
    };
    let mut merged = match object.get("params") {
        Some(Value::String(s)) => from_query_string(s),
        Some(_) => return Err(ApiError::bad_request("params must be a string")),
        None => Map::new(),
    };
    for (k, v) in object {
        if k != "params" && k != "indexName" {
            merged.insert(k.clone(), v.clone());
        }
    }
    Ok(merged)
}

/// Splits the merged parameters into engine parameters and response options.
pub fn into_search_params(mut map: Map<String, Value>) -> Result<(SearchParams, ResponseOptions), ApiError> {
    let get_ranking_info = match map.remove("getRankingInfo") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => b,
        Some(Value::Number(n)) => n.as_u64() == Some(1),
        Some(Value::String(s)) => s == "true" || s == "1",
        Some(_) => return Err(ApiError::bad_request("getRankingInfo must be a boolean")),
    };
    let attributes_to_retrieve = match map.remove("attributesToRetrieve") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let list: Vec<String> = serde_json::from_value(v)
                .map_err(|_| ApiError::bad_request("attributesToRetrieve must be a list of strings"))?;
            (!list.iter().any(|a| a == "*")).then_some(list)
        }
    };
    if let Some(Value::Array(items)) = map.get("optionalFilters")
        && items.iter().any(|i| !i.is_string())
    {
        return Err(ApiError::bad_request("optionalFilters groups (nested lists) are not supported yet"));
    }
    let params: SearchParams = serde_json::from_value(Value::Object(map))
        .map_err(|e| ApiError::bad_request(format!("invalid search parameters: {e}")))?;
    if params.hits_per_page > MAX_HITS_PER_PAGE {
        return Err(ApiError::bad_request(format!("hitsPerPage must be at most {MAX_HITS_PER_PAGE}")));
    }
    Ok((params, ResponseOptions { get_ranking_info, attributes_to_retrieve }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn params_string_is_typed_by_key() {
        let map = from_query_string("query=1996&hitsPerPage=5&facets=%5B%22brand%22%5D&attributesToSnippet=body:5,title&getRankingInfo=true");
        assert_eq!(map["query"], "1996");
        assert_eq!(map["hitsPerPage"], 5);
        assert_eq!(map["facets"], json!(["brand"]));
        assert_eq!(map["attributesToSnippet"], json!(["body:5", "title"]));
        assert_eq!(map["getRankingInfo"], true);
    }

    #[test]
    fn body_fields_override_the_params_string() {
        let body = json!({"params": "query=a&hitsPerPage=3", "query": "b", "indexName": "x"});
        let (params, _) = into_search_params(from_body(&body).unwrap()).unwrap();
        assert_eq!((params.query.as_str(), params.hits_per_page), ("b", 3));
    }

    #[test]
    fn response_options_are_split_out() {
        let map = from_query_string("query=x&getRankingInfo=1&attributesToRetrieve=name,price");
        let (params, options) = into_search_params(map).unwrap();
        assert_eq!(params.query, "x");
        assert!(options.get_ranking_info);
        assert_eq!(options.attributes_to_retrieve.unwrap(), ["name", "price"]);
        let (_, options) = into_search_params(from_query_string("attributesToRetrieve=*")).unwrap();
        assert!(options.attributes_to_retrieve.is_none());
    }

    #[test]
    fn rejects_bad_input() {
        let err = |v: Value| into_search_params(from_body(&v).unwrap()).unwrap_err().message;
        assert_eq!(err(json!({"hitsPerPage": 5000})), "hitsPerPage must be at most 1000");
        assert!(err(json!({"page": "two"})).starts_with("invalid search parameters"));
        assert!(err(json!({"optionalFilters": [["a:b", "c:d"]]})).contains("not supported"));
        assert!(from_body(&json!([1])).is_err());
    }
}
