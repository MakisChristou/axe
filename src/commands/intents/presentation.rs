mod types;

use std::time::Duration;

use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use indicatif::ProgressBar;

use crate::ui;

pub use types::IntentActivity;

impl IntentActivity {
    pub fn new(message: &str, visible: bool) -> Self {
        Self {
            bar: if visible {
                intent_activity_bar(message)
            } else {
                ProgressBar::hidden()
            },
        }
    }
}

impl Drop for IntentActivity {
    fn drop(&mut self) {
        self.bar.finish_and_clear();
    }
}

pub fn intent_progress_bar(length: u64, message: &str) -> ProgressBar {
    let progress = intent_activity_bar(message);
    progress.set_length(length);
    progress.set_style(ui::progress_spinner_style(
        "  {spinner:.cyan} {elapsed_precise} {pos}/{len} fulfilled\n  {msg}",
    ));
    progress
}

pub fn intent_activity_bar(message: &str) -> ProgressBar {
    let progress = ProgressBar::new_spinner();
    progress.set_style(ui::progress_spinner_style(
        "  {spinner:.cyan} {elapsed_precise}  {msg}",
    ));
    progress.set_message(message.to_owned());
    progress.enable_steady_tick(Duration::from_millis(100));
    progress
}

pub fn set_intent_traffic_message(progress: &ProgressBar, context: &str, status: &str) {
    let fulfilled = progress.position();
    let seconds = progress.elapsed().as_secs_f64();
    let rate = if seconds > 0.0 {
        fulfilled as f64 / seconds
    } else {
        0.0
    };
    progress.set_message(format!(
        "RUNNING | FILL {rate:.2}/s (average)\n  {fulfilled} fulfilled | {context}\n  {}",
        compact_detail(status)
    ));
}

pub fn compact_detail(message: &str) -> String {
    let message = ui::scrub_urls(message).replace(['\n', '\r'], " ");
    let width = 100;
    if message.chars().count() <= width {
        return message;
    }

    let mut truncated = message
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

pub fn asset_table(headers: &[&str]) -> Table {
    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(headers.iter().map(|header| header_cell(header)));
    table
}

pub fn header_cell(label: &str) -> Cell {
    Cell::new(label)
        .fg(Color::Cyan)
        .add_attribute(Attribute::Bold)
}

pub fn format_usd(value: f64) -> String {
    format!("${}", format_number(value, 2))
}

pub fn format_usd_price(value: f64) -> String {
    let precision = if value < 1.0 { 4 } else { 2 };
    format!("${}", format_number(value, precision))
}

pub fn format_token_amount(value: &str) -> String {
    let Ok(value) = value.parse::<f64>() else {
        return value.to_owned();
    };
    let precision = if value >= 1_000.0 {
        2
    } else if value >= 1.0 {
        4
    } else {
        6
    };
    trim_fraction(format_number(value, precision))
}

fn format_number(value: f64, precision: usize) -> String {
    let value = if value == 0.0 { 0.0 } else { value };
    let formatted = format!("{value:.precision$}");
    let (whole, fraction) = formatted.split_once('.').unwrap_or((&formatted, ""));
    let grouped = group_digits(whole);
    if fraction.is_empty() {
        grouped
    } else {
        format!("{grouped}.{fraction}")
    }
}

fn group_digits(value: &str) -> String {
    let mut grouped = String::with_capacity(value.len() + value.len() / 3);
    for (index, character) in value.chars().enumerate() {
        if index > 0 && (value.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

fn trim_fraction(mut value: String) -> String {
    if value.contains('.') {
        while value.ends_with('0') {
            value.pop();
        }
        if value.ends_with('.') {
            value.pop();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_human_facing_numbers() {
        assert_eq!(format_usd(12_345.678), "$12,345.68");
        assert_eq!(format_usd_price(0.123_456), "$0.1235");
        assert_eq!(format_token_amount("12345.600000"), "12,345.6");
        assert_eq!(format_token_amount("0.000001"), "0.000001");
    }

    #[test]
    fn details_are_bounded_without_truncating_metrics() {
        let progress = ProgressBar::hidden();
        progress.set_position(12);
        set_intent_traffic_message(&progress, "2 route failures", &"x".repeat(150));
        let message = progress.message();
        assert!(message.contains("12 fulfilled | 2 route failures"));
        assert_eq!(message.lines().count(), 3);
        assert_eq!(message.lines().last().unwrap().trim().chars().count(), 100);
        assert!(message.ends_with('…'));
        assert_eq!(compact_detail("one\ntwo\rthree"), "one two three");
    }

    #[test]
    fn activity_clears_on_drop_and_json_mode_is_hidden() {
        let activity = IntentActivity::new("loading", false);
        let bar = activity.bar.clone();
        assert!(bar.is_hidden());
        drop(activity);
        assert!(bar.is_finished());
    }
}
