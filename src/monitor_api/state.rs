use crate::core::monitor_state::MonitorState;
use std::sync::Arc;

pub type ApiState = Arc<MonitorState>;

pub fn create_api_state(monitor_state: MonitorState) -> ApiState {
    Arc::new(monitor_state)
}
