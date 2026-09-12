//! Bounded host-owned facts outside the model's compaction contract.
//! Checkpoints and live recording use the same admission rules; rendering is a consumer concern.

use std::collections::VecDeque;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

const MAX_ANSWERS: usize = 8;
const MAX_ANSWER_BYTES: usize = 16_384;
const MAX_ANSWERS_BYTES: usize = 65_536;

/// Original assistant question and host-verified user reply, never an inferred permission.
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerifiedQuestionAnswer {
    pub question: String,
    pub answer: String,
}

/// One accepted request_user_input response. Identity is local to the owning thread.
/// An omitted payload records incomplete evidence, rather than keeping a partial grant.
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VerifiedAnswer {
    pub turn_id: String,
    pub call_id: String,
    pub questions: Vec<VerifiedQuestionAnswer>,
}

impl std::fmt::Debug for VerifiedAnswer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedAnswer")
            .field("questions", &self.questions.len())
            .finish_non_exhaustive()
    }
}

/// Sparse, model-invisible updates. Only the host may produce these records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RetainedContextEvent {
    VerifiedAnswer(VerifiedAnswer),
}

/// Bounded snapshot of retained families, persisted with the parent compaction checkpoint.
/// Facts live until their source instruction is rolled back; compaction does not expire them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RetainedContext {
    verified_answers: VecDeque<VerifiedAnswer>,
    /// Lost evidence cannot be treated as a complete authorization history.
    incomplete: bool,
}

impl RetainedContextEvent {
    /// Bounds a persisted event before it enters the rollout or the live snapshot.
    pub fn bound(&mut self) {
        match self {
            Self::VerifiedAnswer(answer) => {
                if serde_json::to_vec(answer).map_or(true, |bytes| bytes.len() > MAX_ANSWER_BYTES) {
                    answer.questions.clear();
                    // IDs are correlation metadata, not model-visible authorization text.
                    answer
                        .turn_id
                        .truncate(answer.turn_id.floor_char_boundary(1_024));
                    answer
                        .call_id
                        .truncate(answer.call_id.floor_char_boundary(1_024));
                }
            }
        }
    }
}

impl RetainedContext {
    pub fn verified_answers(&self) -> impl DoubleEndedIterator<Item = &VerifiedAnswer> {
        self.verified_answers.iter()
    }

    pub fn is_complete(&self) -> bool {
        !self.incomplete
            && self
                .verified_answers
                .iter()
                .all(|answer| !answer.questions.is_empty())
    }

    /// Same-event delivery is idempotent; changed contents replace that source's record.
    pub fn record(&mut self, event: &RetainedContextEvent) -> bool {
        let mut event = event.clone();
        event.bound();
        match event {
            RetainedContextEvent::VerifiedAnswer(answer) => {
                if let Some(index) = self.verified_answers.iter().position(|existing| {
                    existing.turn_id == answer.turn_id && existing.call_id == answer.call_id
                }) {
                    if self.verified_answers[index] == answer {
                        return false;
                    }
                    self.verified_answers.remove(index);
                }
                self.verified_answers.push_back(answer);
                while self.verified_answers.len() > MAX_ANSWERS
                    || serde_json::to_vec(&self.verified_answers)
                        .map_or(true, |bytes| bytes.len() > MAX_ANSWERS_BYTES)
                {
                    self.verified_answers.pop_front();
                    self.incomplete = true;
                }
            }
        }
        true
    }

    /// Restoring a saved thread must not bypass the live storage limits.
    pub fn restore(&mut self, checkpoint: &Self) {
        *self = Self {
            incomplete: checkpoint.incomplete,
            ..Self::default()
        };
        for answer in &checkpoint.verified_answers {
            self.record(&RetainedContextEvent::VerifiedAnswer(answer.clone()));
        }
    }

    /// Retains facts whose sources survive a lifecycle change. Compaction alone is not removal.
    pub fn retain_answers(&mut self, keep: impl FnMut(&VerifiedAnswer) -> bool) {
        self.verified_answers.retain(keep);
    }
}

#[cfg(test)]
#[path = "retained_context_tests.rs"]
mod tests;
