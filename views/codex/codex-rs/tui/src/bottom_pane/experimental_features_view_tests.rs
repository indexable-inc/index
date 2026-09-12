use super::*;
use pretty_assertions::assert_eq;

fn server_feature(name: &str) -> ExperimentalFeature {
    ExperimentalFeature {
        name: name.to_string(),
        stage: ExperimentalFeatureStage::Beta,
        display_name: Some(format!("Server {name}")),
        description: Some("Description supplied by the server.".to_string()),
        announcement: None,
        enabled: true,
        default_enabled: false,
    }
}

#[test]
fn experimental_features_discovery_preserves_server_order_and_limits_writes() {
    let (app_tx, mut app_rx) = tokio::sync::mpsc::unbounded_channel();
    let (catalog_tx, catalog_rx) = oneshot::channel();
    let mut view = ExperimentalFeaturesView::new(
        Vec::new(),
        Some(catalog_rx),
        AppEventSender::new(app_tx),
        crate::keymap::RuntimeKeymap::defaults().list,
    );
    view.toggle_selected();
    let mut stable = server_feature("stable_feature");
    stable.stage = ExperimentalFeatureStage::Stable;
    catalog_tx
        .send(Ok(vec![
            server_feature("network_proxy"),
            server_feature("future_feature"),
            stable,
            server_feature("prevent_idle_sleep"),
        ]))
        .unwrap();
    assert!(view.pre_draw_tick(Instant::now()));
    assert_eq!(
        view.features
            .iter()
            .map(|item| (item.feature, item.enabled))
            .collect::<Vec<_>>(),
        vec![
            (Some(Feature::NetworkProxy), true),
            (None, true),
            (Some(Feature::PreventIdleSleep), true),
        ]
    );
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        /*width*/ 100,
        view.desired_height(/*width*/ 100),
    );
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    insta::assert_snapshot!(
        "experimental_features_server_discovery",
        buffer_text(&buffer)
    );
    // Read-only entries retain their configured checkmark when Space is pressed.
    view.state.selected_idx = Some(1);
    view.toggle_selected();
    assert!(view.features[1].enabled);
    view.state.selected_idx = Some(0);
    view.toggle_selected();
    view.state.selected_idx = Some(2);
    view.toggle_selected();
    view.on_ctrl_c();
    let AppEvent::UpdateFeatureFlags { updates } = app_rx.try_recv().unwrap() else {
        panic!("expected feature edits");
    };
    assert_eq!(
        updates,
        vec![
            (Feature::NetworkProxy, false),
            (Feature::PreventIdleSleep, false)
        ]
    );
}

#[test]
fn experimental_features_empty_error_and_cancel_remain_usable() {
    let mut changed_default = server_feature("network_proxy");
    changed_default.default_enabled = true;
    for (snapshot, result) in [
        ("experimental_features_empty", Ok(Vec::new())),
        (
            "experimental_features_unavailable",
            Err("method not found".to_string()),
        ),
        (
            "experimental_features_changed_default",
            Ok(vec![changed_default]),
        ),
    ] {
        let (app_tx, mut app_rx) = tokio::sync::mpsc::unbounded_channel();
        let (catalog_tx, catalog_rx) = oneshot::channel();
        let mut view = ExperimentalFeaturesView::new(
            Vec::new(),
            Some(catalog_rx),
            AppEventSender::new(app_tx),
            crate::keymap::RuntimeKeymap::defaults().list,
        );
        assert!(view.next_frame_delay().is_some());
        catalog_tx.send(result).unwrap();
        assert!(view.pre_draw_tick(Instant::now()));
        assert_eq!(view.next_frame_delay(), None);
        let area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            /*width*/ 80,
            view.desired_height(/*width*/ 80),
        );
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        insta::assert_snapshot!(snapshot, buffer_text(&buffer));
        view.toggle_selected();
        view.on_ctrl_c();
        assert!(view.is_complete());
        assert!(app_rx.try_recv().is_err());
    }
    let (app_tx, _app_rx) = tokio::sync::mpsc::unbounded_channel();
    let (catalog_tx, catalog_rx) = oneshot::channel();
    let view = ExperimentalFeaturesView::new(
        Vec::new(),
        Some(catalog_rx),
        AppEventSender::new(app_tx),
        crate::keymap::RuntimeKeymap::defaults().list,
    );
    drop(view);
    assert!(catalog_tx.send(Ok(Vec::new())).is_err());
}

fn buffer_text(buffer: &Buffer) -> String {
    buffer
        .content
        .chunks(usize::from(buffer.area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
