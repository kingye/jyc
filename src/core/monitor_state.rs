use super::jyc_event::JycEvent;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tracing::info;

#[derive(Debug, Clone, PartialEq)]
pub enum ThreadStatus {
    Idle,
    Processing,
    Waiting,
}

#[derive(Debug, Clone)]
pub struct ChannelState {
    pub channel_type: String,
    pub connected: bool,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ThreadState {
    pub channel: String,
    pub status: ThreadStatus,
    pub current_sender: Option<String>,
    pub current_topic: Option<String>,
    pub processing_started_at: Option<DateTime<Utc>>,
    pub queue_depth: usize,
    pub last_event: Option<String>,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct SystemState {
    pub started_at: DateTime<Utc>,
    pub version: String,
    pub total_messages: u64,
    pub total_errors: u64,
}

#[derive(Debug)]
pub struct MonitorStateInner {
    pub channels: HashMap<String, ChannelState>,
    pub threads: HashMap<String, ThreadState>,
    pub system: SystemState,
}

pub struct MonitorState {
    inner: Arc<RwLock<MonitorStateInner>>,
    event_sender: broadcast::Sender<JycEvent>,
}

impl MonitorState {
    pub fn new(version: String) -> Self {
        let (event_sender, _) = broadcast::channel(1000);
        let system = SystemState {
            started_at: Utc::now(),
            version,
            total_messages: 0,
            total_errors: 0,
        };
        let inner = MonitorStateInner {
            channels: HashMap::new(),
            threads: HashMap::new(),
            system,
        };
        Self {
            inner: Arc::new(RwLock::new(inner)),
            event_sender,
        }
    }

    pub async fn update_channel_connected(&self, channel: String, channel_type: String, connected: bool) {
        let mut inner = self.inner.write().await;
        inner.channels.insert(
            channel.clone(),
            ChannelState {
                channel_type,
                connected,
                last_activity: Some(Utc::now()),
            },
        );
    }

    pub async fn update_thread_created(&self, thread: String, channel: String) {
        let mut inner = self.inner.write().await;
        inner.threads.insert(
            thread.clone(),
            ThreadState {
                channel,
                status: ThreadStatus::Idle,
                current_sender: None,
                current_topic: None,
                processing_started_at: None,
                queue_depth: 1,
                last_event: Some("ThreadCreated".to_string()),
                last_activity: Some(Utc::now()),
            },
        );
    }

    pub async fn update_thread_closed(&self, thread: &str) {
        let mut inner = self.inner.write().await;
        inner.threads.remove(thread);
    }

    pub async fn update_processing_started(
        &self,
        thread: String,
        message_id: String,
        sender: String,
        topic: Option<String>,
    ) {
        let mut inner = self.inner.write().await;
        inner.system.total_messages += 1;
        if let Some(ts) = inner.threads.get_mut(&thread) {
            ts.status = ThreadStatus::Processing;
            ts.current_sender = Some(sender);
            ts.current_topic = topic;
            ts.processing_started_at = Some(Utc::now());
            ts.last_event = Some(format!("ProcessingStarted({})", message_id));
            ts.last_activity = Some(Utc::now());
        }
    }

    pub async fn update_processing_progress(&self, thread: &str, elapsed_secs: u64, activity: String, progress: Option<f32>) {
        let mut inner = self.inner.write().await;
        if let Some(ts) = inner.threads.get_mut(thread) {
            ts.last_event = Some(format!("Progress: {} ({:?})", activity, progress));
            ts.last_activity = Some(Utc::now());
        }
    }

    pub async fn update_processing_completed(&self, thread: &str, success: bool) {
        let mut inner = self.inner.write().await;
        if !success {
            inner.system.total_errors += 1;
        }
        if let Some(ts) = inner.threads.get_mut(thread) {
            ts.status = ThreadStatus::Idle;
            ts.processing_started_at = None;
            ts.last_event = Some(if success { "ProcessingCompleted" } else { "ProcessingFailed" }.to_string());
            ts.last_activity = Some(Utc::now());
        }
    }

    pub async fn update_queue_depth(&self, thread: &str, depth: usize) {
        let mut inner = self.inner.write().await;
        if let Some(ts) = inner.threads.get_mut(thread) {
            ts.queue_depth = depth;
            ts.last_activity = Some(Utc::now());
        }
    }

    pub async fn get_channels(&self) -> Vec<(String, ChannelState)> {
        let inner = self.inner.read().await;
        inner.channels.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub async fn get_threads(&self) -> Vec<(String, ThreadState)> {
        let inner = self.inner.read().await;
        inner.threads.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub async fn get_thread(&self, name: &str) -> Option<ThreadState> {
        let inner = self.inner.read().await;
        inner.threads.get(name).cloned()
    }

    pub async fn get_stats(&self) -> SystemState {
        let inner = self.inner.read().await;
        inner.system.clone()
    }

    pub fn publish_event(&self, event: JycEvent) {
        let _ = self.event_sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JycEvent> {
        self.event_sender.subscribe()
    }

    pub async fn publish_system_started(&self, channels: Vec<String>) {
        let version = {
            let inner = self.inner.read().await;
            inner.system.version.clone()
        };
        let event = JycEvent::SystemStarted {
            version,
            channels,
        };
        self.publish_event(event);
        info!("Published SystemStarted event");
    }

    pub fn publish_system_stopping(&self) {
        self.publish_event(JycEvent::SystemStopping);
    }

    pub fn publish_channel_connected(&self, channel: String, channel_type: String) {
        self.publish_event(JycEvent::ChannelConnected { channel, channel_type });
    }

    pub fn publish_channel_disconnected(&self, channel: String, reason: String) {
        self.publish_event(JycEvent::ChannelDisconnected { channel, reason });
    }

    pub fn publish_message_received(&self, channel: String, thread: String, sender: String, topic: Option<String>) {
        self.publish_event(JycEvent::MessageReceived { channel, thread, sender, topic });
    }

    pub fn publish_thread_created(&self, thread: String, channel: String) {
        self.publish_event(JycEvent::ThreadCreated { thread, channel });
    }

    pub fn publish_thread_closed(&self, thread: String, channel: String) {
        self.publish_event(JycEvent::ThreadClosed { thread, channel });
    }

    pub fn publish_processing_started(&self, thread: String, message_id: String, sender: String, topic: Option<String>) {
        self.publish_event(JycEvent::ProcessingStarted { thread, message_id, sender, topic });
    }

    pub fn publish_processing_progress(&self, thread: String, elapsed_secs: u64, activity: String, progress: Option<f32>) {
        self.publish_event(JycEvent::ProcessingProgress { thread, elapsed_secs, activity, progress });
    }

    pub fn publish_processing_completed(&self, thread: String, success: bool, duration_secs: u64) {
        self.publish_event(JycEvent::ProcessingCompleted { thread, success, duration_secs });
    }

    pub fn publish_reply_sent(&self, thread: String, channel: String, via: String) {
        self.publish_event(JycEvent::ReplySent { thread, channel, via });
    }

    pub fn publish_reply_failed(&self, thread: String, error: String) {
        self.publish_event(JycEvent::ReplyFailed { thread, error });
    }
}

impl Clone for MonitorState {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            event_sender: self.event_sender.clone(),
        }
    }
}