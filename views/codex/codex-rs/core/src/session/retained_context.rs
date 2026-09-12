//! Records host-verified facts at the same persistence boundary as context checkpoints.

use codex_history::RetainedContextEvent;
use codex_history::RolloutItem;

use super::Session;
use super::thread_settings;

impl Session {
    pub(crate) async fn record_retained_context(&self, mut event: RetainedContextEvent) {
        event.bound();
        // Share the checkpoint persistence lock so a fact cannot land on the wrong side
        // of the checkpoint/suffix boundary. Ephemeral threads use the same live state.
        let _guard = thread_settings::acquire_persistence_lock(self).await;
        if self
            .state
            .lock()
            .await
            .history
            .record_retained_context(&event)
        {
            self.persist_rollout_items(&[RolloutItem::RetainedContext(event)])
                .await;
        }
    }
}
