use anyhow::Result;
use chrono::{DateTime, Utc};
use jyc_types::{InboundMessage, MessageAttachment, MessageContent};
use mail_parser::MimeHeaders;
use std::collections::HashMap;
use uuid::Uuid;

/// Parse a raw email into a normalized InboundMessage.
///
/// Extracts headers, body, attachments, and cleans the subject.
/// HTML emails are converted to markdown via the smtp module's html_to_markdown.
pub fn parse_raw_email(raw: &[u8], uid: u32) -> Result<InboundMessage> {
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow::anyhow!("failed to parse email"))?;

    let from = parsed
        .from()
        .and_then(|a| a.first())
        .map(|a| {
            (
                a.name().unwrap_or("").to_string(),
                a.address().unwrap_or("").to_string(),
            )
        })
        .unwrap_or_default();

    let to: Vec<String> = parsed
        .to()
        .map(|addrs| {
            addrs
                .iter()
                .filter_map(|a| a.address().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let subject = parsed.subject().unwrap_or("").to_string();
    let message_id = parsed.message_id().map(|s| s.to_string());

    let in_reply_to = parsed.in_reply_to().as_text().map(|s| s.to_string());

    let references: Option<Vec<String>> = {
        let refs = parsed.references();
        if let Some(list) = refs.as_text_list() {
            Some(list.into_iter().map(|s| s.to_string()).collect())
        } else {
            refs.as_text().map(|s| vec![s.to_string()])
        }
    };

    let text_body = parsed.body_text(0).map(|s| s.to_string());
    let html_body = parsed.body_html(0).map(|s| s.to_string());

    let best_text = if let Some(ref html) = html_body {
        let md = crate::smtp::client::html_to_markdown(html);
        let cleaned = jyc_core::email_parser::clean_email_body(&md);
        if cleaned.trim().is_empty() {
            text_body.map(|t| jyc_core::email_parser::clean_email_body(&t))
        } else {
            Some(cleaned)
        }
    } else {
        text_body.map(|t| jyc_core::email_parser::clean_email_body(&t))
    };

    let cleaned_subject = jyc_core::email_parser::strip_reply_prefix(&subject);

    let timestamp = parsed
        .date()
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
        .unwrap_or_else(Utc::now);

    let attachments: Vec<MessageAttachment> = parsed
        .attachments()
        .map(|att| {
            let filename = jyc_core::attachment_storage::sanitize_attachment_filename(
                att.attachment_name().unwrap_or("unnamed"),
            );
            let content_type = att
                .content_type()
                .map(|ct| {
                    let subtype = ct.subtype().unwrap_or("octet-stream");
                    format!("{}/{}", ct.ctype(), subtype)
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let content = att.contents().to_vec();
            let size = content.len();

            tracing::debug!(
                "Email UID={}: Attachment '{}' ({} bytes, {})",
                uid,
                filename,
                size,
                content_type
            );
            MessageAttachment {
                filename,
                content_type,
                size,
                content: Some(content),
                saved_path: None,
            }
        })
        .collect();

    let mut metadata = HashMap::new();
    if let Some(ref reply_to) = in_reply_to {
        metadata.insert(
            "in_reply_to".to_string(),
            serde_json::Value::String(reply_to.clone()),
        );
    }
    metadata.insert(
        "from".to_string(),
        serde_json::Value::String(from.1.clone()),
    );

    Ok(InboundMessage {
        id: Uuid::new_v4().to_string(),
        channel: "email".to_string(),
        channel_uid: uid.to_string(),
        sender: if from.0.is_empty() {
            from.1.clone()
        } else {
            from.0.clone()
        },
        sender_address: from.1,
        recipients: to,
        topic: cleaned_subject,
        content: MessageContent {
            text: best_text,
            html: html_body,
            markdown: None,
        },
        timestamp,
        thread_refs: references,
        reply_to_id: in_reply_to,
        external_id: message_id,
        attachments,
        metadata,
        matched_pattern: None,
    })
}
