use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use reqwest::Client;
use rss::Channel;
use serde_json::Value;
use std::collections::HashMap;
use std::env;

#[derive(Clone)]
struct ReviewEntry {
    friend_name: String,
    rating_raw: String,
}

struct MovieGroup {
    general_link: String,
    reviews: Vec<ReviewEntry>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::new();

    let sheet_url = env::var("SHEET_CSV_URL").context("Missing SHEET_CSV_URL env var")?;
    let whapi_token = env::var("WHAPI_TOKEN").context("Missing WHAPI_TOKEN env var")?;
    let group_id = env::var("GROUP_ID").context("Missing GROUP_ID env var")?;

    let movie_map = get_movie_map(&client, &sheet_url).await.unwrap_or_default();

    let message = create_message(movie_map).await;

    let message_id = send_whatsapp(&client, &message, &whapi_token, &group_id).await?;

    pin_message(&client, &message_id, &whapi_token).await?;

    set_presence_offline(&client, &whapi_token).await?;

    Ok(())
}

// --- HELPER FUNCTIONS ---

fn calculate_score(raw: &str) -> f32 {
    let full_stars = raw.chars().filter(|&c| c == '★').count() as f32;
    let half_star = if raw.contains('½') { 0.5 } else { 0.0 };
    full_stars + half_star
}

fn get_reaction_emoji(score: f32) -> &'static str {
    if score == 5.0 {
        "🤩"
    } else if score >= 4.0 {
        "🔥"
    } else if score >= 3.0 {
        "🙂"
    } else if score >= 2.0 {
        "😐"
    } else if score > 0.0 {
        "🤮"
    } else {
        "🤔"
    }
}

async fn fetch_and_parse_feed(client: &Client, url: &str) -> Result<Channel> {
    let content = client.get(url).send().await?.bytes().await?;
    let channel = Channel::read_from(&content[..])?;
    Ok(channel)
}

async fn create_message(movie_map: HashMap<String, MovieGroup>) -> String {
    if movie_map.is_empty() {
        return "No movies watched this week 😱".to_string();
    }

    let mut weekly_summary = String::from("*🍿 Weekly Movie Round-up 🍿*\n\n");
    let mut sorted_movies: Vec<_> = movie_map.keys().collect();
    sorted_movies.sort();

    for movie in sorted_movies {
        let group = &movie_map[movie];

        // Title and link
        weekly_summary.push_str(&format!("🎬 *{}*\n{}\n", movie, group.general_link));

        // The reviews
        for review in &group.reviews {
            if review.rating_raw.is_empty() {
                // No rating = just watched
                weekly_summary.push_str(&format!("• *{}* watched 🍿\n", review.friend_name));
            } else {
                // Has rating - Calculate score and get emoji
                let score = calculate_score(&review.rating_raw);
                let emoji = get_reaction_emoji(score);

                weekly_summary.push_str(&format!(
                    "• *{}* rated ({}) {}\n",
                    review.friend_name, review.rating_raw, emoji
                ));
            }
        }
        weekly_summary.push_str("\n");
    }

    weekly_summary
}

async fn get_movie_map(client: &Client, sheet_url: &str) -> Result<HashMap<String, MovieGroup>> {
    // Regex to split title from rating "The Matrix - ★★★★"
    let title_regex = Regex::new(r"^(.*?)(\s-\s([★½]+))?$")?;
    // Regex to extract the film slug "letterboxd.com/user/film/slug/"
    let link_regex = Regex::new(r"letterboxd\.com/[^/]+/film/([^/]+)/")?;

    println!("Fetching friend list...");
    let csv_content = client.get(sheet_url).send().await?.text().await?;
    let mut rdr = csv::Reader::from_reader(csv_content.as_bytes());

    let mut movie_map: HashMap<String, MovieGroup> = HashMap::new();
    let seven_days_ago = Utc::now() - Duration::days(7);

    for result in rdr.records() {
        let record = result?;
        let friend_name = record.get(0).unwrap_or("Unknown").trim().to_string();
        let username = record.get(1).context("No username")?.trim();

        let feed_url = format!("https://letterboxd.com/{}/rss/", username);

        // We use if let so a single bad feed doesn't crash the whole bot
        if let Ok(channel) = fetch_and_parse_feed(&client, &feed_url).await {
            for item in channel.items() {
                if let Some(pub_date_str) = item.pub_date() {
                    if let Ok(pub_date) = DateTime::parse_from_rfc2822(pub_date_str) {
                        if pub_date.with_timezone(&Utc) >= seven_days_ago {
                            let raw_title = item.title().unwrap_or("Unknown Movie");
                            let user_link = item.link().unwrap_or("");

                            // Extract clean title and rating
                            let (clean_title, rating_raw) = match title_regex.captures(raw_title) {
                                Some(caps) => {
                                    let title = caps.get(1).map_or("", |m| m.as_str()).to_string();
                                    let stars = caps.get(3).map_or("", |m| m.as_str()).to_string();
                                    (title, stars)
                                }
                                None => (raw_title.to_string(), "".to_string()),
                            };

                            // Generate movie link
                            let general_link = match link_regex.captures(user_link) {
                                Some(caps) => format!("https://letterboxd.com/film/{}/", &caps[1]),
                                None => user_link.to_string(),
                            };

                            let entry = ReviewEntry {
                                friend_name: friend_name.clone(),
                                rating_raw,
                            };

                            movie_map
                                .entry(clean_title)
                                .and_modify(|group| group.reviews.push(entry.clone()))
                                .or_insert(MovieGroup {
                                    general_link,
                                    reviews: vec![entry],
                                });
                        }
                    }
                }
            }
        }
    }

    Ok(movie_map)
}

async fn send_whatsapp(
    client: &Client,
    message: &str,
    token: &str,
    group_id: &str,
) -> Result<String> {
    let url = "https://gate.whapi.cloud/messages/text";

    let payload = serde_json::json!({ "to": group_id, "body": message });
    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_body = response
            .text()
            .await
            .unwrap_or_else(|_| "No error details provided".to_string());
        anyhow::bail!(
            "Send message failed! Status: {}. Details: {}",
            status,
            error_body
        );
    }

    println!("Sent the Whatsapp message");

    let body_text = response.text().await?;
    let json: Value = serde_json::from_str(&body_text)?;

    let msg_id = json
        .get("message")
        .and_then(|m| m.get("id"))
        .and_then(|id| id.as_str())
        .context("Could not parse Message ID from API response")?
        .to_string();

    Ok(msg_id)
}

async fn pin_message(client: &Client, message_id: &str, token: &str) -> Result<()> {
    let url = format!("https://gate.whapi.cloud/messages/{}/pin", message_id);

    let payload = serde_json::json!({"time": "week"});

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_body = response.text().await.unwrap_or_default();
        anyhow::bail!("Pin Failed! Status: {}. Details: {}", status, error_body);
    }

    print!("Pinned the message");
    Ok(())
}

async fn set_presence_offline(client: &Client, token: &str) -> Result<()> {
    let url = "https://gate.whapi.cloud/users/presence";

    let payload = serde_json::json!({ "presence": "offline" });

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "Set to offline failed! Status: {}. Details: {}",
            status,
            error_body
        );
    }

    println!("Set the status to offline");
    Ok(())
}
