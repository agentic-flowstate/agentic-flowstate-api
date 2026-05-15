use anyhow::{Context, Result};
use sqlx::SqlitePool;

const SENT_THREAD_LOOKBACK_SECONDS: i64 = 60 * 60 * 24 * 90;

#[derive(Debug, sqlx::FromRow)]
struct ExistingThreadRow {
    thread_id: String,
}

#[derive(Debug, sqlx::FromRow)]
struct SentThreadCandidate {
    message_id: String,
    thread_id: Option<String>,
    to_addresses: String,
    cc_addresses: Option<String>,
    subject: Option<String>,
}

pub(crate) async fn resolve_email_thread_id(
    pool: &SqlitePool,
    mailbox: &str,
    folder: &str,
    from_address: &str,
    subject: Option<&str>,
    received_at: i64,
    references: &[String],
    in_reply_to: Option<&str>,
) -> Result<Option<String>> {
    if let Some(thread_id) =
        find_thread_by_message_reference(pool, mailbox, references, in_reply_to).await?
    {
        return Ok(Some(thread_id));
    }

    if folder.eq_ignore_ascii_case("sent") {
        return Ok(None);
    }

    find_matching_sent_thread(pool, mailbox, from_address, subject, received_at).await
}

async fn find_thread_by_message_reference(
    pool: &SqlitePool,
    mailbox: &str,
    references: &[String],
    in_reply_to: Option<&str>,
) -> Result<Option<String>> {
    for candidate in message_reference_candidates(references, in_reply_to) {
        let row = sqlx::query_as::<_, ExistingThreadRow>(
            r#"
            SELECT COALESCE(thread_id, message_id) AS thread_id
            FROM emails
            WHERE mailbox = ? AND (message_id = ? OR thread_id = ?)
            ORDER BY received_at ASC
            LIMIT 1
            "#,
        )
        .bind(mailbox)
        .bind(&candidate)
        .bind(&candidate)
        .fetch_optional(pool)
        .await
        .context("Failed to resolve email thread by message reference")?;

        if let Some(row) = row {
            return Ok(Some(row.thread_id));
        }
    }

    Ok(None)
}

async fn find_matching_sent_thread(
    pool: &SqlitePool,
    mailbox: &str,
    from_address: &str,
    subject: Option<&str>,
    received_at: i64,
) -> Result<Option<String>> {
    let Some(incoming_subject_key) = subject_conversation_key(subject) else {
        return Ok(None);
    };
    let sender = normalize_email_address(from_address);
    let Some(sender_domain) = email_domain(&sender) else {
        return Ok(None);
    };

    let rows = sqlx::query_as::<_, SentThreadCandidate>(
        r#"
        SELECT message_id, thread_id, to_addresses, cc_addresses, subject
        FROM emails
        WHERE mailbox = ?
          AND folder = 'Sent'
          AND received_at <= ?
          AND received_at >= ?
          AND subject IS NOT NULL
        ORDER BY received_at DESC
        LIMIT 250
        "#,
    )
    .bind(mailbox)
    .bind(received_at)
    .bind(received_at - SENT_THREAD_LOOKBACK_SECONDS)
    .fetch_all(pool)
    .await
    .context("Failed to load sent email candidates for thread matching")?;

    for row in rows {
        if subject_conversation_key(row.subject.as_deref()).as_deref()
            != Some(incoming_subject_key.as_str())
        {
            continue;
        }

        let recipients = sent_candidate_recipients(&row);
        if recipients.iter().any(|recipient| {
            let recipient = normalize_email_address(recipient);
            recipient == sender
                || email_domain(&recipient).as_deref() == Some(sender_domain.as_str())
        }) {
            return Ok(Some(row.thread_id.unwrap_or(row.message_id)));
        }
    }

    Ok(None)
}

fn sent_candidate_recipients(row: &SentThreadCandidate) -> Vec<String> {
    let mut recipients = parse_address_json(&row.to_addresses);
    if let Some(cc) = &row.cc_addresses {
        recipients.extend(parse_address_json(cc));
    }
    recipients
}

fn parse_address_json(value: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(value).unwrap_or_default()
}

fn message_reference_candidates(references: &[String], in_reply_to: Option<&str>) -> Vec<String> {
    let mut values = Vec::new();

    for reference in references {
        push_reference_variants(&mut values, reference);
    }
    if let Some(in_reply_to) = in_reply_to {
        push_reference_variants(&mut values, in_reply_to);
    }

    values
}

fn push_reference_variants(values: &mut Vec<String>, raw: &str) {
    let normalized = normalize_message_reference(raw);
    if normalized.is_empty() {
        return;
    }

    let bracketed = format!("<{}>", normalized);
    for value in [normalized, bracketed] {
        if !values.contains(&value) {
            values.push(value);
        }
    }
}

fn normalize_message_reference(value: &str) -> String {
    value
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .trim()
        .to_string()
}

fn subject_conversation_key(subject: Option<&str>) -> Option<String> {
    let mut value = subject?.trim();
    if value.is_empty() {
        return None;
    }

    loop {
        let without_prefix = strip_reply_prefix(value);
        let without_tag = strip_leading_bracketed_tag(without_prefix);
        if without_tag == value {
            break;
        }
        value = without_tag;
    }

    let key = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

fn strip_reply_prefix(value: &str) -> &str {
    let trimmed = value.trim_start();
    let lowercase = trimmed.to_ascii_lowercase();
    for prefix in ["re", "fw", "fwd", "回复", "答复", "回覆"] {
        if !lowercase.starts_with(prefix) {
            continue;
        }

        let rest = &trimmed[prefix.len()..].trim_start();
        if let Some(stripped) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：')) {
            return stripped.trim_start();
        }
    }
    trimmed
}

fn strip_leading_bracketed_tag(value: &str) -> &str {
    let trimmed = value.trim_start();
    if !trimmed.starts_with('[') {
        return trimmed;
    }

    if let Some(end) = trimmed.find(']') {
        if end <= 32 {
            return trimmed[end + 1..].trim_start();
        }
    }
    trimmed
}

fn normalize_email_address(value: &str) -> String {
    value
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .to_ascii_lowercase()
}

fn email_domain(address: &str) -> Option<String> {
    address
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim().to_ascii_lowercase())
        .filter(|domain| !domain.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_key_strips_common_reply_prefixes() {
        assert_eq!(
            subject_conversation_key(Some(
                "Re : Quote guidance for one assembled LAMP Rev A prototype PCBA"
            )),
            subject_conversation_key(Some(
                "Quote guidance for one assembled LAMP Rev A prototype PCBA"
            ))
        );
        assert_eq!(
            subject_conversation_key(Some(
                "回复：[## 353130 ##] Quote guidance for one assembled LAMP Rev A prototype PCBA"
            )),
            subject_conversation_key(Some(
                "Quote guidance for one assembled LAMP Rev A prototype PCBA"
            ))
        );
    }

    #[test]
    fn message_reference_candidates_include_bracketed_and_unbracketed_forms() {
        let refs = vec!["<root@example.com>".to_string()];
        let candidates = message_reference_candidates(&refs, Some("parent@example.com"));

        assert_eq!(
            candidates,
            vec![
                "root@example.com",
                "<root@example.com>",
                "parent@example.com",
                "<parent@example.com>"
            ]
        );
    }

    #[test]
    fn empty_subject_key_is_none() {
        assert_eq!(subject_conversation_key(Some("   ")), None);
        assert_eq!(subject_conversation_key(None), None);
    }
}
