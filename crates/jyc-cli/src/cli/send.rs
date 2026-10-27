//! `jyc send` — enqueue one or more messages for an existing agent/topic.
//!
//! This is the non-interactive face of the `/send` command: it talks to a
//! running inspect server (no TUI, no auto-start) and returns once each
//! message is enqueued. The target topic processes them in order.

use anyhow::{Result, bail};
use clap::Args;

use jyc_inspect::client::InspectClient;

use super::resolve::{resolve_inspect_client_addr, resolve_inspect_token};

/// Arguments for the `jyc send` command.
#[derive(Debug, Args)]
pub struct SendArgs {
    /// Inspect server address (default: [inspect].bind from config, else
    /// 127.0.0.1:9876)
    #[arg(long)]
    pub addr: Option<String>,

    /// Inspect authorization token (default: <data dir>/auth.token)
    #[arg(long, env = "JYC_DASHBOARD_TOKEN")]
    pub token: Option<String>,

    /// Agent whose own topic receives the messages
    pub agent: String,

    /// One of the agent's topics, instead of its own
    #[arg(short = 't', long)]
    pub topic: Option<String>,

    /// One message per argument; each is delivered separately, in order
    #[arg(required = true, num_args = 1..)]
    pub messages: Vec<String>,
}

/// Run `jyc send`: enqueue each message, stopping at the first rejection.
pub async fn run(
    args: &SendArgs,
    data_dir: &std::path::Path,
    data_dir_explicit: bool,
) -> Result<()> {
    let addr = resolve_inspect_client_addr(args.addr.as_deref(), data_dir, data_dir_explicit)?;
    let token = resolve_inspect_token(args.token.as_deref(), data_dir)?;
    let client = match token {
        Some(token) => InspectClient::with_token(&addr, Some(token.as_str())),
        None => InspectClient::new(&addr),
    };

    let total = args.messages.len();
    for (index, message) in args.messages.iter().enumerate() {
        let (ok, response) = client
            .send_to_topic(&args.agent, args.topic.as_deref(), message)
            .await?;
        if !ok {
            bail!(
                "failed to send message {} of {}: {}",
                index + 1,
                total,
                response
            );
        }
        if total == 1 {
            println!("{response}");
        } else {
            println!("[{}/{}] {response}", index + 1, total);
        }
    }
    Ok(())
}
