use super::*;
use crate::api::schema::{TaskConclusion, TaskProvenance};

fn state() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.retired_presentation_fixture = true;
    state.set_snapshot(Box::new(super::super::tests::snapshot()));
    state.set_endpoint_methods(Some(
        ["task.list", "task.get", "pane.focus"]
            .map(str::to_owned)
            .to_vec(),
    ));
    state.last_composed_size = Some((180, 40));
    state.inspector.open = true;
    state.inspector.view = InspectorView::Tasks;
    state
}
fn tick(state: &mut ClientShellState) -> ClientShellInput {
    let mut out = ClientShellInput::default();
    state.tick_inspector(&mut out);
    out
}
fn pending(state: &ClientShellState) -> String {
    state
        .pending_requests
        .iter()
        .find(|(_, request)| matches!(request.kind, PendingEndpointKind::Tasks { .. }))
        .unwrap()
        .0
        .clone()
}
fn summary(id: &str) -> TaskSummary {
    TaskSummary {
        task_id: id.into(),
        revision: 1,
        title: format!("Task {id}"),
        status: "blocked".into(),
        reported_verification: "fail".into(),
        workspace_id: None,
        workspace_label: None,
        updated_at_unix_ms: 0,
    }
}
fn source() -> TaskSource {
    TaskSource {
        store_id: "store-a".into(),
        reported: TaskProvenance {
            reporter: Some("synthetic reporter".into()),
            boot_id: Some("boot-1".into()),
            pane_id: Some("pane_1".into()),
            ..Default::default()
        },
    }
}
fn info(id: &str) -> TaskInfo {
    TaskInfo {
        task_id: id.into(),
        revision: 1,
        title: format!("Task {id}"),
        status: "review".into(),
        reported_verification: "pass".into(),
        verification_evidence: Some("reported synthetic check".into()),
        description: Some("saved description".into()),
        workspace_id: None,
        workspace_label: None,
        source: source(),
        created_at_unix_ms: 0,
        updated_at_unix_ms: 0,
        conclusions: vec![TaskConclusion {
            conclusion_id: "c1".into(),
            text: "saved conclusion".into(),
            recorded_at_unix_ms: 2,
            source: source(),
        }],
    }
}
fn list(state: &mut ClientShellState, tasks: Vec<TaskSummary>, next: Option<&str>) {
    let id = pending(state);
    state.handle_endpoint_result(
        "boot-1",
        &id,
        Ok(ResponseResult::TaskList {
            store_id: "store-a".into(),
            tasks,
            next_after: next.map(str::to_owned),
        }),
    );
}
fn detail(state: &mut ClientShellState, task: TaskInfo) {
    let id = pending(state);
    state.handle_endpoint_result(
        "boot-1",
        &id,
        Ok(ResponseResult::TaskRecord {
            store_id: "store-a".into(),
            task,
            replayed: false,
            conclusion_id: None,
        }),
    );
}
fn load(state: &mut ClientShellState) {
    tick(state);
    list(state, vec![summary("a"), summary("b")], None);
    state.select_task("a".into());
    tick(state);
    detail(state, info("a"));
}
fn mouse(state: &mut ClientShellState, kind: MouseEventKind, x: u16, y: u16) -> ClientShellInput {
    let mut out = ClientShellInput::default();
    assert!(state.handle_inspector_mouse(
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE
        },
        &mut out
    ));
    out
}

#[test]
fn tasks_missing_capabilities_disable_only_tasks_and_do_not_probe_on_ticks_or_typing() {
    for methods in [None, Some(vec!["task.list".into()])] {
        let mut state = state();
        state.set_endpoint_methods(methods);
        for _ in 0..20 {
            assert!(tick(&mut state).actions.is_empty());
        }
        assert!(state.pending_requests.is_empty());
        assert!(state.tasks.message.contains("unavailable"));
        let notice = state.tasks.message.clone();
        let out = state.handle_input_bytes(b"x");
        assert!(!out.requests.is_empty());
        assert_eq!(state.tasks.message, notice);
        assert!(state.endpoint_is_online(&state.active_endpoint_id));
    }
}

#[test]
fn tasks_workspace_all_orphans_and_bounded_page_navigation_use_endpoint_reads() {
    let mut state = state();
    let out = tick(&mut state);
    assert!(
        matches!(&out.actions[0], ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::TaskList(p) if p.workspace_id.as_deref() == Some("ws_1") && p.limit == Some(PAGE_SIZE)))
    );
    list(&mut state, vec![summary("orphan")], Some("orphan"));
    let area = state.layout(180, 40).inspector;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        area.right() - 5,
        area.y + 4,
    );
    assert_eq!(state.tasks.after.as_deref(), Some("orphan"));
    list(&mut state, vec![summary("unassigned")], None);
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        area.right() - 12,
        area.y + 4,
    );
    assert!(state.tasks.after.is_none());
    list(&mut state, vec![summary("orphan")], None);
    let out = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        area.x + 16,
        area.y + 2,
    );
    assert!(state.tasks.all);
    assert!(
        matches!(&out.actions[0], ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::TaskList(p) if p.workspace_id.is_none() && p.after.is_none()))
    );
    list(
        &mut state,
        vec![summary("closed-workspace"), summary("unassigned")],
        None,
    );
    assert_eq!(state.tasks.summaries.len(), 2);
}

#[test]
fn tasks_without_cwd_or_live_pane_survive_pane_closure() {
    let mut state = state();
    let mut snapshot = super::super::tests::snapshot();
    snapshot.focused_pane_id = None;
    snapshot.panes.clear();
    state.set_snapshot(Box::new(snapshot.clone()));
    load(&mut state);
    let context = state.tasks.context.clone();
    snapshot.panes.clear();
    state.set_snapshot(Box::new(snapshot));
    assert!(tick(&mut state).actions.is_empty());
    assert_eq!(state.tasks.context, context);
    assert!(state
        .tasks
        .detail_lines
        .iter()
        .any(|line| line == "saved conclusion"));
}

#[test]
fn tasks_selection_is_get_only_and_prepared_detail_keeps_status_verification_and_chronology_separate(
) {
    let mut state = state();
    tick(&mut state);
    list(&mut state, vec![summary("a")], None);
    assert!(state.tasks.list_lines[0].contains("blocked | rpt:fail | Task a"));
    let area = state.layout(180, 40).inspector;
    let out = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        area.x + 2,
        area.y + 5,
    );
    assert!(out.requests.is_empty());
    assert!(
        matches!(&out.actions[0], ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::TaskGet(p) if p.task_id == "a"))
    );
    let mut task = info("a");
    task.conclusions.insert(
        0,
        TaskConclusion {
            conclusion_id: "c2".into(),
            text: "later conclusion".into(),
            recorded_at_unix_ms: 3,
            source: source(),
        },
    );
    detail(&mut state, task);
    let lines = &state.tasks.detail_lines;
    assert!(lines
        .iter()
        .any(|s| s.contains("Status: review | revision: 1")));
    assert!(lines.iter().any(|s| s == "Reported verification: pass"));
    assert!(lines.iter().any(|s| s.contains("Herdr did not run checks")));
    assert!(
        lines.iter().position(|s| s == "saved conclusion")
            < lines.iter().position(|s| s == "later conclusion")
    );
}

#[test]
fn tasks_exact_request_id_read_rejection_and_store_mismatch_do_not_accept_wrong_detail() {
    let mut state = state();
    tick(&mut state);
    let id = pending(&state);
    assert!(
        !state
            .handle_endpoint_result(
                "boot-1",
                "wrong-id",
                Ok(ResponseResult::TaskList {
                    store_id: "bad".into(),
                    tasks: vec![],
                    next_after: None
                })
            )
            .0
    );
    assert!(state.pending_requests.contains_key(&id));
    list(&mut state, vec![summary("a")], None);
    state.select_task("a".into());
    tick(&mut state);
    let id = pending(&state);
    state.handle_endpoint_result(
        "boot-1",
        &id,
        Err(ClientShellEndpointError {
            code: Some("task_unavailable".into()),
            message: "denied\u{1b}".into(),
        }),
    );
    assert!(state.tasks.message.contains("denied\\u{1b}"));
    state.invalidate_tasks();
    tick(&mut state);
    let id = pending(&state);
    state.handle_endpoint_result(
        "boot-1",
        &id,
        Ok(ResponseResult::TaskRecord {
            store_id: "different-store".into(),
            task: info("a"),
            replayed: false,
            conclusion_id: None,
        }),
    );
    assert!(state.tasks.detail.is_none());
    assert!(state.tasks.message.contains("mismatch"));
}

#[test]
fn tasks_stale_selection_scope_boot_connection_and_endpoint_results_are_discarded() {
    for change in 0..6 {
        let mut state = state();
        load(&mut state);
        state.select_task("a".into());
        tick(&mut state);
        let id = pending(&state);
        match change {
            0 => state.select_task("b".into()),
            1 => {
                state.tasks.all = true;
            }
            2 => {
                let mut snap = super::super::tests::snapshot();
                snap.boot_id = "boot-2".into();
                state.set_snapshot(Box::new(snap));
            }
            3 => state.active_snapshot_generation = Some(99),
            4 => {
                state.active_endpoint_id = ClientEndpointId::Ssh(
                    crate::client::endpoint::ProfileId::parse("11111111111111111111111111111111")
                        .unwrap(),
                )
            }
            _ => {
                state.inspector.open = false;
            }
        }
        let (paint, actions) = state.handle_endpoint_result(
            "boot-1",
            &id,
            Ok(ResponseResult::TaskRecord {
                store_id: "store-a".into(),
                task: info("a"),
                replayed: false,
                conclusion_id: None,
            }),
        );
        assert!(!paint, "change {change}");
        assert!(actions.is_empty());
        assert!(state.tasks.detail.is_none());
    }
}

#[test]
fn tasks_known_source_focus_is_explicit_and_missing_changed_or_ambiguous_source_retains_conclusion()
{
    for change in 0..7 {
        let mut state = state();
        load(&mut state);
        let mut source = source();
        match change {
            0 => {}
            1 => source.reported.pane_id = Some("closed".into()),
            2 => source.reported.boot_id = Some("prior-boot".into()),
            3 => source.store_id = "other-store".into(),
            4 => source.reported.agent_session = Some("cannot-prove-session".into()),
            5 => source.reported.agent = Some("different-agent".into()),
            _ => {
                state.active_endpoint_id = ClientEndpointId::Ssh(
                    crate::client::endpoint::ProfileId::parse("22222222222222222222222222222222")
                        .unwrap(),
                )
            }
        }
        let mut out = ClientShellInput::default();
        state.focus_task_source(&source, &mut out);
        if change == 0 {
            assert!(
                matches!(&out.actions[0], ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::PaneFocus(p) if p.pane_id == "pane_1"))
            );
        } else {
            assert!(out.actions.is_empty());
            assert!(state.tasks.message.contains("saved conclusion retained"));
        }
        assert!(state
            .tasks
            .detail_lines
            .iter()
            .any(|line| line == "saved conclusion"));
        assert!(out.requests.is_empty());
    }
}

#[test]
fn tasks_sanitize_all_untrusted_fields_without_changing_raw_source_ids() {
    let mut state = state();
    let malicious = "bad\u{1b}[2J\u{85}\u{202e}\u{7}";
    let mut task = info("a");
    task.title = malicious.into();
    task.status = malicious.into();
    task.reported_verification = malicious.into();
    task.description = Some(malicious.into());
    task.verification_evidence = Some(malicious.into());
    task.source.reported.reporter = Some(malicious.into());
    task.source.reported.agent = Some(malicious.into());
    task.source.reported.agent_session = Some(malicious.into());
    task.source.reported.boot_id = Some(malicious.into());
    task.source.reported.pane_id = Some(malicious.into());
    task.conclusions[0].text = malicious.into();
    task.conclusions[0].conclusion_id = malicious.into();
    state.tasks.detail = Some(task.clone());
    state.tasks.summaries = vec![summary(malicious)];
    state.tasks.prepare();
    for line in state
        .tasks
        .list_lines
        .iter()
        .chain(&state.tasks.detail_lines)
    {
        assert!(!line.chars().any(|ch| ch.is_control() || ch == '\u{202e}'));
    }
    assert_eq!(state.tasks.detail.as_ref().unwrap().source, task.source);
}

#[test]
fn tasks_visible_only_singleflight_refresh_and_unchanged_frames_do_not_rebuild_measurements() {
    let mut state = state();
    load(&mut state);
    let builds = state.tasks.builds;
    for _ in 0..20 {
        assert!(tick(&mut state).actions.is_empty());
        state.compose(180, 40).unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 180, 40));
        state.render_inspector(&mut buffer, state.layout(180, 40).inspector);
    }
    assert_eq!(state.tasks.builds, builds);
    state.tasks.last_refresh = Some(Instant::now() - REFRESH);
    assert_eq!(tick(&mut state).actions.len(), 1);
    state.select_task("b".into());
    for _ in 0..10 {
        assert!(tick(&mut state).actions.is_empty());
    }
    assert_eq!(state.pending_requests.len(), 1);
    list(&mut state, vec![summary("a"), summary("b")], None); // stale selection; next tick coalesces latest get
    assert_eq!(tick(&mut state).actions.len(), 1);
    detail(&mut state, info("b"));
    state.tasks.list_needed = false;
    state.tasks.last_refresh = Some(Instant::now());
    let builds = state.tasks.builds;
    state.tasks.last_refresh = Some(Instant::now() - REFRESH);
    tick(&mut state);
    list(&mut state, vec![summary("a"), summary("b")], None);
    assert_eq!(state.tasks.builds, builds);
    state.inspector.open = false;
    state.tasks.last_refresh = Some(Instant::now() - REFRESH);
    assert!(tick(&mut state).actions.is_empty());
    state.inspector.open = true;
    state.last_composed_size = Some((50, 10));
    assert!(tick(&mut state).actions.is_empty());
}

#[test]
fn tasks_scrollbars_capture_cross_boundary_release_and_responsive_geometry() {
    let mut state = state();
    load(&mut state);
    state.tasks.summaries = (0..70)
        .map(|n| summary(&format!("{n}{}", "long".repeat(30))))
        .collect();
    state.tasks.detail.as_mut().unwrap().description = Some(
        (0..70)
            .map(|n| format!("line {n} {}", "wide".repeat(50)))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    state.tasks.prepare();
    for (cols, rows) in [(180, 40), (180, 10), (121, 30), (94, 30)] {
        state.last_composed_size = Some((cols, rows));
        state.compose(cols, rows).unwrap();
        let area = state.layout(cols, rows).inspector;
        assert!(!area.is_empty());
        let (list, detail) = state.task_regions(area, 0, 0, 0, 0);
        assert!(!list.vertical.is_empty());
        if ClientShellState::inspector_list_area(area).height > 1 {
            assert!(!list.horizontal.is_empty());
        }
        assert!(!detail.vertical.is_empty());
        assert!(!detail.horizontal.is_empty());
        for region in [list, detail] {
            for bar in [region.vertical, region.horizontal] {
                // Shared geometry never consumes the only content row for a bar.
                if bar.is_empty() {
                    continue;
                }
                let out = mouse(
                    &mut state,
                    MouseEventKind::Down(MouseButton::Left),
                    bar.x,
                    bar.y,
                );
                assert!(out.requests.is_empty());
                let out = mouse(
                    &mut state,
                    MouseEventKind::Drag(MouseButton::Left),
                    bar.right() + 2,
                    bar.bottom() + 2,
                );
                assert!(out.requests.is_empty());
                let out = mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 1, 1);
                assert!(out.requests.is_empty());
            }
        }
    }
    state.last_composed_size = Some((180, 40));
    let area = state.layout(180, 40).inspector;
    mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        3,
    );
    state.inspector.open = false;
    assert!(
        mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 3, 3)
            .requests
            .is_empty()
    );
    state.inspector.open = true;
    state.last_composed_size = Some((60, 20));
    assert!(state.layout(60, 20).inspector.is_empty());
}

#[test]
fn tasks_wrong_boot_cannot_consume_pending_id_and_disconnect_reconnect_cancels_old_generation() {
    let mut state = state();
    tick(&mut state);
    let id = pending(&state);
    assert!(
        !state
            .handle_endpoint_result(
                "wrong-boot",
                &id,
                Ok(ResponseResult::TaskList {
                    store_id: "bad".into(),
                    tasks: vec![summary("bad")],
                    next_after: None
                })
            )
            .0
    );
    assert!(state.pending_requests.contains_key(&id));
    state.mark_endpoint_disconnected(&ClientEndpointId::Local);
    assert!(state.pending_requests.is_empty());
    assert!(tick(&mut state).actions.is_empty());
    assert!(
        !state
            .handle_endpoint_result(
                "boot-1",
                &id,
                Ok(ResponseResult::TaskList {
                    store_id: "bad".into(),
                    tasks: vec![summary("bad")],
                    next_after: None
                })
            )
            .0
    );
    state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
    state.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        2,
        Box::new(super::super::tests::snapshot()),
    );
    assert_eq!(tick(&mut state).actions.len(), 1);
    list(&mut state, vec![summary("current")], None);
    assert_eq!(state.tasks.summaries[0].task_id, "current");
}

#[test]
fn tasks_remote_endpoint_with_colliding_pane_ids_cannot_consume_or_focus_local_report() {
    let mut state = state();
    load(&mut state);
    let saved_source = source();
    state.select_task("a".into());
    tick(&mut state);
    let stale_id = pending(&state);
    let profile = SavedSshEndpoint {
        id: crate::client::endpoint::ProfileId::parse("33333333333333333333333333333333").unwrap(),
        label: "synthetic remote".into(),
        target: "synthetic.invalid".into(),
        session: "synthetic".into(),
        enabled: true,
    };
    let remote = ClientEndpointId::Ssh(profile.id.clone());
    state.set_endpoint_catalog(&[profile]);
    state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
    state.set_endpoint_snapshot_for_generation(
        &remote,
        1,
        Box::new(super::super::tests::snapshot()),
    );
    assert!(state.activate_endpoint_projection(&remote));
    state.set_endpoint_methods(Some(
        ["task.list", "task.get", "pane.focus"]
            .map(str::to_owned)
            .to_vec(),
    ));
    assert!(
        !state
            .handle_endpoint_result(
                "boot-1",
                &stale_id,
                Ok(ResponseResult::TaskRecord {
                    store_id: "store-a".into(),
                    task: info("a"),
                    replayed: false,
                    conclusion_id: None
                })
            )
            .0
    );
    let mut out = ClientShellInput::default();
    state.focus_task_source(&saved_source, &mut out);
    assert!(out.actions.is_empty());
    assert!(
        tick(&mut state).actions.is_empty(),
        "endpoint switch clears geometry until composed"
    );
    state.compose(180, 40).unwrap();
    let out = tick(&mut state);
    assert!(
        matches!(&out.actions[0], ClientShellAction::Endpoint { endpoint_id, request, .. } if endpoint_id == &remote && matches!(&request.method, Method::TaskList(_)))
    );
    assert!(state.tasks.detail.is_none());
}

#[test]
fn tasks_source_link_hit_requires_visible_button_and_notice_survives_unrelated_refresh() {
    let mut state = state();
    load(&mut state);
    let mut saved = info("a");
    saved.source.reported.pane_id = Some("closed".into());
    state.tasks.detail = Some(saved);
    state.tasks.prepare();
    let link = *state.tasks.links.keys().min().unwrap();
    let area = state.layout(180, 40).inspector;
    state.inspector.preview_scroll = link;
    let (list_region, detail_region) = state.task_regions(area, 0, 0, link, 0);
    let row = detail_region.content.y + (link - detail_region.top) as u16;
    let out = mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        detail_region.content.x + 1,
        row,
    );
    assert!(out.actions.is_empty());
    assert!(state.tasks.message.contains("saved conclusion retained"));
    state.tasks.last_refresh = Some(Instant::now() - REFRESH);
    tick(&mut state);
    list(&mut state, vec![summary("a"), summary("b")], None);
    assert!(state.tasks.message.contains("saved conclusion retained"));
    state.tasks.message.clear();
    let hidden = InspectorRegion::new(
        Rect::new(detail_region.content.x, detail_region.content.y, 20, 2),
        100,
        100,
        link,
        20,
    );
    let mut out = ClientShellInput::default();
    state.handle_tasks_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hidden.content.x,
            row: hidden.content.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
        area,
        list_region,
        hidden,
        &mut out,
    );
    assert!(out.actions.is_empty());
    assert!(state.tasks.message.is_empty());
}

#[test]
fn tasks_terminal_origin_gesture_keeps_release_ownership() {
    let mut state = state();
    load(&mut state);
    let hit = PaneHit {
        rect: Rect::new(26, 1, 112, 39),
        inner_rect: Rect::new(26, 1, 112, 39),
        scrollbar_rect: None,
        scroll: None,
        pane_id: "pane_1".into(),
        popup: false,
        mouse_reporting: true,
        sgr_pixel_mouse: false,
        pixel_width: 0,
        pixel_height: 0,
    };
    let event = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 30,
        row: 10,
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    state.pane_mouse_gesture = Some(ClientPaneMouseGesture {
        hit,
        button: MouseButton::Left,
        stripped_modifiers: crossterm::event::KeyModifiers::NONE,
        last_event: event,
        last_position: ClientMousePosition::Cell { column: 4, row: 9 },
    });
    let area = state.layout(180, 40).inspector;
    let out = state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: area.x + 1,
        row: 10,
        modifiers: crossterm::event::KeyModifiers::NONE,
    })]);
    assert!(state.pane_mouse_gesture.is_none());
    assert!(out
        .requests
        .iter()
        .any(|request| matches!(request, ClientMessage::ClientShellPaneInput { .. })));
}

#[test]
fn tasks_tab_is_clickable_at_minimum_width_without_focused_pane_cwd() {
    for (cols, rows) in [(94, 30), (121, 30)] {
        let mut state = state();
        let mut snapshot = super::super::tests::snapshot();
        snapshot.panes.clear();
        snapshot.focused_pane_id = None;
        state.set_snapshot(Box::new(snapshot));
        state.last_composed_size = Some((cols, rows));
        state.inspector.view = InspectorView::Files;
        let area = state.layout(cols, rows).inspector;
        let out = mouse(
            &mut state,
            MouseEventKind::Down(MouseButton::Left),
            area.x + if area.width >= 32 { 25 } else { 22 },
            area.y + 1,
        );
        assert!(state.inspector.view == InspectorView::Tasks);
        assert!(
            matches!(&out.actions[0], ClientShellAction::Endpoint { request, .. } if matches!(request.method, Method::TaskList(_)))
        );
        assert!(out.requests.is_empty());
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        state.render_inspector(&mut buffer, area);
        let label = (area.x..area.right())
            .map(|x| buffer[(x, area.y + 1)].symbol())
            .collect::<String>();
        assert!(label.contains("Tasks]"));
        assert!(
            mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 2, 2)
                .requests
                .is_empty()
        );
    }
}

#[test]
fn tasks_shallow_list_retains_content_row_and_supports_horizontal_wheel_without_bar() {
    let mut state = state();
    load(&mut state);
    state.last_composed_size = Some((180, 10));
    state.tasks.summaries = vec![summary(&"long".repeat(40))];
    state.tasks.prepare();
    let area = state.layout(180, 10).inspector;
    let (list, _) = state.task_regions(area, 0, 0, 0, 0);
    assert_eq!(list.content.height, 1);
    assert!(list.horizontal.is_empty());
    let out = mouse(
        &mut state,
        MouseEventKind::ScrollRight,
        list.content.x,
        list.content.y,
    );
    assert!(state.inspector.list_horizontal_scroll > 0);
    assert!(out.requests.is_empty());
}

#[test]
fn tasks_narrow_long_title_keeps_blocked_and_reported_fail_visible_before_title() {
    for cols in [94, 121, 180] {
        let mut state = state();
        tick(&mut state);
        let mut task = summary("a");
        task.title = "very long task title ".repeat(25);
        list(&mut state, vec![task], None);
        state.select_task("a".into());
        state.last_composed_size = Some((cols, 30));
        state.compose(cols, 30).unwrap();
        let area = state.layout(cols, 30).inspector;
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, 30));
        state.render_inspector(&mut buffer, area);
        let row = (area.x + 1..area.right() - 1)
            .map(|x| buffer[(x, area.y + 5)].symbol())
            .collect::<String>();
        assert!(row.contains("blocked"), "{row}");
        assert!(row.contains("rpt:fail"), "{row}");
        let legend = (area.x + 1..area.right() - 1)
            .map(|x| buffer[(x, area.y + 3)].symbol())
            .collect::<String>();
        assert!(legend.contains("rpt: agent claim"), "{legend}");
    }
    let mut state = state();
    let mut future = summary("future");
    future.status = "future_status".into();
    future.reported_verification = "future_verification".into();
    state.tasks.summaries = vec![future];
    state.tasks.prepare();
    assert!(state.tasks.list_lines[0].contains("future_status | rpt:future_verification"));
}

/// Pair with tasks_public_source_context_cli_create_append_focus_and_restart_proof.
/// The external JSON comes only from the isolated integration test's public CLI/API.
#[test]
#[ignore = "requires HERDR_TASK_WORKFLOW_PROOF from the isolated public workflow test"]
fn tasks_public_runtime_source_context_proof_focuses_then_rejects_restart() {
    let proof: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            std::env::var_os("HERDR_TASK_WORKFLOW_PROOF")
                .expect("run public workflow with proof path first"),
        )
        .unwrap(),
    )
    .unwrap();
    let before: crate::api::schema::TaskSourceContextInfo =
        serde_json::from_value(proof["observed"]["result"]["context"].clone()).unwrap();
    let after: crate::api::schema::TaskSourceContextInfo =
        serde_json::from_value(proof["current"]["result"]["context"].clone()).unwrap();
    let task: TaskInfo =
        serde_json::from_value(proof["retained"]["result"]["task"].clone()).unwrap();
    assert_eq!(
        proof["focused"]["result"]["pane"]["pane_id"],
        before.pane_id
    );
    assert_eq!(
        proof["retained"]["result"]["task"],
        proof["appended"]["result"]["task"]
    );
    assert_eq!(before.store_id, after.store_id);
    assert_ne!(before.boot_id, after.boot_id);
    assert_eq!(
        task.conclusions[0].source.reported.boot_id.as_ref(),
        Some(&before.boot_id)
    );
    assert!(task.conclusions[0].source.reported.agent_session.is_none());
    let mut state = state();
    state.tasks.all = true;
    let mut snapshot = (**state.snapshot.as_ref().unwrap()).clone();
    snapshot.boot_id = before.boot_id.clone();
    snapshot.panes[0].pane_id = before.pane_id.clone();
    snapshot.focused_pane_id = Some(before.pane_id.clone());
    state.set_snapshot(Box::new(snapshot.clone()));
    state.set_endpoint_methods(Some(
        ["task.list", "task.get", "pane.focus"]
            .map(str::to_owned)
            .to_vec(),
    ));
    state.compose(180, 40).unwrap();
    tick(&mut state);
    let request_id = pending(&state);
    state.handle_endpoint_result(
        &before.boot_id,
        &request_id,
        Ok(serde_json::from_value(proof["before_list"]["result"].clone()).unwrap()),
    );
    state.select_task(task.task_id.clone());
    tick(&mut state);
    let request_id = pending(&state);
    state.handle_endpoint_result(
        &before.boot_id,
        &request_id,
        Ok(serde_json::from_value(proof["appended"]["result"].clone()).unwrap()),
    );
    assert_eq!(state.tasks.detail.as_ref(), Some(&task));
    let source = task.conclusions[0].source.clone();
    let mut out = ClientShellInput::default();
    state.focus_task_source(&source, &mut out);
    assert!(
        matches!(&out.actions[0], ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::PaneFocus(p) if p.pane_id == before.pane_id))
    );
    assert!(out.requests.is_empty());
    // Deliberately keep a colliding live pane ID: the real changed boot must reject.
    snapshot.boot_id = after.boot_id.clone();
    state.set_snapshot(Box::new(snapshot));
    state.set_endpoint_methods(Some(
        ["task.list", "task.get", "pane.focus"]
            .map(str::to_owned)
            .to_vec(),
    ));
    state.compose(180, 40).unwrap();
    tick(&mut state);
    let request_id = pending(&state);
    state.handle_endpoint_result(
        &after.boot_id,
        &request_id,
        Ok(serde_json::from_value(proof["after_list"]["result"].clone()).unwrap()),
    );
    state.select_task(task.task_id.clone());
    tick(&mut state);
    let request_id = pending(&state);
    state.handle_endpoint_result(
        &after.boot_id,
        &request_id,
        Ok(serde_json::from_value(proof["retained"]["result"].clone()).unwrap()),
    );
    assert_eq!(state.tasks.detail.as_ref(), Some(&task));
    let mut out = ClientShellInput::default();
    state.focus_task_source(&source, &mut out);
    assert!(out.actions.is_empty());
    assert!(out.requests.is_empty());
    assert!(state.tasks.message.contains("saved conclusion retained"));
    assert!(state
        .tasks
        .detail_lines
        .iter()
        .any(|line| line == "Durable public workflow conclusion"));
}

#[test]
fn kanban_shared_controller_singleflight_hidden_guard_and_cached_measurements() {
    let mut state = state();
    tick(&mut state);
    let id = pending(&state);
    state.open_kanban(&mut ClientShellInput::default());
    assert_eq!(pending(&state), id);
    list(&mut state, vec![summary("one")], None);
    let builds = state.tasks.builds;
    for _ in 0..20 {
        assert!(tick(&mut state).actions.is_empty());
    }
    assert_eq!(state.tasks.builds, builds);
    state.close_kanban(&mut ClientShellInput::default());
    assert!(tick(&mut state).actions.is_empty());
    state.inspector.open = false;
    state.tasks.last_refresh = Some(Instant::now() - Duration::from_secs(3));
    assert!(tick(&mut state).actions.is_empty());
    state.open_kanban(&mut ClientShellInput::default());
    assert_eq!(
        state
            .pending_requests
            .values()
            .filter(|r| matches!(r.kind, PendingEndpointKind::Tasks { .. }))
            .count(),
        1
    );
}

#[test]
fn kanban_responsive_unicode_unknown_paint_matches_card_hits_and_page_counts() {
    let mut state = state();
    state.open_kanban(&mut ClientShellInput::default());
    let statuses = [
        "todo",
        "working",
        "blocked",
        "review",
        "done",
        "future-status",
    ];
    let summaries = statuses
        .iter()
        .enumerate()
        .map(|(i, status)| {
            let mut task = summary(&format!("task_{i}"));
            task.status = (*status).into();
            task.title = "漢字 👩‍💻 e\u{301} title\u{1b}\u{202e}".into();
            task
        })
        .collect();
    list(&mut state, summaries, Some("task_5"));
    for (cols, rows) in [(121, 30), (80, 24), (39, 12), (1, 1), (0, 0), (121, 5)] {
        state.last_composed_size = Some((cols, rows));
        let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
        state.render_kanban(&mut buffer);
        if cols == 121 && rows == 30 {
            let text = buffer
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("THIS PAGE"));
            assert!(text.contains("Todo"));
            assert!(!text.contains('\u{202e}'));
            state.handle_mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 2,
                    row: 5,
                    modifiers: crossterm::event::KeyModifiers::empty(),
                },
                &mut ClientShellInput::default(),
            );
            assert_eq!(state.tasks.selected.as_deref(), Some("task_0"));
        }
    }
    state.tasks.board.first = 5;
    let mut buffer = Buffer::empty(Rect::new(0, 0, 121, 30));
    state.render_kanban(&mut buffer);
    let text = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(text.contains("Unknown"));
    assert!(text.contains("future-status"));
}

#[test]
fn kanban_board_origin_release_is_consumed_after_close_resize_and_scope_change() {
    let mut state = state();
    state.open_kanban(&mut ClientShellInput::default());
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let mut outcome = ClientShellInput::default();
        state.handle_mouse(
            MouseEvent {
                kind,
                column: 30,
                row: 8,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
            &mut outcome,
        );
        assert!(outcome.requests.is_empty());
        if matches!(kind, MouseEventKind::Down(_)) {
            state.close_kanban(&mut outcome);
            state.last_composed_size = Some((1, 1));
            state.invalidate_tasks();
        }
    }
    assert!(!state.tasks.board_pointer_capture);
}

#[test]
fn kanban_terminal_origin_release_keeps_pty_sink_and_board_keys_never_type() {
    let mut state = state();
    let event = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 30,
        row: 8,
        modifiers: crossterm::event::KeyModifiers::empty(),
    };
    state.pane_mouse_gesture = Some(ClientPaneMouseGesture {
        hit: PaneHit {
            rect: Rect::new(26, 1, 112, 39),
            inner_rect: Rect::new(26, 1, 112, 39),
            scrollbar_rect: None,
            scroll: None,
            pane_id: "pane_1".into(),
            popup: false,
            mouse_reporting: true,
            sgr_pixel_mouse: false,
            pixel_width: 0,
            pixel_height: 0,
        },
        button: MouseButton::Left,
        stripped_modifiers: crossterm::event::KeyModifiers::empty(),
        last_event: event,
        last_position: ClientMousePosition::Cell { column: 4, row: 7 },
    });
    state.open_kanban(&mut ClientShellInput::default());
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..event
        },
        &mut outcome,
    );
    assert!(state.pane_mouse_gesture.is_none());
    assert!(outcome
        .requests
        .iter()
        .any(|r| matches!(r, ClientMessage::ClientShellPaneInput { .. })));
}

#[test]
fn kanban_terminal_origin_local_selection_finishes_without_clipboard_or_board_capture() {
    let mut state = state();
    state.config.copy_on_select = false;
    state.selection = Some(crate::selection::Selection::absolute_range(
        "pane_1".to_owned(),
        (0, 0),
        (1, 1),
    ));
    state.open_kanban(&mut ClientShellInput::default());
    let mut outcome = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 30,
            row: 8,
            modifiers: crossterm::event::KeyModifiers::empty(),
        },
        &mut outcome,
    );
    assert!(!state.selection.as_ref().unwrap().is_in_progress());
    assert!(!state.tasks.board_pointer_capture);
    assert!(outcome.requests.is_empty());
}

#[test]
fn kanban_page_counts_scope_orphans_and_tiny_controls_remain_honest() {
    let mut state = state();
    state.open_kanban(&mut ClientShellInput::default());
    list(
        &mut state,
        (0..50)
            .map(|i| {
                let mut t = summary(&format!("task_{i}"));
                t.status = "todo".into();
                t
            })
            .collect(),
        Some("task_49"),
    );
    let click = |state: &mut ClientShellState, column, row| {
        let mut out = ClientShellInput::default();
        state.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
            &mut out,
        );
        out
    };
    click(&mut state, 44, 1);
    assert_eq!(state.tasks.previous.len(), 1);
    assert!(state.tasks.summaries.is_empty());
    let mut orphan = summary("orphan");
    orphan.status = "future".into();
    orphan.workspace_id = None;
    list(&mut state, vec![orphan], None);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 121, 30));
    state.render_kanban(&mut buffer);
    let text = buffer
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(text.contains("page 2"));
    assert!(text.contains("1 summaries THIS PAGE"));
    assert!(text.contains("Todo — 0 THIS PAGE"));
    assert!(text.contains("Unknown: 1 THIS PAGE"));
    click(&mut state, 2, 1);
    assert!(state.tasks.all);
    assert!(state.tasks.context.as_ref().unwrap().workspace.is_none());
    let before = state.tasks.all;
    state.last_composed_size = Some((1, 2));
    click(&mut state, 0, 1);
    assert_eq!(state.tasks.all, before); // No invisible truncated-control hit.
}

#[test]
fn kanban_captures_keys_committed_text_and_paste_even_with_popup_terminal() {
    let mut state = state();
    state.popup_terminal_id = Some("synthetic-popup".into());
    state.open_kanban(&mut ClientShellInput::default());
    let outcome = state.handle_raw_events(vec![
        crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::empty(),
        )),
        crate::raw_input::RawInputEvent::Paste("not a command\n".into()),
        crate::raw_input::RawInputEvent::Text(crate::input::TextCommit::new("not typed")),
    ]);
    assert!(outcome.requests.is_empty());
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Kanban)));
}

fn board_mouse(
    state: &mut ClientShellState,
    kind: MouseEventKind,
    x: u16,
    y: u16,
) -> ClientShellInput {
    let mut out = ClientShellInput::default();
    state.handle_mouse(
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
        &mut out,
    );
    out
}
fn board_key(state: &mut ClientShellState, code: crossterm::event::KeyCode, shift: bool) {
    state.handle_raw_events(vec![crate::raw_input::RawInputEvent::Key(
        crate::input::TerminalKey::new(
            code,
            if shift {
                crossterm::event::KeyModifiers::SHIFT
            } else {
                crossterm::event::KeyModifiers::NONE
            },
        ),
    )]);
}
fn board_surface(state: &mut ClientShellState) {
    state.set_pane_surface(PaneSurfaceFrame {
        boot_id: "boot-1".into(),
        projection_revision: 1,
        surface_revision: 1,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(
            &Buffer::with_lines(["synthetic"]),
            None,
            &[],
        ),
        panes: Vec::new(),
        splits: Vec::new(),
        popup: None,
        graphics: crate::protocol::SurfaceGraphicsScene::default(),
    });
}
fn board_raster_inspector(state: &mut ClientShellState) {
    state.set_endpoint_methods(Some(
        [
            "task.list",
            "task.get",
            "pane.focus",
            "file.list",
            "file.preview",
        ]
        .map(str::to_owned)
        .to_vec(),
    ));
    tick(state);
    let complete = |state: &mut ClientShellState, result| {
        let id = state
            .pending_requests
            .iter()
            .find(|(_, request)| matches!(request.kind, PendingEndpointKind::Inspector { .. }))
            .unwrap()
            .0
            .clone();
        state.handle_endpoint_result("boot-1", &id, Ok(result));
    };
    complete(
        state,
        ResponseResult::FileList {
            root: "/repo".into(),
            truncated: false,
            entries: vec![crate::api::schema::FileEntry {
                path: "synthetic.png".into(),
                directory: false,
                symlink: false,
            }],
        },
    );
    let area = state.layout(180, 40).inspector;
    let list = ClientShellState::inspector_list_area(area);
    mouse(
        state,
        MouseEventKind::Down(MouseButton::Left),
        list.x + 2,
        list.y,
    );
    complete(
        state,
        ResponseResult::FilePreview {
            content: crate::api::schema::FilePreviewContent::Image {
                image: crate::api::schema::PreviewRaster {
                    width: 1,
                    height: 1,
                    original_width: 1,
                    original_height: 1,
                    rgb_base64: "AAAA".into(),
                },
            },
        },
    );
    mouse(
        state,
        MouseEventKind::Up(MouseButton::Left),
        list.x + 2,
        list.y,
    );
}
fn board_buffer(state: &ClientShellState, cols: u16, rows: u16) -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(0, 0, cols, rows));
    state.render_kanban(&mut buffer);
    buffer
}
fn buffer_text(buffer: &Buffer) -> String {
    buffer.content.iter().map(|cell| cell.symbol()).collect()
}

#[test]
fn kanban_detail_scroll_survives_normal_tick_compose_without_borrowing_inspector_viewport() {
    use crossterm::event::KeyCode;
    for (view, open, raster) in [
        (InspectorView::Files, true, false),
        (InspectorView::Files, true, true),
        (InspectorView::Git, true, false),
        (InspectorView::Tasks, true, false),
        (InspectorView::Files, false, false),
    ] {
        let mut state = state();
        load(&mut state);
        let mut task = info("a");
        task.conclusions[0].text = (0..90)
            .map(|i| format!("saved conclusion {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.tasks.detail = Some(task);
        state.tasks.prepare();
        state.inspector.view = view;
        state.inspector.open = open;
        if raster {
            board_raster_inspector(&mut state);
        }
        board_surface(&mut state);
        state.compose(180, 40).unwrap();
        // Under Tasks, preserve a valid, nonzero inspector viewport as well.
        if view == InspectorView::Tasks {
            state.inspector.preview_scroll = 4;
            state.inspector.preview_horizontal_scroll = 2;
            state.clamp_inspector_scroll();
        }
        let inspector = (
            state.inspector.preview_scroll,
            state.inspector.preview_horizontal_scroll,
        );
        state.open_kanban(&mut ClientShellInput::default());
        let region = state.board_detail_region(Rect::new(0, 0, 180, 40));
        board_mouse(
            &mut state,
            MouseEventKind::ScrollDown,
            region.content.x,
            region.content.y,
        );
        assert_eq!(state.tasks.board.detail_top, 3);
        let builds = state.tasks.builds;
        for _ in 0..4 {
            tick(&mut state);
            state.compose(180, 40).unwrap();
            assert_eq!(state.tasks.board.detail_top, 3);
            assert_eq!(
                (
                    state.inspector.preview_scroll,
                    state.inspector.preview_horizontal_scroll
                ),
                inspector
            );
        }
        board_key(&mut state, KeyCode::PageDown, false);
        assert!(state.tasks.board.detail_top > 3);
        board_key(&mut state, KeyCode::End, false);
        for _ in 0..4 {
            tick(&mut state);
            let frame = state.compose(180, 40).unwrap().to_ratatui_buffer().unwrap();
            assert!(buffer_text(&frame).contains("saved conclusion 89"));
        }
        assert_eq!(state.tasks.builds, builds);
        assert_eq!(
            (
                state.inspector.preview_scroll,
                state.inspector.preview_horizontal_scroll
            ),
            inspector
        );
        // Selection changes reset only the board's offsets, not its underlying inspector.
        state.select_task("b".into());
        assert_eq!(
            (state.tasks.board.detail_top, state.tasks.board.detail_left),
            (0, 0)
        );
        assert_eq!(
            (
                state.inspector.preview_scroll,
                state.inspector.preview_horizontal_scroll
            ),
            inspector
        );
        state.close_kanban(&mut ClientShellInput::default());
    }
}

#[test]
fn kanban_long_unicode_detail_pan_paint_source_hits_and_shallow_last_row_agree() {
    use crossterm::event::KeyCode;
    let mut state = state();
    load(&mut state);
    state.open_kanban(&mut ClientShellInput::default());
    let mut task = info("a");
    let text = format!("{} FAR-RIGHT-CONCLUSION", "漢字👩‍💻e\u{301}".repeat(70));
    task.conclusions[0].text = text.clone();
    state.tasks.detail = Some(task);
    state.tasks.prepare();
    let builds = state.tasks.builds;
    for (cols, rows) in [(121, 30), (39, 12), (80, 7), (24, 6), (1, 5)] {
        state.compose(cols, rows).unwrap();
        let area = Rect::new(0, 0, cols, rows);
        let line = state
            .tasks
            .detail_lines
            .iter()
            .position(|line| line == &text)
            .unwrap();
        state.tasks.board.detail_top = line;
        board_key(&mut state, KeyCode::End, true);
        tick(&mut state);
        state.compose(cols, rows).unwrap();
        let region = state.board_detail_region(area);
        assert!(region.content.height >= 1); // horizontal bar never eats the last row
        assert_eq!(
            region.left,
            state
                .tasks
                .detail_columns
                .saturating_sub(usize::from(region.content.width))
        );
        let buffer = board_buffer(&state, cols, rows);
        let row = region.content.y + (line - region.top) as u16;
        let expected = crop_cells(&text, region.left, region.content.width);
        let painted = (region.content.x..region.content.right())
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>();
        let mut expected_buffer = Buffer::empty(area);
        render::put_text(
            &mut expected_buffer,
            region.content.x,
            row,
            region.content.width,
            &expected,
            Style::default(),
        );
        for x in region.content.x..region.content.right() {
            assert_eq!(
                buffer[(x, row)].symbol(),
                expected_buffer[(x, row)].symbol(),
                "detail crop at {cols}x{rows}, cell {x}"
            );
        }
        if cols >= 24 {
            assert!(painted.contains("FAR-RIGHT-CONCLUSION"));
        }
        // Last saved source row also remains reachable in shallow and one-cell views.
        board_key(&mut state, KeyCode::End, false);
        let last = state.board_detail_region(area);
        assert_eq!(
            last.top + usize::from(last.content.height),
            state.tasks.detail_lines.len()
        );
    }
    // Display-cell cropping retains grapheme boundaries mid-line, not scalar offsets.
    state.compose(121, 30).unwrap();
    board_key(&mut state, KeyCode::Home, false);
    board_key(&mut state, KeyCode::Home, true);
    let unicode_line = state
        .tasks
        .detail_lines
        .iter()
        .position(|line| line == &text)
        .unwrap();
    state.tasks.board.detail_top = unicode_line;
    state.tasks.board.detail_left = 1;
    tick(&mut state);
    state.compose(121, 30).unwrap();
    let unicode_region = state.board_detail_region(Rect::new(0, 0, 121, 30));
    assert!(
        crop_cells(&text, unicode_region.left, unicode_region.content.width)
            .starts_with(" 字👩‍💻e\u{301}")
    );
    assert_eq!(
        board_buffer(&state, 121, 30)[(
            unicode_region.content.x,
            unicode_region.content.y + (unicode_line - unicode_region.top) as u16
        )]
            .symbol(),
        " "
    );
    board_key(&mut state, KeyCode::Home, true);
    let first = state.tasks.board.first;
    board_key(&mut state, KeyCode::Right, true);
    assert_eq!(state.tasks.board.first, first);
    assert_eq!(state.tasks.board.detail_left, 8);
    let region = state.board_detail_region(Rect::new(0, 0, 121, 30));
    board_mouse(
        &mut state,
        MouseEventKind::ScrollRight,
        region.content.x,
        region.content.y,
    );
    assert_eq!(state.tasks.board.detail_left, 16);
    board_mouse(
        &mut state,
        MouseEventKind::ScrollLeft,
        region.content.x,
        region.content.y,
    );
    assert_eq!(state.tasks.board.detail_left, 8);
    board_key(&mut state, KeyCode::Right, false);
    assert_eq!(state.tasks.board.first, first + 1);
    let link = *state.tasks.links.keys().min().unwrap();
    state.tasks.board.detail_top = link;
    let region = state.board_detail_region(Rect::new(0, 0, 121, 30));
    // A horizontally clipped source control must not focus the wrong coordinates.
    let out = board_mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        region.content.x,
        region.content.y,
    );
    assert!(out.actions.is_empty());
    board_mouse(
        &mut state,
        MouseEventKind::Up(MouseButton::Left),
        region.content.x,
        region.content.y,
    );
    board_key(&mut state, KeyCode::Home, true);
    let region = state.board_detail_region(Rect::new(0, 0, 121, 30));
    assert!(buffer_text(&board_buffer(&state, 121, 30)).contains("[Focus source]"));
    let out = board_mouse(
        &mut state,
        MouseEventKind::Down(MouseButton::Left),
        region.content.x,
        region.content.y,
    );
    assert!(out.actions.iter().any(|action| matches!(
        action,
        ClientShellAction::Endpoint { request, .. } if matches!(&request.method, Method::PaneFocus(_))
    )));
    assert_eq!(state.tasks.builds, builds);
}

#[test]
fn kanban_detail_scrollbar_paint_hit_drag_and_cross_boundary_release_share_region() {
    for horizontal in [false, true] {
        let mut state = state();
        load(&mut state);
        let mut task = info("a");
        task.conclusions[0].text = (0..100)
            .map(|i| format!("{} {i}", "漢字👩‍💻e\u{301}".repeat(70)))
            .collect::<Vec<_>>()
            .join("\n");
        state.tasks.detail = Some(task);
        state.tasks.prepare();
        state.open_kanban(&mut ClientShellInput::default());
        state.compose(121, 30).unwrap();
        let area = Rect::new(0, 0, 121, 30);
        let region = state.board_detail_region(area);
        let painted = if horizontal {
            region.horizontal
        } else {
            region.vertical
        };
        assert!(!painted.is_empty());
        let thumb =
            crate::ui::scrollbar_thumb(region.metrics(horizontal), region.track(horizontal))
                .unwrap();
        let (x, y) = if horizontal {
            (thumb.top, painted.y)
        } else {
            (painted.x, thumb.top)
        };
        let buffer = board_buffer(&state, 121, 30);
        assert_eq!(buffer[(x, y)].symbol(), if horizontal { "━" } else { "▐" });
        board_mouse(&mut state, MouseEventKind::Down(MouseButton::Left), x, y);
        board_mouse(&mut state, MouseEventKind::Drag(MouseButton::Left), 240, 80);
        let end = state.board_detail_region(area);
        let metrics = end.metrics(horizontal);
        assert_eq!(metrics.offset_from_bottom, 0);
        assert!(state.tasks.board_pointer_capture);
        state.close_kanban(&mut ClientShellInput::default());
        state.compose(1, 1).unwrap();
        let out = board_mouse(&mut state, MouseEventKind::Up(MouseButton::Left), 240, 80);
        assert!(out.requests.is_empty());
        assert!(!state.tasks.board_pointer_capture);
    }
}
