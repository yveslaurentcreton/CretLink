//! The customer's saved copy of the end report: one self-contained HTML file
//! (ST-M009-012).
//!
//! The file is written only when the customer asks for a copy. It is plain
//! HTML with inline CSS: no script, no external resource (no font, image, link
//! or style sheet), so it opens the same offline in any browser and cannot
//! reach the network. Every string that came from the session — operations,
//! paths, purposes, notes, names — is HTML-escaped, so a hostile string is
//! shown as text, never interpreted (REQ-S-016). States are written in words,
//! never by colour alone.

use std::fmt::Write as _;

use crate::activity::{ActivityEntry, ActivityItem, ActivitySource, ActivityState};
use crate::report::{CleanupState, CustomerReport, EndedBy};

/// The inline style sheet. Static, so it holds nothing from the session.
const STYLE: &str = "\
body{font-family:system-ui,-apple-system,\"Segoe UI\",sans-serif;color:#1d232b;\
background:#fff;margin:0;padding:24px;line-height:1.45}\
main{max-width:860px;margin:0 auto}\
h1{font-size:1.4em;margin:0 0 4px}\
h2{font-size:1.1em;margin:24px 0 8px}\
.band{display:inline-block;border:1px solid #6b7480;border-radius:4px;padding:2px 8px;\
font-weight:600;font-size:.85em}\
.muted{color:#56606b}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:4px 16px;margin:0}\
dt{color:#56606b}dd{margin:0}\
ul.notes{padding-left:20px}\
.problem{font-weight:600}\
table{border-collapse:collapse;width:100%;font-size:.9em}\
th,td{text-align:left;vertical-align:top;border-bottom:1px solid #d6dae0;padding:6px 8px}\
th{color:#56606b;font-weight:600}\
code{font-family:ui-monospace,Consolas,monospace;font-size:.95em;word-break:break-all}\
footer{margin-top:32px;font-size:.85em;color:#56606b}";

/// Renders the report as one self-contained HTML document.
#[must_use]
pub fn render(report: &CustomerReport) -> String {
    let mut out = String::with_capacity(8 * 1024);
    let host = escape(&report.hostname);
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    // No script, no external resource, no inline event handler: the policy
    // forbids anything but the inline style, should a viewer honour it.
    out.push_str(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         style-src 'unsafe-inline'\">\n",
    );
    let _ = writeln!(out, "<title>CretLink session report · {host}</title>");
    let _ = writeln!(out, "<style>{STYLE}</style>");
    out.push_str("</head>\n<body>\n<main>\n");

    out.push_str("<p><span class=\"band\">○ No access</span> The session has ended</p>\n");
    let _ = writeln!(out, "<h1>{}</h1>", escape(&ended_sentence(report)));
    let _ = writeln!(
        out,
        "<p class=\"muted\">CretLink session report for {host}. {}</p>",
        if report.control_granted {
            "The technician can no longer reach this computer. A new session needs a new code \
             and your permission again."
        } else {
            "Full control was never granted in this session."
        }
    );

    out.push_str("<h2>When</h2>\n<dl>\n");
    let _ = writeln!(
        out,
        "<dt>Started</dt><dd>{}</dd>",
        escape(&format_time(&report.started_at))
    );
    let _ = writeln!(
        out,
        "<dt>Ended</dt><dd>{}</dd>",
        escape(&format_time(&report.ended_at))
    );
    let _ = writeln!(
        out,
        "<dt>Duration</dt><dd>{}</dd>",
        format_duration(report.duration_ms)
    );
    if let Some(name) = &report.technician_name {
        let _ = writeln!(
            out,
            "<dt>Technician</dt><dd>{} <span class=\"muted\">(as the technician's computer \
             reports it)</span></dd>",
            escape(name)
        );
    }
    out.push_str("</dl>\n");

    let counts = &report.counts;
    out.push_str("<h2>What happened</h2>\n<dl>\n");
    let _ = writeln!(
        out,
        "<dt>Commands</dt><dd>{}{}</dd>",
        counts.commands,
        if counts.failed_commands > 0 {
            format!(" · {} failed", counts.failed_commands)
        } else {
            String::new()
        }
    );
    let _ = writeln!(
        out,
        "<dt>Files sent</dt><dd>{} (left this computer)</dd>",
        counts.files_sent
    );
    let _ = writeln!(
        out,
        "<dt>Files received</dt><dd>{}</dd>",
        counts.files_received
    );
    let _ = writeln!(out, "<dt>Screenshots</dt><dd>{}</dd>", counts.screenshots);
    out.push_str("</dl>\n");

    let _ = writeln!(
        out,
        "<h2>Cleanup: {}</h2>",
        match report.cleanup.state {
            CleanupState::CleanedUp => "cleaned up".to_owned(),
            CleanupState::Residuals => format!(
                "{} {} left behind",
                report.cleanup.residuals,
                if report.cleanup.residuals == 1 {
                    "item"
                } else {
                    "items"
                }
            ),
            CleanupState::Failed => "cleanup failed".to_owned(),
            CleanupState::Unknown => "not confirmed".to_owned(),
        }
    );
    if report.cleanup.notes.is_empty() {
        out.push_str("<p class=\"muted\">No cleanup notes.</p>\n");
    } else {
        out.push_str("<ul class=\"notes\">\n");
        for note in &report.cleanup.notes {
            if note.ok {
                let _ = writeln!(out, "<li>{}</li>", escape(&note.text));
            } else {
                let _ = writeln!(
                    out,
                    "<li><span class=\"problem\">Problem:</span> {}</li>",
                    escape(&note.text)
                );
            }
        }
        out.push_str("</ul>\n");
    }
    // What is on this computer on purpose is said beside the cleanup, so that
    // it is not taken for something this session left (ST-M009-017).
    if let Some(kept) = &report.kept_program {
        let _ = writeln!(out, "<p>{}</p>", escape(&kept.words()));
    }

    out.push_str("<h2>Activity</h2>\n");
    if report.activity.is_empty() {
        out.push_str("<p class=\"muted\">Nothing was done on this computer.</p>\n");
    } else {
        out.push_str(
            "<table>\n<thead><tr><th>Time</th><th>Who</th><th>What</th><th>Result</th></tr>\
             </thead>\n<tbody>\n",
        );
        for entry in &report.activity {
            match entry {
                ActivityEntry::Item { item } => activity_row(&mut out, item),
                ActivityEntry::Gap { count, at } => {
                    let _ = writeln!(
                        out,
                        "<tr><td>{}</td><td colspan=\"3\" class=\"muted\">{count} {} not \
                         shown here</td></tr>",
                        escape(&format_clock(at)),
                        if *count == 1 { "action" } else { "actions" }
                    );
                }
            }
        }
        out.push_str("</tbody>\n</table>\n");
    }

    out.push_str(
        "<footer>Saved by CretLink at your request. This copy holds what the CretLink window \
         showed you during the session and no session secret. Times are in UTC.</footer>\n",
    );
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

/// One activity row: the time, who started it, what it was (the stated
/// purpose beside, never instead of, the real operation) and its result.
fn activity_row(out: &mut String, item: &ActivityItem) {
    let who = match item.source {
        ActivitySource::Ai => "AI assistant",
        ActivitySource::Terminal => "Technician's terminal",
        ActivitySource::Technician => "Technician",
    };
    let mut what = escape(&item.description);
    if let Some(purpose) = &item.purpose {
        let _ = write!(
            what,
            "<br><span class=\"muted\">Stated purpose:</span> {}",
            escape(purpose)
        );
    }
    let _ = write!(what, "<br><code>{}</code>", escape(&item.operation));
    let _ = writeln!(
        out,
        "<tr><td>{}</td><td>{who}</td><td>{what}</td><td>{}</td></tr>",
        escape(&format_clock(&item.started_at)),
        escape(&state_words(&item.state))
    );
}

/// The headline: who ended the session, in the customer's words.
fn ended_sentence(report: &CustomerReport) -> String {
    match report.ended_by {
        Some(EndedBy::Customer) => "You stopped the session".to_owned(),
        Some(EndedBy::Technician) => match &report.technician_name {
            Some(name) => format!("{name} stopped the session"),
            None => "The technician stopped the session".to_owned(),
        },
        Some(EndedBy::ConnectionLost) => "The connection was lost".to_owned(),
        Some(EndedBy::Failure) => "The session ended because of a failure".to_owned(),
        None => "The session ended".to_owned(),
    }
}

/// An item's state in words.
fn state_words(state: &ActivityState) -> String {
    match state {
        ActivityState::Running => "still running".to_owned(),
        ActivityState::Done { exit_code: None } => "done".to_owned(),
        ActivityState::Done {
            exit_code: Some(code),
        } => format!("done (exit code {code})"),
        ActivityState::Failed { exit_code, message } => {
            let mut words = "failed".to_owned();
            if let Some(code) = exit_code {
                let _ = write!(words, " (exit code {code})");
            }
            if let Some(message) = message {
                let _ = write!(words, ": {message}");
            }
            words
        }
        ActivityState::Cancelled => "cancelled".to_owned(),
        ActivityState::TimedOut => "timed out".to_owned(),
    }
}

/// `2026-10-22T14:03:05.123Z` as `2026-10-22 14:03 UTC`; anything else as is.
fn format_time(rfc3339: &str) -> String {
    match (rfc3339.get(..10), rfc3339.get(11..16)) {
        (Some(date), Some(time)) => format!("{date} {time} UTC"),
        _ => rfc3339.to_owned(),
    }
}

/// `2026-10-22T14:03:05.123Z` as `14:03:05`; anything else as is.
fn format_clock(rfc3339: &str) -> String {
    rfc3339
        .get(11..19)
        .map_or_else(|| rfc3339.to_owned(), str::to_owned)
}

/// A duration in hours, minutes and seconds.
fn format_duration(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours} h {minutes:02} min")
    } else if minutes > 0 {
        format!("{minutes} min {seconds:02} s")
    } else {
        format!("{seconds} s")
    }
}

/// Escapes text for HTML element content and attribute values.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
