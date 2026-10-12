//! Delivery bodies for outgoing webhooks whose `format` is a chat service.
//!
//! `json` sends the envelope (`id`/`event`/`created_at`/`data`) unchanged. The
//! others build one short message from it: a title for the event and the
//! fields a person needs (job, queue, status, error, ...), in the shape the
//! service's incoming-webhook URL accepts. Headers and the signature are the
//! same for every format; the signature covers the body actually sent.

use serde_json::{json, Value};

use crate::models::WebhookFormat;

/// Longest message per service, in characters. Discord rejects `content` over
/// 2000; Slack and Teams accept far more, but an alert is meant to be read.
const SLACK_MAX_CHARS: usize = 3000;
const DISCORD_MAX_CHARS: usize = 2000;
const TEAMS_MAX_CHARS: usize = 3000;
/// Longest single field value (an error text, a JSON result).
const MAX_VALUE_CHARS: usize = 300;

/// The `data` fields shown, in this order, with their labels.
const FIELDS: &[(&str, &str)] = &[
    ("name", "Name"),
    ("workflow_id", "Workflow"),
    ("job_id", "Job"),
    ("queue_name", "Queue"),
    ("schedule_id", "Schedule"),
    ("worker_id", "Worker"),
    ("hostname", "Host"),
    ("status", "Status"),
    ("old_status", "Previous status"),
    ("source", "Source"),
    ("completed_jobs", "Completed jobs"),
    ("failed_jobs", "Failed jobs"),
    ("total_jobs", "Total jobs"),
    ("error", "Error"),
    ("reason", "Reason"),
    ("message", "Message"),
];

/// The body to send for `envelope` in `format`, serialized.
pub fn body_for(format: WebhookFormat, envelope: &Value) -> String {
    let body = match format {
        WebhookFormat::Json => return serde_json::to_string(envelope).unwrap_or_default(),
        WebhookFormat::Slack => slack(&Message::from_envelope(envelope)),
        WebhookFormat::Discord => discord(&Message::from_envelope(envelope)),
        WebhookFormat::Teams => teams(&Message::from_envelope(envelope)),
    };
    serde_json::to_string(&body).unwrap_or_default()
}

/// What every chat format shows.
struct Message {
    event: String,
    title: String,
    facts: Vec<(&'static str, String)>,
    tone: Tone,
}

#[derive(Clone, Copy)]
enum Tone {
    Good,
    Bad,
    Neutral,
}

impl Message {
    /// Read the event and its fields. `data` is the envelope's `data`; a body
    /// without one (the test endpoint's) is read as the data itself.
    fn from_envelope(envelope: &Value) -> Self {
        let event = envelope
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("event")
            .to_string();
        let data = envelope
            .get("data")
            .filter(|d| d.is_object())
            .unwrap_or(envelope);
        let facts = FIELDS
            .iter()
            .filter_map(|(key, label)| {
                let text = match data.get(*key)? {
                    Value::Null => return None,
                    Value::String(s) if s.is_empty() => return None,
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                Some((*label, truncate(&text, MAX_VALUE_CHARS)))
            })
            .collect();
        let tone = match event.as_str() {
            "job.failed" | "job.deadlettered" | "workflow.failed" | "worker.deregistered" => {
                Tone::Bad
            }
            "job.completed" | "workflow.completed" => Tone::Good,
            _ => Tone::Neutral,
        };
        Self {
            title: format!("Spooled: {}", title(&event)),
            event,
            facts,
            tone,
        }
    }
}

fn title(event: &str) -> &str {
    match event {
        "job.created" => "job created",
        "job.started" => "job started",
        "job.completed" => "job completed",
        "job.failed" => "job failed",
        "job.cancelled" => "job cancelled",
        "job.deadlettered" => "job moved to the dead-letter queue",
        "job.retried" => "job retried",
        "queue.paused" => "queue paused",
        "queue.resumed" => "queue resumed",
        "worker.registered" => "worker registered",
        "worker.deregistered" => "worker deregistered",
        "schedule.triggered" => "schedule triggered",
        "workflow.completed" => "workflow completed",
        "workflow.failed" => "workflow failed",
        "webhook.test" => "test webhook",
        other => other,
    }
}

/// Slack `{"text"}` in mrkdwn. `&`, `<` and `>` are escaped in everything
/// taken from the event, so a value cannot form a link or an `<!channel>`.
fn slack(message: &Message) -> Value {
    let mut text = format!(
        "*{}* (`{}`)",
        slack_escape(&message.title),
        slack_escape(&message.event)
    );
    for (label, value) in &message.facts {
        text.push_str(&format!("\n• {}: {}", label, slack_escape(value)));
    }
    json!({ "text": truncate(&text, SLACK_MAX_CHARS) })
}

fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Discord `{"content"}`. Markdown in values is escaped, and
/// `allowed_mentions` is empty, so an `@everyone` in an error text pings
/// nobody.
fn discord(message: &Message) -> Value {
    let mut text = format!(
        "**{}** (`{}`)",
        discord_escape(&message.title),
        message.event.replace('`', "'")
    );
    for (label, value) in &message.facts {
        text.push_str(&format!("\n- {}: {}", label, discord_escape(value)));
    }
    json!({
        "content": truncate(&text, DISCORD_MAX_CHARS),
        "allowed_mentions": { "parse": [] },
    })
}

fn discord_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#' | '[' | ']'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A Teams `MessageCard`: the title, the event as `text`, and the fields as
/// facts. Values are HTML-escaped, which Teams renders as plain text.
fn teams(message: &Message) -> Value {
    let color = match message.tone {
        Tone::Good => "2EB67D",
        Tone::Bad => "D9302F",
        Tone::Neutral => "4A6CF7",
    };
    let mut budget = TEAMS_MAX_CHARS;
    let facts: Vec<Value> = message
        .facts
        .iter()
        .take_while(|(label, value)| {
            let cost = label.len() + value.chars().count();
            let fits = cost <= budget;
            budget = budget.saturating_sub(cost);
            fits
        })
        .map(|(label, value)| json!({ "name": label, "value": html_escape(value) }))
        .collect();
    json!({
        "@type": "MessageCard",
        "@context": "https://schema.org/extensions",
        "summary": html_escape(&message.title),
        "themeColor": color,
        "title": html_escape(&message.title),
        "text": html_escape(&message.event),
        "sections": [ { "facts": facts } ],
    })
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// At most `max` characters, the last one an ellipsis when cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(event: &str, data: Value) -> Value {
        json!({
            "id": "6b0e5f0e-0000-4000-8000-000000000000",
            "event": event,
            "created_at": "2026-10-14T12:00:00Z",
            "data": data,
        })
    }

    fn parse(body: String) -> Value {
        serde_json::from_str(&body).unwrap()
    }

    #[test]
    fn json_is_the_envelope_unchanged() {
        let env = envelope("job.failed", json!({"job_id": "j1"}));
        assert_eq!(parse(body_for(WebhookFormat::Json, &env)), env);
    }

    #[test]
    fn slack_sends_text_with_the_event_and_its_fields() {
        let env = envelope(
            "job.deadlettered",
            json!({"job_id": "j1", "queue_name": "emails", "status": "deadletter",
                   "error": "SMTP <timeout> & retry"}),
        );
        let body = parse(body_for(WebhookFormat::Slack, &env));
        let obj = body.as_object().unwrap();
        assert_eq!(obj.len(), 1, "{body}");
        let text = body["text"].as_str().unwrap();
        assert!(
            text.starts_with("*Spooled: job moved to the dead-letter queue* (`job.deadlettered`)"),
            "{text}"
        );
        assert!(text.contains("• Job: j1"), "{text}");
        assert!(text.contains("• Queue: emails"), "{text}");
        assert!(
            text.contains("• Error: SMTP &lt;timeout&gt; &amp; retry"),
            "{text}"
        );
        // Fields come in a fixed order: job before queue before status.
        let (j, q, s) = (
            text.find("Job:").unwrap(),
            text.find("Queue:").unwrap(),
            text.find("Status:").unwrap(),
        );
        assert!(j < q && q < s, "{text}");
    }

    #[test]
    fn discord_sends_content_and_never_pings() {
        let env = envelope(
            "job.failed",
            json!({"job_id": "j1", "error": "@everyone *bold* `x`"}),
        );
        let body = parse(body_for(WebhookFormat::Discord, &env));
        let content = body["content"].as_str().unwrap();
        assert!(
            content.starts_with("**Spooled: job failed** (`job.failed`)"),
            "{content}"
        );
        assert!(content.contains(r"\*bold\* \`x\`"), "{content}");
        assert_eq!(body["allowed_mentions"], json!({"parse": []}));
    }

    #[test]
    fn discord_content_fits_the_2000_character_limit() {
        let long = "e".repeat(10_000);
        let mut data = json!({"job_id": "j1"});
        for key in ["error", "reason", "message", "name", "source"] {
            data[key] = json!(long);
        }
        let body = parse(body_for(
            WebhookFormat::Discord,
            &envelope("job.failed", data),
        ));
        let content = body["content"].as_str().unwrap();
        assert!(content.chars().count() <= DISCORD_MAX_CHARS);
        assert!(content.ends_with('…'));
    }

    #[test]
    fn teams_sends_a_message_card_with_facts() {
        let env = envelope(
            "workflow.completed",
            json!({"workflow_id": "wf", "name": "nightly <etl>", "status": "completed",
                   "completed_jobs": 3, "failed_jobs": 0, "total_jobs": 3, "metadata": {"k": 1}}),
        );
        let body = parse(body_for(WebhookFormat::Teams, &env));
        assert_eq!(body["@type"], "MessageCard");
        assert_eq!(body["@context"], "https://schema.org/extensions");
        assert_eq!(body["title"], "Spooled: workflow completed");
        assert_eq!(body["text"], "workflow.completed");
        assert_eq!(body["themeColor"], "2EB67D");
        let facts = body["sections"][0]["facts"].as_array().unwrap();
        assert_eq!(
            facts[0],
            json!({"name": "Name", "value": "nightly &lt;etl&gt;"})
        );
        assert!(facts.contains(&json!({"name": "Completed jobs", "value": "3"})));
        // Fields not in the list (metadata) are left out.
        assert!(!body.to_string().contains("metadata"));
    }

    #[test]
    fn a_body_without_data_is_read_as_the_data() {
        // The test endpoint's body is flat.
        let test = json!({"event": "webhook.test", "message": "hello", "test": true});
        let body = parse(body_for(WebhookFormat::Slack, &test));
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Spooled: test webhook"), "{text}");
        assert!(text.contains("Message: hello"), "{text}");
    }

    #[test]
    fn values_are_cut_and_unknown_events_keep_their_name() {
        let env = envelope("custom.thing", json!({"error": "x".repeat(1000)}));
        let body = parse(body_for(WebhookFormat::Slack, &env));
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Spooled: custom.thing"), "{text}");
        let error_line = text.lines().find(|l| l.contains("Error:")).unwrap();
        assert!(
            error_line.chars().count() < MAX_VALUE_CHARS + 20,
            "{error_line}"
        );
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "ab…");
    }
}
