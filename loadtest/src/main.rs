//! HTTP load generator for the search server.
//!
//! Usage: loadtest --url http://127.0.0.1:8080 --index NAME --key SEARCH_KEY
//!                 --queries QUERIES.jsonl [--concurrency 1,4,16,64] [--seconds 10]
//!        loadtest --write --url ... --index NAME --key ADMIN_KEY [--concurrency ...]
//!
//! `--write` sends single-record writes (PUT /1/indexes/NAME/ID) instead of
//! searches, to measure write throughput.
//!
//! For each concurrency level, that many workers send search requests in a
//! closed loop (each sends its next request when the previous one is
//! answered) for the given time, cycling through the query set over
//! kept-alive connections. Reports throughput, latency percentiles, the
//! server's own `processingTimeMS`, and errors.

use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Args {
    url: String,
    index: String,
    key: String,
    queries: String,
    concurrency: Vec<usize>,
    seconds: u64,
    write: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        url: "http://127.0.0.1:8080".into(),
        index: String::new(),
        key: String::new(),
        queries: String::new(),
        concurrency: vec![1, 4, 16, 64],
        seconds: 10,
        write: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--url" => args.url = value()?.trim_end_matches('/').to_owned(),
            "--index" => args.index = value()?,
            "--key" => args.key = value()?,
            "--queries" => args.queries = value()?,
            "--concurrency" => {
                args.concurrency =
                    value()?.split(',').map(|c| c.trim().parse().map_err(|_| "bad --concurrency")).collect::<Result<_, _>>()?
            }
            "--seconds" => args.seconds = value()?.parse().map_err(|_| "bad --seconds")?,
            "--write" => args.write = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.index.is_empty() || args.key.is_empty() || (args.queries.is_empty() && !args.write) {
        return Err("--index and --key are required, and --queries unless --write".into());
    }
    Ok(args)
}

/// What one worker measured.
#[derive(Default)]
struct Sample {
    latencies_us: Vec<u64>,
    server_ms: Vec<u64>,
    errors: u64,
    first_error: Option<String>,
}

/// What a worker sends: searches from the query set, or record writes.
#[derive(Clone)]
enum Work {
    Search { url: String, bodies: Arc<Vec<String>> },
    Write { base: String, worker: usize },
}

async fn worker(client: reqwest::Client, work: Work, key: String, start: usize, until: Instant) -> Sample {
    let mut sample = Sample::default();
    let mut i = start;
    while Instant::now() < until {
        let request = match &work {
            Work::Search { url, bodies } => client.post(url).body(bodies[i % bodies.len()].clone()),
            Work::Write { base, worker } => {
                let body = format!(r#"{{"name":"Load test record {i}","rating":{}}}"#, i % 5);
                client.put(format!("{base}/w{worker}-{i}")).body(body)
            }
        };
        i += 1;
        let sent = Instant::now();
        let result = request.header("X-Algolia-API-Key", &key).send().await;
        let outcome = match result {
            Ok(response) if response.status().is_success() => response.json::<Value>().await.map_err(|e| e.to_string()),
            Ok(response) => Err(format!("HTTP {}", response.status())),
            Err(e) => Err(e.to_string()),
        };
        match outcome {
            Ok(json) => {
                sample.latencies_us.push(sent.elapsed().as_micros() as u64);
                if let Some(ms) = json["processingTimeMS"].as_u64() {
                    sample.server_ms.push(ms);
                }
            }
            Err(e) => {
                sample.errors += 1;
                sample.first_error.get_or_insert(e);
            }
        }
    }
    sample
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    let bodies: Arc<Vec<String>> = if args.write {
        Arc::new(Vec::new())
    } else {
        Arc::new(std::fs::read_to_string(&args.queries)?.lines().filter(|l| !l.trim().is_empty()).map(str::to_owned).collect())
    };
    let url = format!("{}/1/indexes/{}/query", args.url, args.index);
    if args.write {
        println!("single-record writes, {} s per level, PUT {}/1/indexes/{}/ID", args.seconds, args.url, args.index);
    } else {
        println!("{} queries from {}, {} s per level, POST {url}", bodies.len(), args.queries, args.seconds);
    }
    println!("{:>6} {:>9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>7}", "conc", "req/s", "p50 ms", "p90 ms", "p99 ms", "max ms", "server p50", "errors");
    for &concurrency in &args.concurrency {
        let client = reqwest::Client::builder()
            .no_proxy()
            .pool_max_idle_per_host(concurrency)
            .timeout(Duration::from_secs(30))
            .build()?;
        let until = Instant::now() + Duration::from_secs(args.seconds);
        let started = Instant::now();
        let workers: Vec<_> = (0..concurrency)
            .map(|w| {
                let (work, start) = if args.write {
                    (Work::Write { base: format!("{}/1/indexes/{}", args.url, args.index), worker: w }, 0)
                } else {
                    (Work::Search { url: url.clone(), bodies: bodies.clone() }, w * bodies.len() / concurrency.max(1))
                };
                tokio::spawn(worker(client.clone(), work, args.key.clone(), start, until))
            })
            .collect();
        let mut all = Sample::default();
        for w in workers {
            let s = w.await?;
            all.latencies_us.extend(s.latencies_us);
            all.server_ms.extend(s.server_ms);
            all.errors += s.errors;
            if all.first_error.is_none() {
                all.first_error = s.first_error;
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        all.latencies_us.sort_unstable();
        all.server_ms.sort_unstable();
        let ms = |us: u64| us as f64 / 1000.0;
        println!(
            "{:>6} {:>9.0} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>10} {:>7}",
            concurrency,
            all.latencies_us.len() as f64 / elapsed,
            ms(percentile(&all.latencies_us, 0.5)),
            ms(percentile(&all.latencies_us, 0.9)),
            ms(percentile(&all.latencies_us, 0.99)),
            ms(*all.latencies_us.last().unwrap_or(&0)),
            percentile(&all.server_ms, 0.5),
            all.errors
        );
        if let Some(e) = all.first_error {
            println!("       first error: {e}");
        }
    }
    Ok(())
}
