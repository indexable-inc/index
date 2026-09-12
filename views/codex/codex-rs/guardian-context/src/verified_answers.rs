//! Stateless selection of original host-verified question/answer records.
//! Omit a record atomically when it cannot fit; never truncate a restriction out of a grant.

use codex_history::RetainedContext;
use codex_history::VerifiedAnswer;

use crate::GuardianRootMessage;
use codex_protocol::protocol::TruncationPolicy;

const MAX_ANSWER_TOKENS: usize = 900;

/// Request-bounded answers and whether they are complete enough for cached fast approval.
pub struct RenderedVerifiedAnswers {
    pub fragments: Vec<String>,
    pub complete: bool,
}

/// Renders each response with its original source roles and the existing approximate per-answer budget.
pub fn render_verified_answers(context: &RetainedContext) -> RenderedVerifiedAnswers {
    let mut complete = context.is_complete();
    let mut fragments = Vec::new();
    for answer in context.verified_answers() {
        if let Some(text) = render_verified_answer(answer) {
            fragments.push(text);
        } else {
            complete = false;
        }
    }
    if !complete {
        fragments.insert(
            /*index*/ 0,
            GuardianRootMessage::IncompleteVerifiedAnswers.render(),
        );
    }
    RenderedVerifiedAnswers {
        fragments,
        complete,
    }
}

/// Selects one complete response, preserving both sides of every question/answer pair.
/// Returning None means the caller must account for missing evidence, not partial permission.
pub fn render_verified_answer(answer: &VerifiedAnswer) -> Option<String> {
    let text = answer
        .questions
        .iter()
        .map(|pair| {
            format!(
                "{}{}",
                GuardianRootMessage::Assistant(pair.question.clone()).render(),
                GuardianRootMessage::User(pair.answer.clone()).render()
            )
        })
        .collect::<String>();
    (!text.is_empty() && text.len() <= TruncationPolicy::Tokens(MAX_ANSWER_TOKENS).byte_budget())
        .then_some(text)
}
