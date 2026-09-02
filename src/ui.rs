use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use tokio::task::spawn_blocking;

/// Print a step header: "[5/23] Deploying AxelarGateway..."
pub fn step_header(current: usize, total: usize, name: &str) {
    let prefix = format!("[{}/{}]", current, total);
    println!("{} {}", prefix.green().bold(), name.bold());
}

/// Print success: "  + {msg}" in green
pub fn success(msg: &str) {
    println!("  {} {}", "+".green(), msg.green());
}

/// Print success with a dimmed chain annotation: "  + {msg}  ({annotation})"
pub fn success_annotated(msg: &str, annotation: &str) {
    println!(
        "  {} {}  {}",
        "+".green(),
        msg.green(),
        format!("({annotation})").dimmed()
    );
}

/// Print info: "  {msg}" in dimmed
pub fn info(msg: &str) {
    println!("  {}", msg.dimmed());
}

/// Print warning: "  ! {msg}" in yellow
pub fn warn(msg: &str) {
    println!("{}", warning_line(msg));
}

/// Print a warning to stderr so structured stdout remains machine-readable.
pub fn warn_stderr(msg: &str) {
    eprintln!("{}", warning_line(msg));
}

pub fn warning_line(msg: &str) -> String {
    format!("  {} {}", "!".yellow(), msg.yellow())
}

/// Print error: "  x {msg}" in red
pub fn error(msg: &str) {
    println!("  {} {}", "x".red(), msg.red());
}

/// Print a tx hash: "  label: hash" with hash in cyan
pub fn tx_hash(label: &str, hash: &str) {
    println!("  {}: {}", label.dimmed(), hash.cyan());
}

/// Print an address: "  label: addr" with addr in cyan
pub fn address(label: &str, addr: &str) {
    println!("  {}: {}", label.dimmed(), addr.cyan());
}

/// Create a spinner with a message, returns ProgressBar handle.
/// Call `.finish_and_clear()` or `.finish_with_message()` when done.
pub fn wait_spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    if let Ok(style) = ProgressStyle::with_template("  {spinner:.cyan} {msg}") {
        pb.set_style(style.tick_strings(&["|", "/", "-", "\\", ""]));
    }
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

pub fn progress_spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_spinner())
}

pub fn progress_bar_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_bar())
}

/// Print a key-value pair: "  key: value" with key dimmed
pub fn kv(key: &str, value: &str) {
    println!("  {}: {}", key.dimmed(), value);
}

/// Print a section divider: "\n-- title --"
pub fn section(title: &str) {
    println!("\n{} {} {}", "--".dimmed(), title.bold(), "--".dimmed());
}

/// Print an action-required block in yellow
pub fn action_required(lines: &[&str]) {
    println!();
    println!("  {}", "ACTION REQUIRED:".yellow().bold());
    for line in lines {
        println!("  {}", line.yellow());
    }
    println!();
}

/// Ask the user to confirm an action on stdin.
///
/// Returns false when stdin is not interactive.
pub async fn confirm(prompt: &str) -> bool {
    let prompt = prompt.to_string();

    spawn_blocking(move || confirm_blocking(&prompt))
        .await
        .unwrap_or(false)
}

fn confirm_blocking(prompt: &str) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }

    print!("  {} {} ", prompt.bold(), "[y/N]".dimmed());
    let _ = io::stdout().flush();
    let mut input = String::new();

    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Format a duration for compact human-readable terminal output.
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        return format!("{millis} ms");
    }

    let seconds = duration.as_secs_f64();
    if seconds < 10.0 {
        return format!("{seconds:.2} s");
    }
    if seconds < 60.0 {
        return format!("{seconds:.1} s");
    }

    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let remaining_seconds = total_seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {remaining_seconds:02}s");
    }

    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    format!("{hours}h {remaining_minutes:02}m")
}

/// Format a millisecond metric for human-readable terminal output.
pub fn format_millis(milliseconds: u64) -> String {
    format_duration(Duration::from_millis(milliseconds))
}

/// Format elapsed time from an instant for human-readable terminal output.
pub fn format_elapsed(start: Instant) -> String {
    format_duration(start.elapsed())
}

/// Truncate large JSON strings to first/last N lines
pub fn truncated_json(json_str: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = json_str.lines().collect();
    if lines.len() <= max_lines * 2 + 1 {
        return json_str.to_string();
    }
    let head: Vec<&str> = lines[..max_lines].to_vec();
    let tail: Vec<&str> = lines[lines.len() - max_lines..].to_vec();
    let omitted = lines.len() - max_lines * 2;
    format!(
        "{}\n  ... ({} lines omitted) ...\n{}",
        head.join("\n"),
        omitted,
        tail.join("\n")
    )
}

/// Replace any `http://…` / `https://…` substring with `<redacted-url>`,
/// preserving the surrounding text. Used to keep RPC URLs (which can come
/// from repo secrets) out of the load-test JSON report and other surfaces
/// that may include propagated error messages.
///
/// Terminators recognised as the end of a URL: whitespace, `'`, `"`, `)`,
/// `]`, `,`, `;`, `<`, `>`.
pub fn scrub_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '\'' | '"' | ')' | ']' | ',' | ';' | '<' | '>')
                })
                .unwrap_or(rest.len());
            out.push_str("<redacted-url>");
            i += end;
        } else {
            let ch_len = rest.chars().next().map_or(1, char::len_utf8);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_subsecond_durations_as_milliseconds() {
        assert_eq!(format_duration(Duration::ZERO), "0 ms");
        assert_eq!(format_duration(Duration::from_millis(181)), "181 ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999 ms");
    }

    #[test]
    fn formats_seconds_with_scale_appropriate_precision() {
        assert_eq!(format_millis(2_924), "2.92 s");
        assert_eq!(format_millis(8_823), "8.82 s");
        assert_eq!(format_millis(11_929), "11.9 s");
    }

    #[test]
    fn formats_long_durations_compactly() {
        assert_eq!(format_duration(Duration::from_secs(72)), "1m 12s");
        assert_eq!(format_duration(Duration::from_secs(3_780)), "1h 03m");
    }
}
