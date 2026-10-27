//! Starts the search server.
//!
//! Usage: server [--host 127.0.0.1] [--port 8080] [--data-dir DIR | --in-memory]
//!               [--load NAME=records.jsonl[,settings.json]]...
//!               [--replica-of PRIMARY_URL] [--trust-proxy]
//!               [--cluster ID=URL,ID=URL,... --node-id ID | --join --node-id ID]
//!        server secured-key PARENT_KEY 'filters=...&validUntil=...'
//!
//! Data is kept in DIR (default `./search-data`) and reloaded at startup.
//! `--load` replaces that index with the file's records.
//!
//! Keys come from SEARCH_ADMIN_KEY and SEARCH_SEARCH_KEY. A missing key is
//! generated and printed once at startup.
//!
//! With `--replica-of`, the server copies every index from the primary,
//! follows its changes and refuses client writes. It authenticates with the
//! primary's admin key from SEARCH_PRIMARY_ADMIN_KEY (default: its own
//! admin key).
//!
//! With `--cluster`, the server is one node of a Raft cluster (see
//! `server::cluster`): every node has every index and serves searches,
//! writes are agreed on by a majority, and a new leader is elected when the
//! leader fails. Each node is started with the same member list, its own
//! `--node-id` and its own `--data-dir` (the Raft log and snapshots go in
//! `DIR/raft`); all share SEARCH_ADMIN_KEY. `--load` submits the records
//! through the cluster once it has a leader. The member list only starts a
//! new cluster: after that the membership lives in the Raft log and
//! snapshots, and changes with `POST /1/cluster/members` and
//! `DELETE /1/cluster/members/ID`. To add a node, start it with `--join`
//! (and its `--node-id` and `--data-dir`, no member list): it waits until
//! the cluster adds it, then receives all data from the leader.
//!
//! `--trust-proxy` takes client addresses (for rate limits and
//! `restrictSources`) from `X-Forwarded-For`; use it only behind a proxy
//! that sets that header. `secured-key` prints a secured API key, as
//! Algolia's `generateSecuredApiKey` would.

use engine::index::{Index, Settings};
use rand::RngCore;
use server::cluster::{Cluster, ClusterConfig};
use server::replication::run_replica;
use server::state::{Command, Op};
use server::{AppState, app};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

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
    let mut data_dir = Some(PathBuf::from("search-data"));
    let mut replica_of: Option<String> = None;
    let mut trust_proxy = false;
    let mut cluster: Option<String> = None;
    let mut node_id: Option<String> = None;
    let mut join = false;
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) == Some("secured-key") {
        let (Some(parent), Some(params)) = (args.nth(1), args.next()) else {
            return Err("usage: server secured-key PARENT_KEY 'filters=...&validUntil=...'".into());
        };
        println!("{}", server::keys::generate_secured_key(&parent, &params));
        return Ok(());
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => host = args.next().ok_or("--host needs a value")?,
            "--port" => port = args.next().ok_or("--port needs a value")?.parse()?,
            "--load" => loads.push(args.next().ok_or("--load needs a value")?),
            "--data-dir" => data_dir = Some(args.next().ok_or("--data-dir needs a value")?.into()),
            "--in-memory" => data_dir = None,
            "--replica-of" => replica_of = Some(args.next().ok_or("--replica-of needs the primary's URL")?),
            "--trust-proxy" => trust_proxy = true,
            "--cluster" => cluster = Some(args.next().ok_or("--cluster needs ID=URL,ID=URL,...")?),
            "--node-id" => node_id = Some(args.next().ok_or("--node-id needs a value")?),
            "--join" => join = true,
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

    if join && cluster.is_some() {
        return Err("--join waits to be added to a running cluster; --cluster starts a new one".into());
    }
    if cluster.is_some() || join {
        let members = match &cluster {
            Some(spec) => ClusterConfig::parse_members(spec)?,
            None => Vec::new(),
        };
        let id = node_id.ok_or("a cluster node needs --node-id")?;
        let dir = data_dir.ok_or("a cluster node needs a --data-dir")?.join("raft");
        if replica_of.is_some() {
            return Err("--cluster and --replica-of cannot be combined".into());
        }
        let state = Arc::new(AppState::new(admin_key.clone(), search_key).trusting_proxy(trust_proxy));
        let start = Instant::now();
        let config = ClusterConfig::new(id.clone(), members, dir, admin_key);
        let node = Cluster::start(state.clone(), config)?;
        println!(
            "cluster node {id}: {} index(es) loaded in {:.0} ms{}",
            state.index_names().len(),
            start.elapsed().as_secs_f64() * 1000.0,
            if join && node.status().members.voters.is_empty() { "; waiting to be added to a cluster" } else { "" }
        );
        let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
        println!("listening on http://{}", listener.local_addr()?);
        if !loads.is_empty() {
            tokio::spawn(load_into_cluster(node, loads));
        }
        let service = app(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
        axum::serve(listener, service).with_graceful_shutdown(shutdown_signal()).await?;
        return Ok(());
    }

    let state = match &data_dir {
        Some(dir) => {
            let start = Instant::now();
            let state = AppState::open(admin_key.clone(), search_key, dir)?;
            let names = state.index_names();
            println!(
                "data directory {}: {} index(es) loaded in {:.0} ms {:?}",
                dir.display(),
                names.len(),
                start.elapsed().as_secs_f64() * 1000.0,
                names
            );
            state
        }
        None => {
            println!("in-memory mode: data is lost when the server stops");
            AppState::new(admin_key.clone(), search_key)
        }
    };
    if replica_of.is_some() && !loads.is_empty() {
        return Err("a replica copies its data from the primary; --load is not allowed".into());
    }
    let primary_key = std::env::var("SEARCH_PRIMARY_ADMIN_KEY").unwrap_or_else(|_| admin_key.clone());
    let state = match &replica_of {
        Some(url) => {
            println!("read replica of {url}");
            state.as_replica_of(url.as_str())
        }
        None => state,
    };
    let state = Arc::new(state.trusting_proxy(trust_proxy));
    if replica_of.is_some() {
        tokio::spawn(run_replica(state.clone(), primary_key));
    }
    for spec in &loads {
        let (name, index) = load(spec)?;
        state.insert_index(&name, index)?;
    }

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    println!("listening on http://{}", listener.local_addr()?);
    let service = app(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, service).with_graceful_shutdown(shutdown_signal()).await?;
    if data_dir.is_some() {
        let start = Instant::now();
        state.checkpoint()?;
        println!("checkpointed in {:.0} ms", start.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(())
}

/// Submits `--load` files through the cluster: the settings, then the
/// records in batches of 1,000.
async fn load_into_cluster(cluster: Arc<Cluster>, specs: Vec<String>) {
    for spec in specs {
        let result: Result<usize, Box<dyn std::error::Error>> = async {
            let (name, files) = spec.split_once('=').ok_or("--load expects NAME=records.jsonl[,settings.json]")?;
            let (records, settings) = match files.split_once(',') {
                Some((r, s)) => (r, Some(s)),
                None => (files, None),
            };
            let settings: Settings = match settings {
                Some(path) => serde_json::from_reader(BufReader::new(File::open(path)?))?,
                None => Settings::default(),
            };
            let write = |ops| Command::Write { index: name.to_owned(), create: true, ops };
            cluster.submit(&write(vec![Op::Clear, Op::Settings(settings)])).await.map_err(|e| e.message)?;
            let mut batch = Vec::new();
            let mut count = 0;
            for line in BufRead::lines(BufReader::new(File::open(records)?)) {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let serde_json::Value::Object(record) = serde_json::from_str(&line)? else {
                    return Err("records must be JSON objects".into());
                };
                let id = match record.get("objectID") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Number(n)) => n.to_string(),
                    _ => return Err("every record needs an objectID".into()),
                };
                batch.push(Op::Put(id, record));
                count += 1;
                if batch.len() == 1000 {
                    cluster.submit(&write(std::mem::take(&mut batch))).await.map_err(|e| e.message)?;
                }
            }
            if !batch.is_empty() {
                cluster.submit(&write(batch)).await.map_err(|e| e.message)?;
            }
            Ok(count)
        }
        .await;
        match result {
            Ok(count) => println!("loaded {count} records through the cluster ({spec})"),
            Err(e) => eprintln!("--load {spec}: {e}"),
        }
    }
}

/// Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("Ctrl-C handler") };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler").recv().await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("shutting down");
}
