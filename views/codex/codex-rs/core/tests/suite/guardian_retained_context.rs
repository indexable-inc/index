//! Retained-answer lifecycle through real sessions and the durable thread-store boundary.
//! Typed fixtures enter via the public rollout append API to isolate storage from producer
//! wiring; compaction, resume, rollback and child forks use production paths.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TurnInputRequest;
use codex_core::config::ThreadStoreConfig;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RetainedContextEvent;
use codex_history::RolloutItem;
use codex_history::VerifiedAnswer;
use codex_history::VerifiedQuestionAnswer;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;
use wiremock::MockServer;

async fn record_answer(
    thread: &CodexThread,
    server: &MockServer,
    call_id: &str,
    answer: &str,
) -> Result<VerifiedAnswer> {
    let question = format!("May I publish {call_id}?");
    mount_sse_sequence(
        server,
        vec![
            sse(vec![
                ev_function_call(
                    call_id,
                    "request_user_input",
                    &json!({"questions": [{
                        "id": "publish", "header": "Publish", "question": question,
                        "options": [
                            {"label": "Yes", "description": "Publish privately."},
                            {"label": "No", "description": "Keep local."}
                        ]
                    }]})
                    .to_string(),
                ),
                ev_completed(&format!("ask-{call_id}")),
            ]),
            sse(vec![ev_completed(&format!("answered-{call_id}"))]),
        ],
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: format!("Check authorization for {call_id}."),
            text_elements: Vec::new(),
        }]))
        .await?;
    let request = wait_for_event_match(thread, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;
    thread
        .submit(Op::UserInputAnswer {
            id: request.turn_id.clone(),
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "publish".to_owned(),
                    RequestUserInputAnswer {
                        answers: vec![answer.to_owned()],
                    },
                )]),
            },
        })
        .await?;
    wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let retained = VerifiedAnswer {
        turn_id: request.turn_id,
        call_id: call_id.to_owned(),
        questions: vec![VerifiedQuestionAnswer {
            question,
            answer: answer.to_owned(),
        }],
    };
    thread.ensure_rollout_materialized().await;
    let event =
        RolloutItem::RetainedContext(RetainedContextEvent::VerifiedAnswer(retained.clone()));
    // Repeated delivery must not duplicate evidence during replay.
    thread.append_rollout_items(&[event.clone(), event]).await?;
    Ok(retained)
}

async fn load_context(test: &TestCodex, thread: &CodexThread) -> Result<Vec<RolloutItem>> {
    Ok(test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: thread.session_configured().thread_id,
            include_archived: false,
        })
        .await?
        .items)
}

async fn resume(test: &TestCodex, thread: &CodexThread) -> Result<Arc<CodexThread>> {
    let thread_id = thread.session_configured().thread_id;
    thread.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&thread_id).await;
    let saved = load_context(test, thread).await?;
    let items: Vec<RolloutItem> = serde_json::from_value(serde_json::to_value(saved)?)?;
    Ok(test
        .thread_manager
        .resume_thread_with_history(
            test.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: thread_id,
                history: Arc::new(items),
                rollout_path: None,
            }),
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            ClientMcpExtensions::default(),
        )
        .await?
        .thread)
}

async fn compact_and_assert_answers(
    test: &TestCodex,
    thread: &CodexThread,
    expected: &[VerifiedAnswer],
) -> Result<()> {
    // Inspect live state by persisting a real compaction checkpoint, not a private getter.
    // Repeating this after resume/rollback also catches checkpoint resurrection.
    thread.submit(Op::Compact).await?;
    wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    thread.flush_rollout().await?;
    let history = load_context(test, thread).await?;
    let checkpoint = history
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::Compacted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .context("compaction checkpoint")?;
    let actual = checkpoint
        .retained_context
        .as_ref()
        .context("retained context checkpoint")?;
    assert_eq!(
        actual.verified_answers().cloned().collect::<Vec<_>>(),
        expected
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_answers_rollback_only_the_steered_instruction() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::Legacy)
        .with_config(|config| {
            config.experimental_thread_store = ThreadStoreConfig::Local;
            for feature in [Feature::TokenBudget, Feature::DefaultModeRequestUserInput] {
                config
                    .features
                    .enable(feature)
                    .expect("enable test feature");
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let questions = [
        ("before-steer", "Publish?", "Only privately."),
        ("after-steer", "Publish the README?", "Do not publish it."),
    ];
    let mut responses = questions
        .iter()
        .map(|(call_id, question, _)| {
            sse(vec![
                ev_function_call(
                    call_id,
                    "request_user_input",
                    &json!({"questions": [{
                        "id": "publish", "header": "Publish", "question": question,
                        "options": [
                            {"label": "Yes", "description": "Publish privately."},
                            {"label": "No", "description": "Keep local."}
                        ]
                    }]})
                    .to_string(),
                ),
                ev_completed(call_id),
            ])
        })
        .collect::<Vec<_>>();
    responses.push(sse(vec![ev_completed("done")]));
    let response_mock = mount_sse_sequence(&server, responses).await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Check whether to publish.".to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut answers = Vec::new();
    for (call_id, question, answer) in questions {
        let request = wait_for_event_match(&test.codex, |event| match event {
            EventMsg::RequestUserInput(request) => Some(request.clone()),
            _ => None,
        })
        .await;
        if answers.is_empty() {
            test.codex
                .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "Also inspect the README.".to_owned(),
                    text_elements: Vec::new(),
                }]))
                .await?;
        }
        test.codex
            .submit(Op::UserInputAnswer {
                id: request.turn_id.clone(),
                response: RequestUserInputResponse {
                    answers: HashMap::from([(
                        "publish".to_owned(),
                        RequestUserInputAnswer {
                            answers: vec![answer.to_owned()],
                        },
                    )]),
                },
            })
            .await?;
        answers.push(VerifiedAnswer {
            turn_id: request.turn_id,
            call_id: call_id.to_owned(),
            questions: vec![VerifiedQuestionAnswer {
                question: question.to_owned(),
                answer: answer.to_owned(),
            }],
        });
    }
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(answers[0].turn_id, answers[1].turn_id);
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0].has_message_with_input_texts("user", |texts| {
            texts == ["Check whether to publish."]
        })
    );
    for (index, request) in requests.iter().skip(1).enumerate() {
        assert!(request.has_message_with_input_texts("user", |texts| {
            texts == ["Also inspect the README."]
        }));
        for answer in &answers[..=index] {
            let (output, _) = request
                .function_call_output_content_and_success(&answer.call_id)
                .context("answer tool output")?;
            let output: serde_json::Value =
                serde_json::from_str(&output.context("answer tool output content")?)?;
            assert_eq!(
                output,
                json!({"answers": {"publish": {"answers": [answer.questions[0].answer]}}})
            );
        }
    }
    test.codex
        .append_rollout_items(
            &answers
                .iter()
                .cloned()
                .map(|answer| {
                    RolloutItem::RetainedContext(RetainedContextEvent::VerifiedAnswer(answer))
                })
                .collect::<Vec<_>>(),
        )
        .await?;
    let mut thread = resume(&test, &test.codex).await?;
    compact_and_assert_answers(&test, &thread, &answers).await?;
    for expected in [&answers[..1], &[]] {
        thread.submit(Op::ThreadRollback { num_turns: 1 }).await?;
        wait_for_event(&thread, |event| {
            matches!(event, EventMsg::ThreadRolledBack(_))
        })
        .await;
        compact_and_assert_answers(&test, &thread, expected).await?;
        thread = resume(&test, &thread).await?;
        compact_and_assert_answers(&test, &thread, expected).await?;
    }
    thread.shutdown_and_wait().await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum LifecycleBoundary {
    Rollback,
    ChildFork,
}

#[test_case(ThreadHistoryMode::Legacy, LifecycleBoundary::Rollback; "legacy rollback")]
#[test_case(ThreadHistoryMode::Legacy, LifecycleBoundary::ChildFork; "legacy child fork")]
#[test_case(ThreadHistoryMode::Paginated, LifecycleBoundary::ChildFork; "paginated child fork")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_answers_cross_real_session_boundaries(
    history_mode: ThreadHistoryMode,
    boundary: LifecycleBoundary,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.experimental_thread_store = ThreadStoreConfig::Local;
            for feature in [
                Feature::TokenBudget,
                Feature::DefaultModeRequestUserInput,
                Feature::Collab,
                Feature::MultiAgentV2,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("enable test feature");
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let before = record_answer(
        &test.codex,
        &server,
        "before-compact",
        "Only publish privately.",
    )
    .await?;
    let thread = resume(&test, &test.codex).await?;
    compact_and_assert_answers(&test, &thread, std::slice::from_ref(&before)).await?;

    let after = record_answer(
        &thread,
        &server,
        "after-compact",
        "Do not publish after all.",
    )
    .await?;
    thread.flush_rollout().await?;
    let saved = load_context(&test, &thread).await?;
    let checkpoint_index = saved
        .iter()
        .rposition(|item| matches!(item, RolloutItem::Compacted(_)))
        .context("real compaction persisted a checkpoint")?;
    let RolloutItem::Compacted(checkpoint) = &saved[checkpoint_index] else {
        unreachable!()
    };
    assert_eq!(
        checkpoint
            .retained_context
            .as_ref()
            .context("retained checkpoint")?
            .verified_answers()
            .cloned()
            .collect::<Vec<_>>(),
        vec![before.clone()],
    );
    let suffix = &saved[checkpoint_index + 1..];
    let mut events = suffix
        .iter()
        .filter_map(|item| match item {
            RolloutItem::RetainedContext(event) => Some(event.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    events.dedup();
    assert_eq!(
        events,
        vec![RetainedContextEvent::VerifiedAnswer(after.clone())]
    );
    let thread = resume(&test, &thread).await?;
    let expected = [before.clone(), after];

    match boundary {
        LifecycleBoundary::Rollback => {
            // First remove the suffix answer, then the source retained only in the checkpoint.
            let mut thread = thread;
            for expected in [std::slice::from_ref(&before), &[]] {
                thread.submit(Op::ThreadRollback { num_turns: 1 }).await?;
                wait_for_event(&thread, |event| {
                    matches!(event, EventMsg::ThreadRolledBack(_))
                })
                .await;
                compact_and_assert_answers(&test, &thread, expected).await?;
                thread = resume(&test, &thread).await?;
                compact_and_assert_answers(&test, &thread, expected).await?;
            }
            thread.shutdown_and_wait().await?;
        }
        LifecycleBoundary::ChildFork => {
            // Full-history delegation must not turn parent-local answers into child-local facts.
            let mut created = test.thread_manager.subscribe_thread_created();
            let arguments = json!({
                "task_name": "worker",
                "message": "Inspect without publishing.",
                "fork_turns": "all",
            });
            mount_sse_sequence(
                &server,
                vec![
                    sse(vec![
                        ev_function_call_with_namespace(
                            "spawn",
                            "collaboration",
                            "spawn_agent",
                            &arguments.to_string(),
                        ),
                        ev_completed("spawn-response"),
                    ]),
                    sse(vec![ev_completed("first-fork-completion")]),
                    sse(vec![ev_completed("second-fork-completion")]),
                ],
            )
            .await;
            thread
                .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "Delegate an inspection.".to_owned(),
                    text_elements: Vec::new(),
                }]))
                .await?;
            wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
            let child = test.thread_manager.get_thread(created.try_recv()?).await?;
            wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
            child.flush_rollout().await?;
            let child_history = load_context(&test, &child).await?;
            assert!(
                serde_json::to_string(&child_history)?.contains("Delegate an inspection."),
                "the child must actually inherit parent conversation context"
            );
            compact_and_assert_answers(&test, &child, &[]).await?;
            let child = resume(&test, &child).await?;
            compact_and_assert_answers(&test, &child, &[]).await?;
            child.shutdown_and_wait().await?;
            compact_and_assert_answers(&test, &thread, &expected).await?;

            thread.shutdown_and_wait().await?;
        }
    }
    Ok(())
}
