//! Read-only presentation of the shared Tasks controller's single summary page.
use super::*;
use crossterm::event::{KeyCode, KeyModifiers};

const LABELS: [&str; 6] = ["Todo", "Working", "Blocked", "Review", "Done", "Unknown"];
const STATUSES: [&str; 5] = ["todo", "working", "blocked", "review", "done"];

#[derive(Default)]
pub(super) struct Kanban {
    columns: [Vec<usize>; 6],
    cards: Vec<(String, String)>,
    scroll: [usize; 6],
    pub(super) first: usize,
    // Presentation-only offsets: shared Tasks data must not borrow inspector geometry.
    pub(super) detail_top: usize,
    pub(super) detail_left: usize,
    detail_drag: Option<DetailDrag>,
}
#[derive(Clone, Copy)]
struct DetailDrag {
    horizontal: bool,
    grab_offset: u16,
}
impl Kanban {
    pub(super) fn prepare(&mut self, summaries: &[TaskSummary]) {
        self.detail_drag = None;
        self.columns.iter_mut().for_each(Vec::clear);
        self.cards.clear();
        for (index, task) in summaries.iter().enumerate() {
            let column = STATUSES
                .iter()
                .position(|status| *status == task.status)
                .unwrap_or(5);
            self.columns[column].push(index);
            self.cards.push((
                format!(
                    "{} | rpt:{}",
                    clean(&task.status),
                    clean(&task.reported_verification)
                ),
                clean(&task.title),
            ));
        }
        for (scroll, column) in self.scroll.iter_mut().zip(&self.columns) {
            *scroll = (*scroll).min(column.len().saturating_sub(1));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client::shell) enum TasksSurface {
    Inspector,
    Board,
}

struct BoardGeometry {
    columns: Vec<(usize, Rect)>,
    detail: Rect,
}
fn detail_area(area: Rect, detail: bool) -> Rect {
    let top = area.y.saturating_add(4).min(area.bottom());
    let available = area.bottom().saturating_sub(top);
    let height = if detail && available > 0 {
        (available / 3)
            .max(if available >= 8 { 5 } else { 2 })
            .min(available)
    } else {
        0
    };
    Rect::new(area.x, area.bottom() - height, area.width, height)
}
fn geometry(area: Rect, first: usize, detail: bool) -> BoardGeometry {
    let count = usize::from((area.width / 38).clamp(1, 3)).min(6 - first.min(5));
    let top = area.y.saturating_add(4).min(area.bottom());
    let detail = detail_area(area, detail);
    let board_height = detail.y.saturating_sub(top);
    let mut columns = Vec::new();
    for slot in 0..count {
        let x = area.x + (u32::from(area.width) * slot as u32 / count as u32) as u16;
        let right = area.x + (u32::from(area.width) * (slot + 1) as u32 / count as u32) as u16;
        columns.push((
            first.min(5) + slot,
            Rect::new(x, top, right - x, board_height),
        ));
    }
    BoardGeometry { columns, detail }
}
impl ClientShellState {
    pub(super) fn board_detail_region(&self, area: Rect) -> InspectorRegion {
        let detail = detail_area(area, self.tasks.selected.is_some());
        let header = u16::from(detail.height > 1);
        InspectorRegion::new(
            Rect::new(
                detail.x,
                detail.y + header,
                detail.width,
                detail.height.saturating_sub(header),
            ),
            self.tasks.detail_lines.len(),
            self.tasks.detail_columns,
            self.tasks.board.detail_top,
            self.tasks.board.detail_left,
        )
    }
    pub(in crate::client::shell) fn clamp_kanban_scroll(&mut self) {
        if !self.retired_presentation_enabled() {
            return;
        }
        if !matches!(self.overlay, Some(ClientShellOverlay::Kanban)) {
            return;
        }
        if let Some((cols, rows)) = self.last_composed_size {
            let region = self.board_detail_region(Rect::new(0, 0, cols, rows));
            self.tasks.board.detail_top = region.top;
            self.tasks.board.detail_left = region.left;
            if region.content.is_empty() {
                self.tasks.board.detail_drag = None;
            }
        }
    }
    fn set_board_detail_scroll(&mut self, horizontal: bool, offset: usize) {
        if horizontal {
            self.tasks.board.detail_left = offset;
        } else {
            self.tasks.board.detail_top = offset;
        }
    }

    pub(in crate::client::shell) fn active_tasks_surface(&self) -> Option<TasksSurface> {
        if !self.retired_presentation_enabled() {
            return None;
        }
        if matches!(self.overlay, Some(ClientShellOverlay::Kanban)) {
            Some(TasksSurface::Board)
        } else if self.inspector.view == InspectorView::Tasks && self.inspector_visible() {
            Some(TasksSurface::Inspector)
        } else {
            None
        }
    }
    pub(in crate::client::shell) fn open_kanban(&mut self, outcome: &mut ClientShellInput) {
        if !self.retired_presentation_enabled() {
            return;
        }
        self.cancel_inspector_drag();
        self.chrome_drag = None;
        self.workspace_press = None;
        self.tab_press = None;
        self.overlay = Some(ClientShellOverlay::Kanban);
        self.tick_tasks(outcome);
        outcome.repaint = true;
    }
    pub(in crate::client::shell) fn close_kanban(&mut self, outcome: &mut ClientShellInput) {
        self.overlay = None;
        self.tasks.board.detail_drag = None;
        self.cancel_inspector_drag();
        outcome.repaint = true;
    }
    fn board_page(&mut self, next: bool) {
        if next {
            let Some(after) = self.tasks.next.clone() else {
                return;
            };
            self.tasks.previous.push(self.tasks.after.clone());
            self.tasks.after = Some(after);
        } else {
            if self.tasks.after.is_none() {
                return;
            }
            self.tasks.after = self.tasks.previous.pop().flatten();
        }
        self.tasks.selected = None;
        self.tasks.detail = None;
        self.tasks.detail_needed = false;
        self.tasks.summaries.clear();
        self.tasks.next = None;
        self.tasks.board.scroll = [0; 6];
        self.tasks.board.detail_top = 0;
        self.tasks.board.detail_left = 0;
        self.tasks.board.detail_drag = None;
        self.tasks.prepare();
        self.invalidate_tasks();
    }
    pub(in crate::client::shell) fn handle_kanban_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if !self.retired_presentation_enabled() {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_kanban(outcome),
            KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End
                if key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.tasks.board.detail_left = match key.code {
                    KeyCode::Left => self.tasks.board.detail_left.saturating_sub(8),
                    KeyCode::Right => self.tasks.board.detail_left.saturating_add(8),
                    KeyCode::Home => 0,
                    _ => usize::MAX,
                };
            }
            KeyCode::PageDown | KeyCode::PageUp | KeyCode::Home | KeyCode::End => {
                let page = self.last_composed_size.map_or(1, |(cols, rows)| {
                    usize::from(
                        self.board_detail_region(Rect::new(0, 0, cols, rows))
                            .content
                            .height,
                    )
                    .max(1)
                });
                self.tasks.board.detail_top = match key.code {
                    KeyCode::PageUp => self.tasks.board.detail_top.saturating_sub(page),
                    KeyCode::PageDown => self.tasks.board.detail_top.saturating_add(page),
                    KeyCode::Home => 0,
                    _ => usize::MAX,
                };
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.tasks.board.first = self.tasks.board.first.saturating_sub(1)
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.tasks.board.first = (self.tasks.board.first + 1).min(5)
            }
            KeyCode::Char('a') => {
                self.tasks.all = !self.tasks.all;
                self.invalidate_tasks();
            }
            KeyCode::Char('n') => self.board_page(true),
            KeyCode::Char('p') => self.board_page(false),
            KeyCode::Down | KeyCode::Char('j') => {
                let column = self.tasks.board.first;
                self.tasks.board.scroll[column] = (self.tasks.board.scroll[column] + 1)
                    .min(self.tasks.board.columns[column].len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let column = self.tasks.board.first;
                self.tasks.board.scroll[column] = self.tasks.board.scroll[column].saturating_sub(1);
            }
            KeyCode::Enter => {
                let column = self.tasks.board.first;
                if let Some(index) =
                    self.tasks.board.columns[column].get(self.tasks.board.scroll[column])
                {
                    self.select_task(self.tasks.summaries[*index].task_id.clone());
                }
            }
            _ => {}
        }
        self.tick_tasks(outcome);
        self.clamp_kanban_scroll();
        outcome.repaint = true;
    }
    pub(in crate::client::shell) fn render_kanban(&self, buffer: &mut Buffer) {
        if !self.retired_presentation_enabled() {
            return;
        }
        let area = buffer.area;
        let style = Style::default()
            .fg(self.config.palette.text)
            .bg(self.config.palette.panel_bg);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                buffer[(x, y)].set_symbol(" ").set_style(style);
            }
        }
        let put = |buffer: &mut Buffer, rect: Rect, row: u16, text: &str| {
            if row < rect.height {
                render::put_text(
                    buffer,
                    rect.x,
                    rect.y + row,
                    rect.width,
                    &crop_cells(text, 0, rect.width),
                    style,
                );
            }
        };
        put(
            buffer,
            area,
            0,
            "[q] close  [<] columns [>]   Kanban — read-only reports",
        );
        put(
            buffer,
            area,
            1,
            &format!(
                "{:<29} {:<11} {:<11} page {} | {} summaries THIS PAGE",
                if self.tasks.all {
                    "[a] All (includes orphans)"
                } else {
                    "[a] Workspace"
                },
                if self.tasks.after.is_some() {
                    "[p] prev"
                } else {
                    "no prev"
                },
                if self.tasks.next.is_some() {
                    "[n] next"
                } else {
                    "no next"
                },
                self.tasks.previous.len() + 1,
                self.tasks.summaries.len()
            ),
        );
        put(buffer, area, 2, &format!("Columns {}–{} of 6 | Unknown: {} THIS PAGE | [j/k] scroll, Enter/select | rpt = agent claim", self.tasks.board.first + 1, (self.tasks.board.first + usize::from((area.width / 38).clamp(1,3))).min(6), self.tasks.board.columns[5].len()));
        let current = self.tasks.context == self.task_context();
        put(
            buffer,
            area,
            3,
            if !current {
                "Waiting for current task scope..."
            } else if self.task_context().is_none() {
                "Tasks unavailable: no connected workspace. Choose All or reconnect this endpoint."
            } else if !self.tasks.message.is_empty() {
                &self.tasks.message
            } else {
                "Counts are THIS PAGE only, not totals. Mouse wheel scrolls each column/detail."
            },
        );
        if !current || self.task_context().is_none() {
            return;
        }
        let geometry = geometry(area, self.tasks.board.first, self.tasks.selected.is_some());
        for (column, rect) in geometry.columns {
            put(
                buffer,
                rect,
                0,
                &format!(
                    "{} — {} THIS PAGE",
                    LABELS[column],
                    self.tasks.board.columns[column].len()
                ),
            );
            for (slot, index) in self.tasks.board.columns[column]
                .iter()
                .skip(self.tasks.board.scroll[column])
                .enumerate()
            {
                let row = 1 + slot * 3;
                if row >= usize::from(rect.height) {
                    break;
                }
                let task = &self.tasks.summaries[*index];
                let (metadata, title) = &self.tasks.board.cards[*index];
                put(
                    buffer,
                    rect,
                    row as u16,
                    &format!(
                        "{}{}",
                        if self.tasks.selected.as_ref() == Some(&task.task_id) {
                            "> "
                        } else {
                            "  "
                        },
                        metadata
                    ),
                );
                put(buffer, rect, (row + 1) as u16, title);
            }
        }
        if geometry.detail.height > 1 {
            put(
                buffer,
                geometry.detail,
                0,
                "Saved detail / conclusions — PgUp/Dn, Home/End; Shift+←/→ pan",
            );
        }
        let region = self.board_detail_region(area);
        for (row, line) in self
            .tasks
            .detail_lines
            .iter()
            .skip(region.top)
            .take(usize::from(region.content.height))
            .enumerate()
        {
            render::put_text(
                buffer,
                region.content.x,
                region.content.y + row as u16,
                region.content.width,
                &crop_cells(line, region.left, region.content.width),
                style,
            );
        }
        region.paint(buffer, &self.config.palette);
    }
    pub(in crate::client::shell) fn route_kanban_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !self.retired_presentation_enabled() {
            return false;
        }
        let visible = matches!(self.overlay, Some(ClientShellOverlay::Kanban));
        if !visible && !self.tasks.board_pointer_capture {
            return false;
        }
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.tasks.board_pointer_capture = false;
            self.tasks.board.detail_drag = None;
        }
        if visible {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.tasks.board_pointer_capture = true;
            }
            self.handle_kanban_mouse(mouse, outcome);
        }
        true
    }
    fn handle_kanban_mouse(&mut self, mouse: MouseEvent, outcome: &mut ClientShellInput) {
        let Some((cols, rows)) = self.last_composed_size else {
            return;
        };
        let area = Rect::new(0, 0, cols, rows);
        let point = (mouse.column, mouse.row);
        self.clamp_kanban_scroll();
        let region = self.board_detail_region(area);
        if let (MouseEventKind::Drag(MouseButton::Left), Some(drag)) =
            (mouse.kind, self.tasks.board.detail_drag)
        {
            let track = region.track(drag.horizontal);
            if !track.is_empty() {
                let metrics = region.metrics(drag.horizontal);
                let position = if drag.horizontal {
                    mouse.column
                } else {
                    mouse.row
                };
                let bottom = crate::ui::scrollbar_offset_from_drag_row(
                    metrics,
                    track,
                    position,
                    drag.grab_offset,
                );
                self.set_board_detail_scroll(
                    drag.horizontal,
                    metrics.max_offset_from_bottom.saturating_sub(bottom),
                );
                outcome.repaint = true;
            }
            return;
        }
        if !contains(area, point) {
            return;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if mouse.row == 0 {
                if mouse.column < 9 && area.width >= 9 {
                    self.close_kanban(outcome);
                } else if (11..14).contains(&mouse.column) && area.width >= 14 {
                    self.tasks.board.first = self.tasks.board.first.saturating_sub(1);
                } else if (22..25).contains(&mouse.column) && area.width >= 25 {
                    self.tasks.board.first = (self.tasks.board.first + 1).min(5);
                }
                outcome.repaint = true;
                return;
            }
            if mouse.row == 1 {
                if mouse.column < 29 && area.width >= 13 {
                    self.tasks.all = !self.tasks.all;
                    self.invalidate_tasks();
                } else if (30..38).contains(&mouse.column) && area.width >= 38 {
                    self.board_page(false);
                } else if (42..50).contains(&mouse.column) && area.width >= 50 {
                    self.board_page(true);
                }
                self.tick_tasks(outcome);
                outcome.repaint = true;
                return;
            }
        }
        if self.tasks.context != self.task_context() {
            return;
        }
        let geometry = geometry(area, self.tasks.board.first, self.tasks.selected.is_some());
        if contains(geometry.detail, point) {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                for horizontal in [false, true] {
                    let painted = if horizontal {
                        region.horizontal
                    } else {
                        region.vertical
                    };
                    if painted.is_empty() || !contains(painted, point) {
                        continue;
                    }
                    let position = if horizontal { mouse.column } else { mouse.row };
                    let metrics = region.metrics(horizontal);
                    let track = region.track(horizontal);
                    let grab_offset = if let Some(grab) =
                        crate::ui::scrollbar_thumb_grab_offset(metrics, track, position)
                    {
                        grab
                    } else {
                        let bottom = crate::ui::scrollbar_offset_from_row(metrics, track, position);
                        self.set_board_detail_scroll(
                            horizontal,
                            metrics.max_offset_from_bottom.saturating_sub(bottom),
                        );
                        crate::ui::scrollbar_thumb_grab_offset(
                            crate::pane::ScrollMetrics {
                                offset_from_bottom: bottom,
                                ..metrics
                            },
                            track,
                            position,
                        )
                        .unwrap_or(0)
                    };
                    self.tasks.board.detail_drag = Some(DetailDrag {
                        horizontal,
                        grab_offset,
                    });
                    outcome.repaint = true;
                    return;
                }
            }
            match mouse.kind {
                MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.tasks.board.detail_left = self.tasks.board.detail_left.saturating_add(8);
                }
                MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.tasks.board.detail_left = self.tasks.board.detail_left.saturating_sub(8);
                }
                MouseEventKind::ScrollDown => {
                    self.tasks.board.detail_top = self.tasks.board.detail_top.saturating_add(3);
                }
                MouseEventKind::ScrollUp => {
                    self.tasks.board.detail_top = self.tasks.board.detail_top.saturating_sub(3);
                }
                MouseEventKind::ScrollRight => {
                    self.tasks.board.detail_left = self.tasks.board.detail_left.saturating_add(8);
                }
                MouseEventKind::ScrollLeft => {
                    self.tasks.board.detail_left = self.tasks.board.detail_left.saturating_sub(8);
                }
                MouseEventKind::Down(MouseButton::Left) if contains(region.content, point) => {
                    let row = region.top + usize::from(mouse.row - region.content.y);
                    let column = region.left + usize::from(mouse.column - region.content.x);
                    // Only the completely visible painted source control is clickable.
                    if region.left == 0 && region.content.width >= 14 && column < 14 {
                        if let Some(source) = self.tasks.links.get(&row).cloned() {
                            self.focus_task_source(&source, outcome);
                        }
                    }
                }
                _ => {}
            }
        }
        for (column, rect) in geometry.columns {
            if !contains(rect, point) {
                continue;
            }
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.tasks.board.scroll[column] = (self.tasks.board.scroll[column] + 1)
                        .min(self.tasks.board.columns[column].len().saturating_sub(1))
                }
                MouseEventKind::ScrollUp => {
                    self.tasks.board.scroll[column] =
                        self.tasks.board.scroll[column].saturating_sub(1)
                }
                MouseEventKind::ScrollLeft => {
                    self.tasks.board.first = self.tasks.board.first.saturating_sub(1)
                }
                MouseEventKind::ScrollRight => {
                    self.tasks.board.first = (self.tasks.board.first + 1).min(5)
                }
                MouseEventKind::Down(MouseButton::Left)
                    if mouse.row > rect.y && (mouse.row - rect.y - 1) % 3 < 2 =>
                {
                    let row =
                        self.tasks.board.scroll[column] + usize::from((mouse.row - rect.y - 1) / 3);
                    if let Some(index) = self.tasks.board.columns[column].get(row) {
                        self.select_task(self.tasks.summaries[*index].task_id.clone());
                    }
                }
                _ => {}
            }
        }
        self.tick_tasks(outcome);
        self.clamp_kanban_scroll();
        outcome.repaint = true;
    }
}
