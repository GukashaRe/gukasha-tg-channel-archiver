// src/main.rs
//
// Telegram Public Channel Fetcher with:
// - Anonymous fetch via https://t.me/s/<channel>
// - TOML config
// - HTTP / SOCKS5 proxy
// - SQLite archive
// - Incremental updates
// - Deleted message detection
// - --store
// - --since N (only process most recent N fetched messages)
//
// Cargo.toml:
//
// [package]
// name = "tg_fetcher"
// version = "0.1.0"
// edition = "2024"
//
// [dependencies]
// reqwest = { version = "0.12", features = ["rustls-tls", "socks"] }
// tokio = { version = "1", features = ["full"] }
// scraper = "0.24"
// serde = { version = "1", features = ["derive"] }
// serde_json = "1"
// toml = "0.9"
// rusqlite = { version = "0.37", features = ["bundled"] }
//
// config.toml:
//
// channel = "rustlang"
// max_messages = 50
// pretty_json = true
//
// [proxy]
// enabled = true
// url = "socks5h://127.0.0.1:10808"
//
// [user_agent]
// value = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:150.0) Gecko/20100101 Firefox/150.0"
//
// [storage]
// enabled = true
// path = "data/monitor.db"

use reqwest::{Client, Proxy};
use rusqlite::{params, Connection};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Duration;

// =========================
// Config
// =========================

#[derive(Debug, Deserialize)]
struct Config {
    channel: String,
    max_messages: Option<usize>,
    pretty_json: Option<bool>,
    proxy: Option<ProxyConfig>,
    user_agent: Option<UserAgentConfig>,
    storage: Option<StorageConfig>,
}

#[derive(Debug, Deserialize)]
struct ProxyConfig {
    enabled: bool,
    url: String,
}

#[derive(Debug, Deserialize)]
struct UserAgentConfig {
    value: String,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    enabled: bool,
    path: String,
}

// =========================
// CLI Args
// =========================

#[derive(Debug, Default)]
struct Args {
    store: bool,
    since: Option<usize>, // only process latest N fetched messages
}

fn parse_args() -> Args {
    let mut args = Args::default();
    let mut iter = env::args().skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--store" => args.store = true,
            "--since" => {
                if let Some(v) = iter.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        args.since = Some(n);
                    }
                }
            }
            _ => {}
        }
    }

    args
}

// =========================
// Message
// =========================

#[derive(Debug, Serialize)]
struct Message {
    id: String,
    time: String,
    views: String,
    text: String,
    url: String,
}

// =========================
// Config Loading
// =========================

fn load_config() -> Result<Config, Box<dyn Error>> {
    let content = fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&content)?;
    Ok(config)
}

// =========================
// HTTP Client
// =========================

fn build_client(config: &Config) -> Result<Client, Box<dyn Error>> {
    let ua = config
        .user_agent
        .as_ref()
        .map(|u| u.value.as_str())
        .unwrap_or("Mozilla/5.0");

    let mut builder = Client::builder()
        .user_agent(ua)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .tcp_keepalive(Duration::from_secs(30));

    if let Some(proxy) = &config.proxy {
        if proxy.enabled {
            builder = builder.proxy(Proxy::all(&proxy.url)?);
        }
    }

    Ok(builder.build()?)
}

// =========================
// Fetch HTML
// =========================

async fn fetch_html(client: &Client, channel: &str) -> Result<String, Box<dyn Error>> {
    let url = format!("https://t.me/s/{}", channel);

    let response = client
        .get(&url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?
        .error_for_status()?;

    Ok(response.text().await?)
}

// =========================
// HTML Parsing
// =========================

fn collect_text(element: scraper::ElementRef) -> String {
    element
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_messages(html: &str, limit: usize) -> Result<Vec<Message>, Box<dyn Error>> {
    let document = Html::parse_document(html);

    let msg_selector = Selector::parse("div.tgme_widget_message_wrap")?;

    let text_selector_primary =
        Selector::parse("div.tgme_widget_message_text.js-message_text")?;
    let text_selector_fallback =
        Selector::parse("div.tgme_widget_message_text")?;

    let time_selector = Selector::parse("time")?;
    let views_selector = Selector::parse("span.tgme_widget_message_views")?;
    let link_selector = Selector::parse("a.tgme_widget_message_date")?;

    let mut messages = Vec::new();

    for msg in document.select(&msg_selector).take(limit) {
        let text = msg
            .select(&text_selector_primary)
            .next()
            .or_else(|| msg.select(&text_selector_fallback).next())
            .map(collect_text)
            .unwrap_or_default();

        let time = msg
            .select(&time_selector)
            .next()
            .and_then(|e| e.value().attr("datetime"))
            .unwrap_or("")
            .to_string();

        let views = msg
            .select(&views_selector)
            .next()
            .map(collect_text)
            .unwrap_or_default();

        let url = msg
            .select(&link_selector)
            .next()
            .and_then(|e| e.value().attr("href"))
            .unwrap_or("")
            .to_string();

        let id = url.rsplit('/').next().unwrap_or("").to_string();

        if text.is_empty() && time.is_empty() {
            continue;
        }

        messages.push(Message {
            id,
            time,
            views,
            text,
            url,
        });
    }

    Ok(messages)
}

// =========================
// SQLite
// =========================

fn init_db(db_path: &str) -> Result<Connection, Box<dyn Error>> {
    if let Some(parent) = Path::new(db_path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let conn = Connection::open(db_path)?;

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY,
            time TEXT NOT NULL,
            views TEXT NOT NULL,
            text TEXT NOT NULL,
            url TEXT NOT NULL UNIQUE,
            first_seen TEXT NOT NULL DEFAULT (datetime('now')),
            deleted INTEGER NOT NULL DEFAULT 0,
            deleted_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_messages_time
        ON messages(time);

        CREATE INDEX IF NOT EXISTS idx_messages_deleted
        ON messages(deleted);
        "#,
    )?;

    // Automatic migration for older databases.
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
        [],
    );

    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN deleted_at TEXT",
        [],
    );

    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_messages_deleted ON messages(deleted)",
        [],
    );

    Ok(conn)
}

// Insert only new messages. If a previously deleted message reappears,
// reset deleted=0 and clear deleted_at.
fn store_new_messages(
    conn: &mut Connection,
    messages: &[Message],
) -> Result<usize, Box<dyn Error>> {
    let tx = conn.transaction()?;
    let mut inserted = 0usize;

    {
        let mut exists_stmt = tx.prepare(
            "SELECT EXISTS(
                SELECT 1 FROM messages
                WHERE id = ?1 OR url = ?2
            )",
        )?;

        let mut insert_stmt = tx.prepare(
            r#"
            INSERT INTO messages (
                id, time, views, text, url, deleted, deleted_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL)
            "#,
        )?;

        let mut revive_stmt = tx.prepare(
            r#"
            UPDATE messages
            SET
                time = ?2,
                views = ?3,
                text = ?4,
                url = ?5,
                deleted = 0,
                deleted_at = NULL
            WHERE id = ?1 OR url = ?5
            "#,
        )?;

        for msg in messages {
            let exists: i64 =
                exists_stmt.query_row(params![msg.id, msg.url], |row| row.get(0))?;

            if exists == 0 {
                insert_stmt.execute(params![
                    msg.id,
                    msg.time,
                    msg.views,
                    msg.text,
                    msg.url
                ])?;
                inserted += 1;
            } else {
                // Refresh content and clear deleted flag if message reappears.
                revive_stmt.execute(params![
                    msg.id,
                    msg.time,
                    msg.views,
                    msg.text,
                    msg.url
                ])?;
            }
        }
    }

    tx.commit()?;
    Ok(inserted)
}

// Mark messages as deleted if they were previously visible (deleted=0)
// but are absent from the current fetched snapshot.
fn mark_deleted_messages(
    conn: &Connection,
    current_messages: &[Message],
) -> Result<usize, Box<dyn Error>> {
    let current_ids: HashSet<&str> =
        current_messages.iter().map(|m| m.id.as_str()).collect();

    let mut stmt = conn.prepare(
        "SELECT id FROM messages WHERE deleted = 0"
    )?;

    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut deleted_count = 0usize;

    for row in rows {
        let id = row?;

        if !current_ids.contains(id.as_str()) {
            conn.execute(
                r#"
                UPDATE messages
                SET
                    deleted = 1,
                    deleted_at = datetime('now')
                WHERE id = ?1
                  AND deleted = 0
                "#,
                params![id],
            )?;

            deleted_count += 1;
        }
    }

    Ok(deleted_count)
}

// =========================
// Main
// =========================

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = load_config()?;
    let args = parse_args();

    let client = build_client(&config)?;

    let mut limit = config.max_messages.unwrap_or(20);

    // --since N means only process the latest N fetched messages.
    if let Some(n) = args.since {
        limit = n;
    }

    let pretty_json = config.pretty_json.unwrap_or(true);

    let html = fetch_html(&client, &config.channel).await?;
    let messages = parse_messages(&html, limit)?;

    let storage_enabled = config
        .storage
        .as_ref()
        .map(|s| s.enabled)
        .unwrap_or(false);

    // Archive mode.
    if args.store || storage_enabled {
        let db_path = config
            .storage
            .as_ref()
            .map(|s| s.path.as_str())
            .unwrap_or("data/monitor.db");

        let mut conn = init_db(db_path)?;

        let inserted = store_new_messages(&mut conn, &messages)?;
        let deleted = mark_deleted_messages(&conn, &messages)?;

        println!(
            "新增 {} 条消息，标记删除 {} 条消息",
            inserted,
            deleted
        );

        return Ok(());
    }

    // Normal mode: print fetched messages.
    if pretty_json {
        println!("{}", serde_json::to_string_pretty(&messages)?);
    } else {
        println!("{}", serde_json::to_string(&messages)?);
    }

    Ok(())
}