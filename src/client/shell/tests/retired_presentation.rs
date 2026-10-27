use super::*;
use crate::api::schema::{FilePathParams, GitReadParams, Method, TaskListParams, TaskTarget};

fn normal_client() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    assert!(!state.retired_presentation_enabled());
    state.set_snapshot(Box::new(snapshot()));
    state.set_endpoint_methods(Some(
        [
            "file.list",
            "file.read",
            "file.preview",
            "git.status",
            "git.diff",
            "git.log",
            "task.list",
            "task.get",
            "pane.link.resolve",
            "pane.link.activate",
            "pane.file_link.resolve",
            "pane.file_link.activate",
        ]
        .map(str::to_owned)
        .to_vec(),
    ));
    state.set_pane_surface(surface());
    state
}

#[test]
fn withdrawn_presentation_normal_constructor_and_stale_state_never_reserve_render_or_poll() {
    let mut state = normal_client();
    for position in [TabBarPositionConfig::Top, TabBarPositionConfig::Bottom] {
        state.config.tab_bar_position = position;
        for (cols, rows) in [(180, 40), (121, 30), (80, 20), (44, 20)] {
            state.inspector.open = false;
            let baseline = state.layout(cols, rows);
            let size = state.surface_size(cols, rows);
            state.inspector.open = true;
            state.inspector.preferred_width = Some(90);
            state.inspector.view = super::super::inspector::InspectorView::Tasks;
            state.overlay = Some(ClientShellOverlay::Kanban);
            for _ in 0..3 {
                let mut outcome = ClientShellInput::default();
                state.tick_inspector(&mut outcome);
                state.tick_tasks(&mut outcome);
                state.toggle_inspector(&mut outcome);
                state.open_kanban(&mut outcome);
                assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
                assert!(!outcome.resize);
                state.compose(cols, rows).expect("normal client frame");
                let layout = state.layout(cols, rows);
                assert!(layout.inspector.is_empty());
                assert_eq!(layout.pane_surface, baseline.pane_surface);
                assert_eq!(layout.tab_bar, baseline.tab_bar);
                assert_eq!(layout.sidebar, baseline.sidebar);
                assert_eq!(state.surface_size(cols, rows), size);
                assert!(state.overlay.is_none());
                assert!(state.pending_requests.is_empty());
                assert!(state.pending_file_open.is_none());
            }
            let mut buffer = Buffer::with_lines(["terminal must remain unchanged"]);
            let original = buffer.clone();
            let area = buffer.area;
            state.render_inspector(&mut buffer, area);
            state.render_kanban(&mut buffer);
            assert_eq!(buffer, original, "direct stale render routes are inert");
        }
    }
    assert_eq!(state.next_request_id, 1);
}

#[test]
fn withdrawn_presentation_dispatch_boundary_does_not_create_requests_or_captures() {
    let mut state = normal_client();
    let serial = state.next_request_id;
    let path = FilePathParams {
        root: "/fixture".into(),
        path: "file.pdf".into(),
    };
    for method in [
        Method::FileList(path.clone()),
        Method::FileRead(path),
        Method::FilePreview(crate::api::schema::FilePreviewParams {
            root: "/fixture".into(),
            path: "image.png".into(),
            page: 1,
            width: 100,
            height: 100,
        }),
        Method::GitStatus(GitReadParams {
            cwd: "/fixture".into(),
        }),
        Method::GitDiff(crate::api::schema::GitDiffParams {
            cwd: "/fixture".into(),
            path: "file".into(),
            original_path: None,
            staged: false,
        }),
        Method::GitLog(GitReadParams {
            cwd: "/fixture".into(),
        }),
        Method::TaskList(TaskListParams::default()),
        Method::TaskGet(TaskTarget {
            task_id: "task_fixture".into(),
        }),
        Method::PaneFileLinkResolve(link_params()),
        Method::PaneFileLinkActivate(link_params()),
    ] {
        let mut outcome = ClientShellInput::default();
        assert!(!state.push_endpoint_method_with_kind(
            method,
            PendingEndpointKind::Generic,
            &mut outcome
        ));
        assert!(outcome.actions.is_empty() && outcome.requests.is_empty());
        assert!(state.pending_requests.is_empty());
        assert!(state.visible_endpoint_notice.is_none());
    }
    assert_eq!(state.next_request_id, serial);
    assert!(!state.supports_file_link());
    assert!(state.file_link_attempt(link_params()).is_none());
}

fn link_params() -> crate::api::schema::PaneLinkActivateParams {
    crate::api::schema::PaneLinkActivateParams {
        pane_id: "pane_1".into(),
        viewport_row: 0,
        col: 1,
        content_revision: Some(1),
        offset_from_bottom: None,
    }
}

#[test]
fn withdrawn_presentation_menu_key_mouse_and_hover_keep_only_standard_terminal_routes() {
    let mut state = normal_client();
    state.mode = ClientShellMode::Terminal;
    state.compose(180, 40).unwrap();
    let menu = super::super::global_menu::global_menu_items(state.snapshot.as_deref().unwrap());
    assert_eq!(
        menu.iter().map(|(name, _)| name.as_ref()).collect::<Vec<_>>(),
        vec!["settings", "keybinds", "reload config", "detach"]
    );
    for key in [b"a".as_slice(), b"n", b"p", b"q", b"\x1b"] {
        state.overlay = Some(ClientShellOverlay::Kanban);
        let outcome = state.handle_input_bytes(key);
        assert!(!matches!(state.overlay, Some(ClientShellOverlay::Kanban)));
        assert!(outcome
            .actions
            .iter()
            .all(|action| !matches!(action, ClientShellAction::Endpoint { .. })));
    }
    state.compose(180, 40).unwrap();
    let pane = state.hits.panes[0].inner_rect;
    let mut hover = MouseEvent {
        kind: MouseEventKind::Moved,
        column: pane.x + 1,
        row: pane.y,
        modifiers: KeyModifiers::CONTROL,
    };
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(hover)]);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("standard link hover should remain available");
    };
    assert!(matches!(request.method, Method::PaneLinkResolve(_)));
    let id = request.id.clone();
    state.handle_endpoint_result(
        "boot-1",
        &id,
        Ok(crate::api::schema::ResponseResult::PaneLinkResolved {
            regions: Vec::new(),
        }),
    );
    hover.kind = MouseEventKind::Down(MouseButton::Left);
    let outcome = state.handle_raw_events(vec![RawInputEvent::Mouse(hover)]);
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!("standard link activation should remain available");
    };
    assert!(matches!(request.method, Method::PaneLinkActivate(_)));
    let id = request.id.clone();
    let (_, actions) = state.handle_endpoint_result(
        "boot-1",
        &id,
        Ok(crate::api::schema::ResponseResult::PaneLinkActivated {
            handled: false,
            url: Some("https://example.test/fixture".into()),
        }),
    );
    assert!(
        matches!(&actions[..], [ClientShellAction::OpenSafeWebUrl(url)] if url == "https://example.test/fixture")
    );
    assert!(state.pending_requests.is_empty());
    assert!(state.pending_file_open.is_none());
    assert!(state.layout(180, 40).inspector.is_empty());
}

#[test]
fn withdrawn_presentation_preserves_spaces_agents_custom_rows_labels_and_controls() {
    let mut config = Config::default();
    config.ui.sidebar.agents.rows = vec![vec![crate::config::AgentSidebarToken::Workspace]];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    let mut projected = snapshot();
    projected.tabs[0].label = "review patch".into();
    projected.tabs[0].custom_label = true;
    projected.agents.push(ClientShellAgent {
        pane_id: "pane_1".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: Some("named agent".into()),
        display_agent: None,
        agent: Some("pi".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Idle,
        state_change_seq: 0,
        state_labels: Vec::new(),
        tokens: vec![
            ("model".into(), "selected-model".into()),
            ("model_agent".into(), "pi".into()),
        ],
        focused: true,
    });
    state.set_snapshot(Box::new(projected));
    state.set_pane_surface(surface());
    let baseline = state.compose(180, 40).unwrap();
    let layout = state.layout(180, 40);
    let workspace = state.hits.workspaces[0].rect;
    let tab = state.hits.tabs[0].0;
    let agent = state.hits.agents[0].0;
    let sort = state.hits.agent_sort_toggle;
    assert!(!workspace.is_empty() && !tab.is_empty() && !agent.is_empty() && !sort.is_empty());
    let text = baseline
        .cells
        .iter()
        .map(|cell| cell.symbol.as_str())
        .collect::<String>();
    assert!(text.contains("spaces") && text.contains("agents") && text.contains("review patch"));
    let custom = super::super::agent_sidebar::agent_row(
        state.snapshot.as_deref().unwrap(),
        "pane_1",
        &state.config,
        None,
    )
    .unwrap();
    assert!(
        matches!(&custom.rows[0][0].kind, crate::ui::ResolvedTokenKind::Workspace(label) if label == "client-shell")
    );
    state.inspector.open = true;
    state.overlay = Some(ClientShellOverlay::Kanban);
    let after = state.compose(180, 40).unwrap();
    for rect in [layout.sidebar, layout.tab_bar] {
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                let index = usize::from(y) * 180 + usize::from(x);
                assert_eq!(baseline.cells[index], after.cells[index]);
            }
        }
    }
    assert_eq!(state.hits.workspaces[0].rect, workspace);
    assert_eq!(state.hits.tabs[0].0, tab);
    assert_eq!(state.hits.agents[0].0, agent);
    let before_sort = state.config.agent_panel_sort;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: sort.x,
        row: sort.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert_ne!(state.config.agent_panel_sort, before_sort);
}
