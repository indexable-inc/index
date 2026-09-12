//! Bounded, popup-scoped experimental-feature discovery through the existing RPC.
//! Dropping a popup stops pagination. One reserved request ID bounds abandoned
//! requests until the server replies or the connection closes.

use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ExperimentalFeature;
use codex_app_server_protocol::ExperimentalFeatureListParams;
use codex_app_server_protocol::ExperimentalFeatureListResponse;
use codex_app_server_protocol::RequestId;
use codex_protocol::ThreadId;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::oneshot;

pub(crate) fn fetch(
    request_handle: AppServerRequestHandle,
    thread_id: ThreadId,
    mut response_tx: oneshot::Sender<Result<Vec<ExperimentalFeature>, String>>,
) {
    tokio::spawn(async move {
        let discovery = async {
            let mut features = Vec::new();
            let mut cursor = None;
            let mut cursors = HashSet::new();
            let mut names = HashSet::new();
            for _ in 0..10 {
                let response = request_handle
                    .request_typed::<ExperimentalFeatureListResponse>(
                        ClientRequest::ExperimentalFeatureList {
                            // The client rejects duplicate pending IDs, bounding unanswered
                            // requests across popup cancellation and timeout/retry cycles.
                            request_id: RequestId::String("tui-experimental-features".to_string()),
                            params: ExperimentalFeatureListParams {
                                cursor,
                                limit: Some(100),
                                thread_id: Some(thread_id.to_string()),
                            },
                        },
                    )
                    .await
                    .map_err(|_| "Experimental feature request failed".to_string())?;
                if response.data.len() > 100 {
                    return Err("Experimental feature page exceeds requested limit".to_string());
                }
                features.extend(
                    response
                        .data
                        .into_iter()
                        .filter(|feature| names.insert(feature.name.clone())),
                );
                cursor = response.next_cursor;
                let Some(next) = cursor.as_ref() else {
                    return Ok(features);
                };
                if !cursors.insert(next.clone()) {
                    return Err("Experimental feature pagination repeated a cursor".to_string());
                }
            }
            Err("Experimental feature discovery exceeded 10 pages".to_string())
        };
        tokio::select! {
            _ = response_tx.closed() => {},
            result = tokio::time::timeout(Duration::from_secs(/*secs*/ 5), discovery) => {
                let result = result.unwrap_or_else(|_| Err("Experimental feature discovery timed out".to_string()));
                let _ = response_tx.send(result);
            }
        }
    });
}

#[cfg(test)]
#[path = "experimental_features_tests.rs"]
mod tests;
