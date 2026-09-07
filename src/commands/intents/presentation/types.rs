use indicatif::ProgressBar;

/// Clears transient output on success, errors, and cancellation.
pub struct IntentActivity {
    pub bar: ProgressBar,
}
