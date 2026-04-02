use anyhow::{Context, Result};
use lettre::message::header::{ContentType, InReplyTo, References};
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use regex::Regex;
use std::sync::LazyLock;

use crate::config::types::SmtpConfig;

/// An outbound file attachment.
#[derive(Debug, Clone)]
pub struct EmailAttachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Markdown to HTML conversion using comrak (GFM mode).
pub fn markdown_to_html(markdown: &str) -> String {
    let options = comrak::Options {
        extension: comrak::ExtensionOptions {
            strikethrough: true,
            table: true,
            autolink: true,
            tasklist: true,
            ..Default::default()
        },
        ..Default::default()
    };
    comrak::markdown_to_html(markdown, &options)
}

// --- Static regexes for html_to_markdown (compiled once) ---
static STYLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<style[^>]*>.*?</style>").unwrap());
static SCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<script[^>]*>.*?</script>").unwrap());
static HEAD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<head[^>]*>.*?</head>").unwrap());
static COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
static META_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<meta[^>]*>").unwrap());
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<link[^>]*>").unwrap());
static CSS_RULE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@(?:import|media)[^{]*\{[^}]*\}").unwrap());
static CSS_IMPORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@import\s+url\([^)]*\)\s*;?").unwrap());
static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

/// HTML to Markdown conversion using htmd.
///
/// Strips email HTML boilerplate (style tags, CSS, meta tags, comments)
/// before converting visible content to markdown.
pub fn html_to_markdown(html: &str) -> String {
    let mut cleaned = html.to_string();

    // Remove <style>...</style> blocks (including content)
    cleaned = STYLE_RE.replace_all(&cleaned, "").to_string();

    // Remove <script>...</script> blocks
    cleaned = SCRIPT_RE.replace_all(&cleaned, "").to_string();

    // Remove <head>...</head> blocks
    cleaned = HEAD_RE.replace_all(&cleaned, "").to_string();

    // Remove HTML comments
    cleaned = COMMENT_RE.replace_all(&cleaned, "").to_string();

    // Remove <meta> tags
    cleaned = META_RE.replace_all(&cleaned, "").to_string();

    // Remove <link> tags (CSS includes)
    cleaned = LINK_RE.replace_all(&cleaned, "").to_string();

    // Remove @import and @media CSS rules that leak into text
    cleaned = CSS_RULE_RE.replace_all(&cleaned, "").to_string();
    cleaned = CSS_IMPORT_RE.replace_all(&cleaned, "").to_string();

    htmd::convert(&cleaned).unwrap_or_else(|_| {
        // If htmd fails, do basic tag stripping
        TAG_RE.replace_all(&cleaned, "").to_string()
    })
}

/// SMTP client wrapper around lettre.
///
/// Handles connection, markdown→HTML conversion, threading headers,
/// and auto-reconnect on connection errors.
pub struct SmtpClient {
    transport: Option<AsyncSmtpTransport<Tokio1Executor>>,
    config: SmtpConfig,
}

impl SmtpClient {
    pub fn new(config: SmtpConfig) -> Self {
        Self {
            transport: None,
            config,
        }
    }

    /// Connect to the SMTP server.
    pub async fn connect(&mut self) -> Result<()> {
        let creds = Credentials::new(
            self.config.username.clone(),
            self.config.password.clone(),
        );

        let transport = if self.config.secure {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&self.config.host)
                .context("failed to create SMTP relay")?
                .credentials(creds)
                .port(self.config.port)
                .build()
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.config.host)
                .context("failed to create SMTP STARTTLS relay")?
                .credentials(creds)
                .port(self.config.port)
                .build()
        };

        // Test the connection
        transport
            .test_connection()
            .await
            .context("SMTP connection test failed")?;

        tracing::info!(
            host = %self.config.host,
            port = self.config.port,
            "SMTP connected"
        );

        self.transport = Some(transport);
        Ok(())
    }

    /// Disconnect from the SMTP server.
    pub async fn disconnect(&mut self) {
        self.transport = None;
        tracing::debug!("SMTP disconnected");
    }

    /// Send a reply email with threading headers and optional attachments.
    ///
    /// - Adds `Re:` prefix to subject (if not already present)
    /// - Sets `In-Reply-To` and `References` headers for threading
    /// - Converts markdown body to HTML for multipart email
    /// - Attaches files if provided
    pub async fn send_reply(
        &mut self,
        from: &str,
        from_name: Option<&str>,
        to: &str,
        subject: &str,
        markdown_body: &str,
        in_reply_to: Option<&str>,
        references: Option<&[String]>,
        attachments: Option<&[EmailAttachment]>,
    ) -> Result<String> {
        let html_body = markdown_to_html(markdown_body);

        // Build subject with Re: prefix
        let reply_subject = if subject.to_lowercase().starts_with("re:") {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        };

        let from_mailbox: Mailbox = if let Some(name) = from_name {
            format!("{name} <{from}>")
                .parse()
                .with_context(|| format!("invalid from address: {from}"))?
        } else {
            from.parse()
                .with_context(|| format!("invalid from address: {from}"))?
        };

        let to_mailbox: Mailbox = to
            .parse()
            .with_context(|| format!("invalid to address: {to}"))?;

        let mut builder = Message::builder()
            .from(from_mailbox)
            .to(to_mailbox)
            .subject(&reply_subject);

        // Add threading headers
        if let Some(reply_to) = in_reply_to {
            builder = builder.header(InReplyTo::from(reply_to.to_string()));
        }
        if let Some(refs) = references {
            let refs_str = refs.join(" ");
            builder = builder.header(References::from(refs_str));
        }

        // Build the body part (text + HTML alternative)
        let body_part = MultiPart::alternative()
            .singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(markdown_body.to_string()),
            )
            .singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_HTML)
                    .body(html_body),
            );

        // Build the email: if attachments, wrap in mixed multipart
        let email = if let Some(atts) = attachments {
            if atts.is_empty() {
                builder
                    .multipart(body_part)
                    .context("failed to build email message")?
            } else {
                let mut mixed = MultiPart::mixed().multipart(body_part);
                for att in atts {
                    let ct: ContentType = att.content_type.parse().unwrap_or(ContentType::parse(
                        "application/octet-stream",
                    ).unwrap());
                    let attachment = Attachment::new(att.filename.clone())
                        .body(att.data.clone(), ct);
                    mixed = mixed.singlepart(attachment);
                }
                builder
                    .multipart(mixed)
                    .context("failed to build email with attachments")?
            }
        } else {
            builder
                .multipart(body_part)
                .context("failed to build email message")?
        };

        let message_id = email
            .headers()
            .get_raw("Message-ID")
            .unwrap_or_default()
            .to_string();

        self.send_with_retry(email).await?;

        tracing::info!(
            to = %to,
            subject = %reply_subject,
            "Reply sent"
        );

        Ok(message_id)
    }

    /// Send a fresh (non-reply) email — no threading headers.
    pub async fn send_mail(
        &mut self,
        from: &str,
        to: &str,
        subject: &str,
        markdown_body: &str,
    ) -> Result<String> {
        let html_body = markdown_to_html(markdown_body);

        let from_mailbox: Mailbox = from
            .parse()
            .with_context(|| format!("invalid from address: {from}"))?;
        let to_mailbox: Mailbox = to
            .parse()
            .with_context(|| format!("invalid to address: {to}"))?;

        let email = Message::builder()
            .from(from_mailbox)
            .to(to_mailbox)
            .subject(subject)
            .multipart(
                MultiPart::alternative()
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_PLAIN)
                            .body(markdown_body.to_string()),
                    )
                    .singlepart(
                        SinglePart::builder()
                            .header(ContentType::TEXT_HTML)
                            .body(html_body),
                    ),
            )
            .context("failed to build email message")?;

        let message_id = email
            .headers()
            .get_raw("Message-ID")
            .unwrap_or_default()
            .to_string();

        self.send_with_retry(email).await?;

        tracing::info!(to = %to, subject = %subject, "Email sent");

        Ok(message_id)
    }

    /// Send an email with one retry on connection errors.
    async fn send_with_retry(&mut self, email: Message) -> Result<()> {
        let transport = self
            .transport
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("SMTP: not connected"))?;

        match transport.send(email.clone()).await {
            Ok(_) => Ok(()),
            Err(e) => {
                let err_str = e.to_string().to_lowercase();
                if err_str.contains("connect")
                    || err_str.contains("econn")
                    || err_str.contains("timeout")
                {
                    tracing::warn!(error = %e, "SMTP connection error, reconnecting...");
                    self.reconnect().await?;
                    let transport = self
                        .transport
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("SMTP: reconnect failed"))?;
                    transport
                        .send(email)
                        .await
                        .context("SMTP send failed after reconnect")?;
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("SMTP send failed: {e}"))
                }
            }
        }
    }

    /// Reconnect to SMTP server.
    async fn reconnect(&mut self) -> Result<()> {
        self.disconnect().await;
        self.connect().await
    }

    /// Check if the client is connected.
    #[allow(dead_code)]
    pub fn is_connected(&self) -> bool {
        self.transport.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_to_html() {
        let md = "Hello **world**\n\n- item 1\n- item 2";
        let html = markdown_to_html(md);
        assert!(html.contains("<strong>world</strong>"));
        assert!(html.contains("<li>item 1</li>"));
    }

    #[test]
    fn test_markdown_to_html_table() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let html = markdown_to_html(md);
        assert!(html.contains("<table>"));
        assert!(html.contains("<td>1</td>"));
    }

    #[test]
    fn test_html_to_markdown() {
        let html = "<p>Hello <strong>world</strong></p>";
        let md = html_to_markdown(html);
        assert!(md.contains("**world**"));
    }
}
