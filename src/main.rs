use std::time::Duration;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::env;
use std::collections::HashMap;

use chrono::{Datelike, Local};
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE};
use scraper::{Html, Selector};
use tokio::time::sleep;

const BASE_TEAMS_URL: &str = "https://polypwn.polycyber.io/teams";
const INITIAL_TEAM_SEARCH_BOUND: u32 = 100;
const BATCH_SIZE: usize = 10;
const DELAY_BETWEEN_BATCHES_MS: u64 = 1500;

#[derive(Debug)]
struct UserScore {
    name: String,
    id: u32,
    team_id: u32,
    team_name: String,
    points: u32,
}

#[derive(Debug, Clone)]
struct TeamMember {
    id: u32,
    name: String,
    points: u32,
}

#[derive(Debug, Clone)]
struct TeamScore {
    id: u32,
    name: String,
    points: u32,
    member_count: usize,
    members: Vec<TeamMember>,
}

fn escape_csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

async fn fetch_team(client: &Client, id: u32) -> Option<TeamScore> {
    let url = format!("{}/{}", BASE_TEAMS_URL, id);

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Request error for team {id}: {e}");
            return None;
        }
    };

    if !resp.status().is_success() {
        eprintln!("Team {id}: HTTP {}", resp.status());
        return None;
    }

    let html = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Team {id}: error while reading response body: {e}");
            return None;
        }
    };

    parse_team(id, &html)
}

async fn team_exists(client: &Client, id: u32) -> bool {
    let url = format!("{}/{}", BASE_TEAMS_URL, id);
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            eprintln!("Request error while checking team {id}: {e}");
            false
        }
    }
}

async fn find_last_registered_team_id(client: &Client) -> u32 {
    let mut low = 0;
    let mut high = INITIAL_TEAM_SEARCH_BOUND;

    while team_exists(client, high).await {
        low = high;
        high = high.saturating_mul(2);

        if high == low {
            break;
        }

        println!("Team #{low} exists, expanding search bound to {high}...");
    }

    while low < high {
        let mid = low + (high - low + 1) / 2;

        if mid > 0 && team_exists(client, mid).await {
            low = mid;
        } else {
            high = mid - 1;
        }
    }

    low
}

fn parse_team(id: u32, html: &str) -> Option<TeamScore> {
    let doc = Html::parse_document(html);

    let team_name_sel = Selector::parse("h1#team-id").unwrap();
    let team_name = doc
        .select(&team_name_sel)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("Team#{id}"));

    let row_sel = Selector::parse("table.table tbody tr").unwrap();
    let user_link_sel = Selector::parse("td:nth-child(1) a[href^='/users/']").unwrap();
    let score_sel = Selector::parse("td:nth-child(2)").unwrap();

    let mut points = 0u32;
    let mut member_count = 0usize;
    let mut members: Vec<TeamMember> = Vec::new();

    for row in doc.select(&row_sel) {
        let Some(user_link) = row.select(&user_link_sel).next() else {
            continue;
        };

        let user_id = user_link
            .value()
            .attr("href")
            .and_then(|href| href.rsplit('/').next())
            .and_then(|id_str| id_str.parse::<u32>().ok());

        let user_name = user_link.text().collect::<String>().trim().to_string();

        let member_points = row
            .select(&score_sel)
            .next()
            .and_then(|el| el.text().collect::<String>().trim().parse::<u32>().ok());

        if let (Some(uid), Some(value)) = (user_id, member_points) {
            points += value;
            member_count += 1;
            members.push(TeamMember {
                id: uid,
                name: user_name,
                points: value,
            });
        }
    }

    Some(TeamScore {
        id,
        name: team_name,
        points,
        member_count,
        members,
    })
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

    println!("Detecting last registered team id...");
    let total_teams = find_last_registered_team_id(&client).await;
    if total_teams == 0 {
        anyhow::bail!("Could not find any registered teams.");
    }
    println!("Detected last registered team id: {total_teams}");

    let team_ids: Vec<u32> = (1..=total_teams).collect();
    let batches: Vec<&[u32]> = team_ids.chunks(BATCH_SIZE).collect();

    let mut teams: Vec<TeamScore> = Vec::new();
    let total_batches = batches.len();

    for (i, batch) in batches.iter().enumerate() {
        println!("Batch {}/{} — teams {:?}...", i + 1, total_batches, &batch[..1]);

        let futures: Vec<_> = batch
            .iter()
            .map(|&id| fetch_team(&client, id))
            .collect();

        let batch_results = futures::future::join_all(futures).await;

        for team in batch_results.into_iter().flatten() {
            println!(
                "  ✓ team #{} {} — {} pts ({} members)",
                team.id, team.name, team.points, team.member_count
            );
            teams.push(team);
        }

        if i + 1 < total_batches {
            sleep(Duration::from_millis(DELAY_BETWEEN_BATCHES_MS)).await;
        }
    }

    teams.sort_by(|a, b| b.points.cmp(&a.points).then(a.id.cmp(&b.id)));

    let team_rank_map: HashMap<u32, usize> = teams
        .iter()
        .enumerate()
        .map(|(index, team)| (team.id, index + 1))
        .collect();
    let team_points_map: HashMap<u32, u32> = teams
        .iter()
        .map(|team| (team.id, team.points))
        .collect();

    let mut users_by_id: HashMap<u32, UserScore> = HashMap::new();
    for team in &teams {
        for member in &team.members {
            users_by_id.entry(member.id).or_insert_with(|| UserScore {
                name: member.name.clone(),
                id: member.id,
                team_id: team.id,
                team_name: team.name.clone(),
                points: member.points,
            });
        }
    }

    let mut results: Vec<UserScore> = users_by_id.into_values().collect();
    results.sort_by(|a, b| b.points.cmp(&a.points).then(a.id.cmp(&b.id)));

    let current_year = Local::now().year();
    let output_file = format!("polypwn_rankings_{current_year}.csv");
    let file = File::create(&output_file)?;
    let mut writer = BufWriter::new(file);

    writeln!(writer, "rank,user_id,name,team_id,team_name,team_rank,team_points,points")?;
    for (rank, user) in results.iter().enumerate() {
        let team_rank_csv = team_rank_map
            .get(&user.team_id)
            .copied()
            .map(|rank| rank.to_string())
            .unwrap_or_default();
        let team_points_csv = team_points_map
            .get(&user.team_id)
            .copied()
            .map(|points| points.to_string())
            .unwrap_or_default();

        writeln!(
            writer,
            "{},{},{},{},{},{},{},{}",
            rank + 1,
            user.id,
            escape_csv_field(&user.name),
            user.team_id,
            escape_csv_field(&user.team_name),
            team_rank_csv,
            team_points_csv,
            user.points
        )?;
    }

    println!(
        "\n✅ Ranking exported to '{}' ({} participants from {} teams)",
        output_file,
        results.len(),
        teams.len()
    );
    Ok(())
}
