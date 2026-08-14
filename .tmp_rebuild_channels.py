# Rebuild channels.rs: previous outbound fn + new spawn_inbound_monitor fn
prev = open('/tmp/channels_prev.rs').read()
idx = prev.rfind('    Ok(Some(outbound))\n}')
assert idx != -1
outbound_part = prev[:idx + len('    Ok(Some(outbound))\n}')]

block = open('/tmp/inbound2.rs').read().rstrip().split('\n')
assert block[0].strip() == 'match channel_type {', block[0]
block_lines = [l[4:] if l.startswith('        ') else l for l in block]

spawn_head = """
/// Spawn the channel-type-specific inbound monitor task(s).
///
/// `state_manager` is consumed (moved into the IMAP monitor closure);
/// `tasks` collects the spawned join handles.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_inbound_monitor(
    channel_type: &str,
    channel_config: &ChannelConfig,
    channel_name: &str,
    workdir: &Path,
    workspace_dir: &Path,
    args: &crate::cli::serve::ServeArgs,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    thread_manager: Arc<ThreadManager>,
    router: Arc<MessageRouter>,
    state_manager: StateManager,
    cancel: CancellationToken,
    cancel_child: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    wechat_sender_arc: &mut Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
    wecom_bot_handle_arc: &mut Option<Arc<Mutex<Option<WecomBotConnectionHandle>>>>,
    wecomkf_kf_client: &mut Option<Arc<KfApiClient>>,
    orchestrator: Arc<ChannelOrchestrator>,
    channel_info: ChannelInfo,
) -> Result<()> {
    let channel_name_owned = channel_name.to_string();
    let tm = thread_manager.clone();
    let channel_span = tracing::info_span!("in", ch = %channel_name);
"""

tail = """
    Ok(())
}
"""
full = outbound_part + spawn_head + '\n'.join(block_lines) + tail
open('crates/jyc-cli/src/cli/serve/channels.rs', 'w').write(full)
print('channels.rs rebuilt; lines:', full.count(chr(10)))
