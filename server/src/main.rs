//! Starts the search server.
//!
//! Usage: server [--host 127.0.0.1] [--port 8080]
//!               [--load NAME=records.jsonl[,settings.json]]...
//!
//! Keys come from SEARCH_ADMIN_KEY and SEARCH_SEARCH_KEY. A missing key is
//! generated and printed once at startup.

use engine::index::{Index, Settings};
use rand::RngCore;
use server::{AppState, app};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

fn random_key() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn key_from_env(name: &str) -> (String, bool) {
    match std::env::var(name) {
        Ok(key) if !key.is_empty() => (key, false),
        _ => (random_key(), true),
    }
}

/// `NAME=records.jsonl[,settings.json]`
fn load(spec: &str) -> Result<(String, Index), Box<dyn std::error::Error>> {
    let (name, files) = spec.split_once('=').ok_or("--load expects NAME=records.jsonl[,settings.json]")?;
    let (records, settings) = match files.split_once(',') {
        Some((r, s)) => (r, Some(s)),
        None => (files, None),
    };
    let settings: Settings = match settings {
        Some(path) => serde_json::from_reader(BufReader::new(File::open(path)?)).map_err(|e| format!("{path}: {e}"))?,
        None => Settings::default(),
    };
    let mut index = Index::new(settings);
    let count = index.load_jsonl(BufReader::new(File::open(records)?))?;
    println!("loaded {count} records into {name}");
    Ok((name.to_owned(), index))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut host = "127.0.0.1".to_owned();
    let mut port = 8080u16;
    let mut loads = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => host = args.next().ok_or("--host needs a value")?,
            "--port" => port = args.next().ok_or("--port needs a value")?.parse()?,
            "--load" => loads.push(args.next().ok_or("--load needs a value")?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }

    let (admin_key, admin_generated) = key_from_env("SEARCH_ADMIN_KEY");
    let (search_key, search_generated) = key_from_env("SEARCH_SEARCH_KEY");
    if admin_key == search_key {
        return Err("SEARCH_ADMIN_KEY and SEARCH_SEARCH_KEY must differ".into());
    }
    if admin_generated {
        println!("admin key:  {admin_key}  (set SEARCH_ADMIN_KEY to keep it)");
    }
    if search_generated {
        println!("search key: {search_key}  (set SEARCH_SEARCH_KEY to keep it)");
    }

    let state = Arc::new(AppState::new(admin_key, search_key));
    for spec in &loads {
        let (name, index) = load(spec)?;
        state.insert_index(&name, index);
    }

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    println!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
