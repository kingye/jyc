//! WeChat outbound adapter implementation.
//!
//! This module handles sending messages to WeChat via the OpenILink WebSocket Bridge.
//! Unlike Feishu which uses HTTP API calls, WeChat sends messages through the same
//! WebSocket connection used for receiving messages. The outbound adapter holds a
//! `mpsc::UnboundedSender<String>` to push JSON-formatted messages into the WebSocket.
