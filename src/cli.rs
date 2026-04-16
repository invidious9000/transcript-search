use std::io::Write;

use crossterm::style::{Color, SetForegroundColor, ResetColor};
use futures_util::StreamExt;

fn provider_color(provider: &str) -> Color {
    match provider {
        "claude" => Color::Magenta,
        "codex" => Color::Cyan,
        "copilot" => Color::Blue,
        "vibe" => Color::Yellow,
        "gemini" => Color::Green,
        _ => Color::White,
    }
}

fn status_color(event_type: &str) -> Color {
    match event_type {
        "task_completed" => Color::Green,
        "task_failed" => Color::Red,
        "task_cancelled" => Color::DarkYellow,
        "task_started" => Color::White,
        _ => Color::DarkGrey,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 || args[1] != "tail" {
        eprintln!("Usage: bro tail [--team NAME] [--bro NAME] [--provider NAME]");
        std::process::exit(1);
    }

    let port = std::env::var("BBOX_PORT")
        .or_else(|_| std::env::var("BRO_PORT"))
        .unwrap_or_else(|_| "7264".into());

    // Parse filter flags
    let mut team_filter: Option<String> = None;
    let mut bro_filter: Option<String> = None;
    let mut provider_filter: Option<String> = None;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--team" if i + 1 < args.len() => { team_filter = Some(args[i + 1].clone()); i += 2; }
            "--bro" if i + 1 < args.len() => { bro_filter = Some(args[i + 1].clone()); i += 2; }
            "--provider" if i + 1 < args.len() => { provider_filter = Some(args[i + 1].clone()); i += 2; }
            _ => { i += 1; }
        }
    }

    let mut url = format!("http://127.0.0.1:{port}/tail");
    let mut params = Vec::new();
    if let Some(ref t) = team_filter { params.push(format!("team={t}")); }
    if let Some(ref b) = bro_filter { params.push(format!("bro={b}")); }
    if let Some(ref p) = provider_filter { params.push(format!("provider={p}")); }
    if !params.is_empty() {
        url = format!("{url}?{}", params.join("&"));
    }

    eprintln!("Connecting to {url}...");

    let client = reqwest::Client::new();
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to connect to blackboxd: {e}");
            eprintln!("Is the daemon running? Start with: blackboxd");
            std::process::exit(1);
        }
    };

    if !response.status().is_success() {
        eprintln!("Server returned {}", response.status());
        std::process::exit(1);
    }

    eprintln!("Connected. Streaming events...\n");

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut stdout = std::io::stdout();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Parse SSE events (data: {...}\n\n)
        while let Some(pos) = buffer.find("\n\n") {
            let event_text = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();

            for line in event_text.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(evt) = serde_json::from_str::<serde_json::Value>(data) {
                        render_event(&mut stdout, &evt);
                    }
                }
            }
        }
    }

    eprintln!("\nConnection closed.");
    Ok(())
}

fn render_event(out: &mut impl Write, evt: &serde_json::Value) {
    let event_type = evt["type"].as_str().unwrap_or("unknown");
    let task_id = evt["task_id"].as_str().unwrap_or("?");
    let short_id = if task_id.len() > 8 { &task_id[..8] } else { task_id };

    let bro_name = evt["bro_name"].as_str();
    let provider = evt["provider"].as_str().unwrap_or("");
    let label = bro_name.unwrap_or(short_id);

    let color = if !provider.is_empty() {
        provider_color(provider)
    } else {
        status_color(event_type)
    };

    match event_type {
        "task_started" => {
            let _ = write!(out, "{}", SetForegroundColor(color));
            let _ = write!(out, "[{label}/{provider}]");
            let _ = write!(out, "{}", ResetColor);
            let _ = writeln!(out, " started");
        }
        "task_progress" => {
            let activity = evt["activity"].as_str().unwrap_or("...");
            let _ = write!(out, "{}", SetForegroundColor(color));
            let _ = write!(out, "[{label}]");
            let _ = write!(out, "{}", ResetColor);
            let _ = writeln!(out, " {activity}");
        }
        "task_completed" => {
            let elapsed = evt["elapsed"].as_str().unwrap_or("?");
            let cost = evt["cost"].as_f64();
            let cost_str = cost.map(|c| format!(", ${c:.3}")).unwrap_or_default();
            let _ = write!(out, "{}", SetForegroundColor(Color::Green));
            let _ = write!(out, "[{label}] completed");
            let _ = write!(out, "{}", ResetColor);
            let _ = writeln!(out, " ({elapsed}{cost_str})");
        }
        "task_failed" => {
            let elapsed = evt["elapsed"].as_str().unwrap_or("?");
            let error = evt["error"].as_str().unwrap_or("");
            let _ = write!(out, "{}", SetForegroundColor(Color::Red));
            let _ = write!(out, "[{label}] failed");
            let _ = write!(out, "{}", ResetColor);
            let _ = writeln!(out, " ({elapsed}) {error}");
        }
        "task_cancelled" => {
            let elapsed = evt["elapsed"].as_str().unwrap_or("?");
            let _ = write!(out, "{}", SetForegroundColor(Color::DarkYellow));
            let _ = writeln!(out, "[{label}] cancelled ({elapsed})");
            let _ = write!(out, "{}", ResetColor);
        }
        _ => {
            let _ = writeln!(out, "[{label}] {event_type}");
        }
    }
    let _ = out.flush();
}
