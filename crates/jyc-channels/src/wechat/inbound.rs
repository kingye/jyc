//! WeChat inbound adapter and matcher implementation.
//!
//! This module handles receiving messages from WeChat via the OpenILink WebSocket Bridge
//! and provides channel-specific pattern matching and thread name derivation.
//!
//! Unlike Feishu which supports multiple chats/threads, WeChat in this implementation
//! uses one bot = one fixed thread. The thread name is derived directly from the channel
//! configuration name (e.g., "wechat_bot").
