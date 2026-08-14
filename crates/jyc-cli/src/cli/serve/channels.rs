//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{Mutex, mpsc};

use anyhow::Result;

use jyc_channels::email::outbound::EmailOutboundAdapter;
use jyc_channels::feishu::outbound::FeishuOutboundAdapter;
use jyc_channels::gitee::outbound::GiteeOutboundAdapter;
use jyc_channels::github::outbound::GithubOutboundAdapter;
use jyc_channels::websocket::inbound::WebsocketInboundAdapter;
use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;
use jyc_channels::wechat::outbound::WechatOutboundAdapter;
use jyc_channels::wecom::kf_client::KfApiClient;
use jyc_channels::wecom::kf_outbound::WecomKfOutboundAdapter;
use jyc_channels::wecom::outbound::WecomOutboundAdapter;
use jyc_channels::wecom::token_cache::AccessTokenCache;
use jyc_channels::wecom_bot::client::WecomBotConnectionHandle;
use jyc_channels::wecom_bot::outbound::WecomBotOutboundAdapter;
use jyc_core::message_storage::MessageStorage;
use jyc_types::{ChannelConfig, OutboundAdapter, OutboundAttachmentConfig};

/// Build the outbound adapter for a channel.
///
/// Returns `Ok(None)` for unsupported channel types (the caller skips them,
/// matching the original inline `continue` behavior).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_outbound_adapter(
    channel_type: &str,
    channel_config: &ChannelConfig,
    channel_name: &str,
    storage: Arc<MessageStorage>,
    outbound_attachment_config: Option<OutboundAttachmentConfig>,
    footer_enabled: bool,
    workspace_dir: &std::path::PathBuf,
    inspect_broadcast: Arc<broadcast::Sender<String>>,
    wechat_sender_arc: &mut Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
    wecom_bot_handle_arc: &mut Option<Arc<Mutex<Option<WecomBotConnectionHandle>>>>,
    wecomkf_kf_client: &mut Option<Arc<KfApiClient>>,
    ws_handler_for_channel: &mut HashMap<String, Arc<WebsocketInboundAdapter>>,
    websocket_handlers: &mut Vec<Arc<WebsocketInboundAdapter>>,
) -> Result<Option<Arc<dyn OutboundAdapter>>> {
    let outbound: Arc<dyn OutboundAdapter> = match channel_type {
        "email" => {
            let outbound_config = channel_config.outbound.as_ref().ok_or_else(|| {
                anyhow::anyhow!("channel '{channel_name}': missing outbound config")
            })?;
            Arc::new(EmailOutboundAdapter::new_with_attachments(
                outbound_config,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "feishu" => {
            let feishu_config = channel_config
                .feishu
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing feishu config"))?
                .clone();
            Arc::new(FeishuOutboundAdapter::new_with_attachments(
                feishu_config,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "gitee" => {
            let gitee_config = channel_config
                .gitee
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing gitee config"))?
                .clone();
            Arc::new(GiteeOutboundAdapter::with_footer_enabled(
                gitee_config,
                storage.clone(),
                footer_enabled,
            )?)
        }
        "github" => {
            let github_config = channel_config
                .github
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing github config"))?
                .clone();
            Arc::new(GithubOutboundAdapter::with_footer_enabled(
                github_config,
                storage.clone(),
                footer_enabled,
            )?)
        }
        "wechat" => {
            // WeChat config is validated and cloned in the inbound section.
            // Outbound only needs sender, storage, and footer config.
            let adapter = WechatOutboundAdapter::new_with_attachments(
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            );
            // Store the sender_arc for later use in the inbound section
            *wechat_sender_arc = Some(adapter.sender_arc());
            Arc::new(adapter)
        }
        "wecom_bot" => {
            let adapter = WecomBotOutboundAdapter::new_with_attachments(
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            );
            *wecom_bot_handle_arc = Some(adapter.handle_arc());
            Arc::new(adapter)
        }
        "wecom" => {
            let wecom_config = channel_config
                .wecom
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom config"))?
                .clone();
            Arc::new(WecomOutboundAdapter::new_with_attachments(
                wecom_config.corp_id,
                wecom_config.corp_secret,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "wecomkf" => {
            let wecomkf_config = channel_config
                .wecom_kf
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("channel '{channel_name}': missing wecom_kf config")
                })?
                .clone();

            let access_token_cache = Arc::new(AccessTokenCache::new(
                wecomkf_config.corp_id.clone(),
                wecomkf_config.corp_secret.clone(),
            ));
            let kf_client = Arc::new(KfApiClient::new(access_token_cache));
            *wecomkf_kf_client = Some(kf_client.clone());

            Arc::new(WecomKfOutboundAdapter::new(
                kf_client,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "websocket" => {
            let (broadcast_tx, _) = tokio::sync::broadcast::channel(64);
            let adapter = WebsocketOutboundAdapter::new(broadcast_tx.clone(), storage.clone());
            // Store the inbound adapter for later registration with the inspect server
            let mut handler = WebsocketInboundAdapter::new(channel_name.to_string(), broadcast_tx);
            handler.set_workspace_dir(workspace_dir.clone());
            handler.set_inspect_broadcast(inspect_broadcast.clone());
            let handler = Arc::new(handler);
            ws_handler_for_channel.insert(channel_name.to_string(), handler.clone());
            websocket_handlers.push(handler);
            Arc::new(adapter)
        }
        other => {
            tracing::warn!(
                channel = %channel_name,
                channel_type = %other,
                "Unsupported channel type, skipping"
            );
            return Ok(None);
        }
    };
    Ok(Some(outbound))
}
