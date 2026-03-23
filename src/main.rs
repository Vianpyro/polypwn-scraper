use std::time::Duration;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::env;

use chrono::{Datelike, Local};
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE};
use scraper::{Html, Selector};
use tokio::time::sleep;

const BASE_URL: &str = "https://polypwn.polycyber.io/users";
const TOTAL_USERS: u32 = 223;
const BATCH_SIZE: usize = 10;
const DELAY_BETWEEN_BATCHES_MS: u64 = 1500;

#[derive(Debug)]
struct UserScore {
    name: String,
    id: u32,
    points: u32,
}

async fn fetch_user(client: &Client, id: u32) -> Option<UserScore> {
    let url = format!("{}/{}", BASE_URL, id);

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Request error for user {id}: {e}");
            return None;
        }
    };

    if !resp.status().is_success() {
        eprintln!("User {id}: HTTP {}", resp.status());
        return None;
    }

    let html = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("User {id}: error while reading response body: {e}");
            return None;
        }
    };

    parse_user(id, &html)
}

fn parse_user(id: u32, html: &str) -> Option<UserScore> {
    let doc = Html::parse_document(html);

    let h1_sel = Selector::parse("div.jumbotron h1").unwrap();
    let name = doc
        .select(&h1_sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .unwrap_or_else(|| format!("User#{id}"));

    let td_sel = Selector::parse("table.table tbody tr td:nth-child(3)").unwrap();

    let points: u32 = doc
        .select(&td_sel)
        .filter_map(|el| {
            el.text()
                .collect::<String>()
                .trim()
                .parse::<u32>()
                .ok()
        })
        .sum();

    Some(UserScore { id, name, points })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let cookie = args
        .windows(2)
        .find(|w| w[0] == "--cookie")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| {
            println!("No --cookie argument provided.");
            println!("Usage: cargo run --release -- --cookie \"session=VALUE; other=VALUE\"");
            print!("Please enter your cookie value: ");
            std::io::stdout().flush().expect("failed to flush stdout");

            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .expect("failed to read cookie from stdin");

            input.trim().to_string()
        });

    if cookie.is_empty() {
        anyhow::bail!("No cookie provided. Aborting.");
    }

    let mut headers = HeaderMap::new();
    headers.insert(COOKIE, HeaderValue::from_str(&cookie)?);

    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("PolyPwn-Scraper/1.0 (CTF ranking tool)")
        .default_headers(headers)
        .build()?;

    let ids: Vec<u32> = (1..=TOTAL_USERS).collect();
    let batches: Vec<&[u32]> = ids.chunks(BATCH_SIZE).collect();

    let mut results: Vec<UserScore> = Vec::new();
    let total_batches = batches.len();

    for (i, batch) in batches.iter().enumerate() {
        println!("Batch {}/{} — users {:?}...", i + 1, total_batches, &batch[..1]);

        let futures: Vec<_> = batch
            .iter()
            .map(|&id| fetch_user(&client, id))
            .collect();

        let batch_results = futures::future::join_all(futures).await;

        for res in batch_results.into_iter().flatten() {
            println!("  ✓ #{} {} — {} pts", res.id, res.name, res.points);
            results.push(res);
        }

        if i + 1 < total_batches {
            sleep(Duration::from_millis(DELAY_BETWEEN_BATCHES_MS)).await;
        }
    }

    results.sort_by(|a, b| b.points.cmp(&a.points).then(a.id.cmp(&b.id)));

    let current_year = Local::now().year();
    let output_file = format!("polypwn_rankings_{current_year}.csv");
    let file = File::create(&output_file)?;
    let mut writer = BufWriter::new(file);

    writeln!(writer, "rank,user_id,name,points")?;
    for (rank, user) in results.iter().enumerate() {
        writeln!(
            writer,
            "{},{},{},{}",
            rank + 1,
            user.id,
            if user.name.contains(',') || user.name.contains('"') {
                format!("\"{}\"", user.name.replace('"', "\"\""))
            } else {
                user.name.clone()
            },
            user.points
        )?;
    }

    println!("\n✅ Ranking exported to '{}' ({} participants)", output_file, results.len());
    Ok(())
}
