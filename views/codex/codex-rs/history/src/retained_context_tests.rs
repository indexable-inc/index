use super::*;
use pretty_assertions::assert_eq;

#[test]
fn answer_lifecycle_is_bounded_idempotent_and_checkpointed() {
    let mut context = RetainedContext::default();
    let first = RetainedContextEvent::VerifiedAnswer(VerifiedAnswer {
        turn_id: "turn-1".to_owned(),
        call_id: "ask-1".to_owned(),
        questions: vec![VerifiedQuestionAnswer {
            question: "Publish?".to_owned(),
            answer: "Yes, but never publicly.".to_owned(),
        }],
    });
    assert!(context.record(&first));
    let snapshot = context.clone();
    assert!(!context.record(&first));
    assert_eq!(context, snapshot);
    let checkpoint =
        serde_json::from_str(&serde_json::to_string(&context).expect("retained answer fixture"))
            .expect("retained answer fixture");
    let mut restored = RetainedContext::default();
    restored.restore(&checkpoint);
    assert_eq!(restored, snapshot);

    for index in 2..=10 {
        context.record(&RetainedContextEvent::VerifiedAnswer(VerifiedAnswer {
            turn_id: "turn-2".to_owned(),
            call_id: format!("ask-{index}"),
            questions: vec![VerifiedQuestionAnswer {
                question: "Continue?".to_owned(),
                answer: "Yes".to_owned(),
            }],
        }));
    }
    assert!(!context.is_complete());
    assert_eq!(context.verified_answers().count(), MAX_ANSWERS);
    context.retain_answers(|answer| answer.turn_id != "turn-2");
    assert_eq!(context.verified_answers().count(), 0);
    assert!(!context.is_complete());

    let mut oversized = first.clone();
    let RetainedContextEvent::VerifiedAnswer(answer) = &mut oversized;
    answer.questions[0].answer = "a".repeat(MAX_ANSWER_BYTES);
    restored.record(&oversized);
    assert!(!restored.is_complete());
    assert!(
        restored
            .verified_answers()
            .next()
            .expect("retained answer fixture")
            .questions
            .is_empty()
    );
    assert_eq!(
        snapshot
            .verified_answers()
            .next()
            .expect("retained answer fixture")
            .questions[0]
            .answer,
        "Yes, but never publicly."
    );
}
