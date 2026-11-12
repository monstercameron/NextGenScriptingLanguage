//! Issue #682: the drain budget, `--drain-timeout-ms`, and the cancel on expiry must be
//! documented in the language reference, the man page and the CLI usage text.
//! The default is `runtime::DEFAULT_DRAIN_TIMEOUT` (10 s); the expiry behaviour is the
//! watchdog in `src/runtime.rs`, which marks the run cancelled like a second interrupt.

const LANGUAGE: &str = include_str!("../docs/language.md");
const MAN_PAGE: &str = include_str!("../docs/man/execute.1");
const MAIN: &str = include_str!("../src/main.rs");

#[test]
fn language_reference_documents_the_default_budget_and_expiry() {
    assert!(LANGUAGE.contains("It defaults to\n10 seconds."));
    assert!(LANGUAGE.contains("`run --drain-timeout-ms N`"));
    assert!(LANGUAGE.contains("the `on interrupt` body is skipped"));
}

#[test]
fn man_page_documents_the_default_budget_and_expiry() {
    assert!(MAN_PAGE.contains("10000. Only with"));
    assert!(MAN_PAGE
        .contains("When the budget expires, the run is cancelled as by a second interrupt:"));
}

#[test]
fn usage_text_states_the_default_and_lists_systemd_timer() {
    assert!(MAIN.contains("Interrupt drain budget for run (default 10000 ms; expiry cancels)"));
    assert!(
        MAIN.contains("execute service generate <systemd|systemd-timer|launchd> FILE [options]")
    );
}
