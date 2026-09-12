//! Channel notifications: custom MCP notifications a server pushes to the
//! running thread. Two methods carry them, `notifications/claude/channel` (the
//! Claude Code spelling) and `notifications/codex/channel`, both with params
//! `{"content": string, "meta"?: object}`. This module parses them, owns the
//! per-connection-set handler slot, and hands them to the owner's
//! [`McpNotificationHandler`]; core turns them into user input on the session.
//! A notification has no reply channel, so a malformed one or a failing
//! handler surfaces as the error the client handler logs once.

use std::mem;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;

use anyhow::Result;
use anyhow::bail;
use codex_rmcp_client::SendNotification;
use futures::future::BoxFuture;
use rmcp::model::CustomNotification;
use serde_json::Map;
use serde_json::Value;
use tracing::debug;

const CLAUDE_CHANNEL_NOTIFICATION_METHOD: &str = "notifications/claude/channel";
const CODEX_CHANNEL_NOTIFICATION_METHOD: &str = "notifications/codex/channel";

/// A channel notification an MCP server pushed to the thread.
#[derive(Debug, Clone, PartialEq)]
pub struct McpChannelNotification {
    pub server_name: String,
    pub method: String,
    pub content: String,
    /// The `meta` object, when the server sent one.
    pub meta: Option<Map<String, Value>>,
}

/// Owner-side sink for [`McpChannelNotification`]s.
pub trait McpNotificationHandler: Send + Sync {
    fn handle(&self, notification: McpChannelNotification) -> BoxFuture<'static, Result<()>>;
}

pub type McpNotificationHandlerHandle = Arc<dyn McpNotificationHandler>;

/// The handler slot a connection set shares with every sender it hands out,
/// so a reconciliation that installs or replaces the handler reaches the
/// servers that stay connected across it: senders resolve the handler per
/// notification, not at creation.
#[derive(Clone, Default)]
pub(crate) struct NotificationRouter {
    handler: Arc<StdMutex<Option<McpNotificationHandlerHandle>>>,
}

impl NotificationRouter {
    /// Installs (or clears) the handler for every server this router serves.
    pub(crate) fn set_handler(&self, handler: Option<McpNotificationHandlerHandle>) {
        // The slot is an `Option<Arc>`, so a poisoned lock cannot have left it
        // torn. The previous handler is dropped after the guard is released so
        // a panicking destructor never poisons the slot.
        let previous = {
            let mut slot = self.handler.lock().unwrap_or_else(PoisonError::into_inner);
            mem::replace(&mut *slot, handler)
        };
        drop(previous);
    }

    fn handler(&self) -> Option<McpNotificationHandlerHandle> {
        self.handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The custom-notification sink to hand `server_name`'s client.
    pub(crate) fn sender(&self, server_name: String) -> SendNotification {
        let router = self.clone();
        Box::new(move |notification| {
            let router = router.clone();
            let server_name = server_name.clone();
            Box::pin(async move { router.route(&server_name, notification).await })
        })
    }

    /// Routes one custom notification from `server_name` to the installed
    /// handler. Other methods and a missing handler are silent drops; malformed
    /// params and a failing handler are the error the caller logs.
    async fn route(&self, server_name: &str, notification: CustomNotification) -> Result<()> {
        let Some(channel) = parse_channel_notification(server_name, &notification)? else {
            return Ok(());
        };
        let Some(handler) = self.handler() else {
            debug!(
                "MCP channel notification {} from {server_name} dropped: no notification handler",
                channel.method
            );
            return Ok(());
        };
        handler.handle(channel).await
    }
}

/// `Ok(Some)` for a well-formed channel notification, `Ok(None)` for any other
/// method, `Err` for a channel notification whose params are malformed.
fn parse_channel_notification(
    server_name: &str,
    notification: &CustomNotification,
) -> Result<Option<McpChannelNotification>> {
    if !matches!(
        notification.method.as_str(),
        CLAUDE_CHANNEL_NOTIFICATION_METHOD | CODEX_CHANNEL_NOTIFICATION_METHOD
    ) {
        return Ok(None);
    }
    let method = notification.method.as_str();
    let Some(params) = notification.params.as_ref().and_then(Value::as_object) else {
        bail!("MCP channel notification {method} from {server_name} has no object params");
    };
    let Some(content) = params.get("content").and_then(Value::as_str) else {
        bail!("MCP channel notification {method} from {server_name} has no string `content`");
    };
    let meta = match params.get("meta") {
        None => None,
        Some(Value::Object(meta)) => Some(meta.clone()),
        Some(_) => {
            bail!("MCP channel notification {method} from {server_name} has a non-object `meta`")
        }
    };
    Ok(Some(McpChannelNotification {
        server_name: server_name.to_string(),
        method: method.to_string(),
        content: content.to_string(),
        meta,
    }))
}

/// Test doubles shared by the codex-mcp test modules.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::PoisonError;

    use anyhow::Result;
    use futures::future::BoxFuture;

    use super::McpChannelNotification;
    use super::McpNotificationHandler;

    /// Records every notification it is handed.
    #[derive(Default)]
    pub(crate) struct RecordingNotificationHandler {
        notifications: Arc<StdMutex<Vec<McpChannelNotification>>>,
    }

    impl RecordingNotificationHandler {
        pub(crate) fn recorded(&self) -> Vec<McpChannelNotification> {
            self.notifications
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl McpNotificationHandler for RecordingNotificationHandler {
        fn handle(&self, notification: McpChannelNotification) -> BoxFuture<'static, Result<()>> {
            let notifications = Arc::clone(&self.notifications);
            Box::pin(async move {
                notifications
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(notification);
                Ok(())
            })
        }
    }
}
