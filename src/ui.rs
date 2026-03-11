use std::time::Instant;

use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;

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
    println!("  {} {}", "!".yellow(), msg.yellow());
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
    pb.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["|", "/", "-", "\\", ""]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
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

/// Format elapsed duration as human-readable
pub fn format_elapsed(start: Instant) -> String {
    let elapsed = start.elapsed();
    if elapsed.as_secs() >= 60 {
        format!(
            "{}m{:.1}s",
            elapsed.as_secs() / 60,
            elapsed.as_secs_f64() % 60.0
        )
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
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
