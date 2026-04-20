use crate::core::monitor_state::ThreadStatus;
use crate::monitor_api::state::ApiState;
use axum::{
    extract::{Path, State},
    response::sse::{Event, Sse},
    Json, 
};
use futures::Stream;
use std::convert::Infallible;

#[derive(serde::Serialize)]
pub struct ChannelInfo {
    pub name: String,
    pub channel_type: String,
    pub connected: bool,
    pub last_activity: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ThreadInfo {
    pub name: String,
    pub channel: String,
    pub status: String,
    pub current_sender: Option<String>,
    pub current_topic: Option<String>,
    pub queue_depth: usize,
    pub last_event: Option<String>,
    pub last_activity: Option<String>,
    pub processing_duration_secs: Option<u64>,
}

#[derive(serde::Serialize)]
pub struct SystemStats {
    pub version: String,
    pub started_at: String,
    pub total_messages: u64,
    pub total_errors: u64,
    pub active_threads: usize,
    pub active_channels: usize,
}

fn thread_status_to_string(status: &ThreadStatus) -> String {
    match status {
        ThreadStatus::Idle => "idle".to_string(),
        ThreadStatus::Processing => "processing".to_string(),
        ThreadStatus::Waiting => "waiting".to_string(),
    }
}

fn datetime_to_string(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339()
}

pub async fn get_channels(
    State(state): State<ApiState>,
) -> Json<Vec<ChannelInfo>> {
    let channels = state.get_channels().await;
    let infos: Vec<ChannelInfo> = channels
        .into_iter()
        .map(|(name, cs)| ChannelInfo {
            name,
            channel_type: cs.channel_type,
            connected: cs.connected,
            last_activity: cs.last_activity.map(datetime_to_string),
        })
        .collect();
    Json(infos)
}

pub async fn get_threads(
    State(state): State<ApiState>,
) -> Json<Vec<ThreadInfo>> {
    let threads = state.get_threads().await;
    let now = chrono::Utc::now();
    let infos: Vec<ThreadInfo> = threads
        .into_iter()
        .map(|(name, ts)| {
            let processing_duration_secs = ts.processing_started_at.map(|started| {
                (now - started).num_seconds() as u64
            });
            ThreadInfo {
                name,
                channel: ts.channel,
                status: thread_status_to_string(&ts.status),
                current_sender: ts.current_sender,
                current_topic: ts.current_topic,
                queue_depth: ts.queue_depth,
                last_event: ts.last_event,
                last_activity: ts.last_activity.map(datetime_to_string),
                processing_duration_secs,
            }
        })
        .collect();
    Json(infos)
}

pub async fn get_thread(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Json<Option<ThreadInfo>> {
    let thread = state.get_thread(&name).await;
    let now = chrono::Utc::now();
    let info = thread.map(|ts| {
        let processing_duration_secs = ts.processing_started_at.map(|started| {
            (now - started).num_seconds() as u64
        });
        ThreadInfo {
            name: name.clone(),
            channel: ts.channel,
            status: thread_status_to_string(&ts.status),
            current_sender: ts.current_sender,
            current_topic: ts.current_topic,
            queue_depth: ts.queue_depth,
            last_event: ts.last_event,
            last_activity: ts.last_activity.map(datetime_to_string),
            processing_duration_secs,
        }
    });
    Json(info)
}

pub async fn get_stats(
    State(state): State<ApiState>,
) -> Json<SystemStats> {
    let system = state.get_stats().await;
    let threads = state.get_threads().await;
    let channels = state.get_channels().await;
    
    let active_threads = threads.iter()
        .filter(|(_, ts)| ts.status != ThreadStatus::Idle)
        .count();
    
    let active_channels = channels.iter()
        .filter(|(_, cs)| cs.connected)
        .count();
    
    Json(SystemStats {
        version: system.version,
        started_at: datetime_to_string(system.started_at),
        total_messages: system.total_messages,
        total_errors: system.total_errors,
        active_threads,
        active_channels,
    })
}

pub async fn get_events(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut receiver = state.subscribe();
    
    let stream = async_stream::stream! {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let json = serde_json::to_string(&event).unwrap_or_default();
                    yield Ok(Event::default().data(json));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "SSE lagged behind, skipping events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };
    
    Sse::new(stream)
}